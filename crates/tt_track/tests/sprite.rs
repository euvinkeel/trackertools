//! End to end on the sprite fixture (`cargo xtask fixtures`): a guide that is
//! only roughly right (a rough pass), a tracker run by background jobs forward
//! and backward from its anchor through the long-GOP source, measured against
//! the analytic truth. Skipped when the fixture hasn't been generated.
//!
//! The fixture is looked for in `TT_FIXTURES`, else in the nearest `fixtures/`
//! above this crate (so a git worktree inside the main checkout finds it).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bevy_ecs::name::Name;
use bevy_ecs::prelude::*;
use tt_core::history::{History, edit, redo, undo};

use tt_core::op::{Dirty, Inputs, Invalidations, OpError, Operator, Output};
use tt_core::signal::{FrameState, SignalStore};
use tt_core::sketch::{BOX_CHANNELS, Capture, ClockMap, STREAM_CHANNELS, SketchParams, Stroke};
use tt_core::transport::Transport;
use tt_core::view::SourceSize;
use tt_core::{AppBuilder, Core, CoreModules};
use tt_media::{DecodeOptions, VideoIndex};
use tt_track::runner::{Footage, MAX_JOBS, TrackBook, TrackJobs, TrackStatus, coverage, settled};
use tt_track::{Direction, TrackModule, Tracker, add_tracker};

const NAME: &str = "sprite_1080p60.mp4";
const FRAMES: i64 = 1200;

fn fixture() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("TT_FIXTURES") {
        return Some(PathBuf::from(dir).join(NAME)).filter(|p| p.exists());
    }
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().map(|d| d.join("fixtures").join(NAME)).find(|p| p.exists())
}

/// The sprite's corner formula at (continuous) frame f, source px.
fn path(f: f64) -> [f64; 2] {
    let t = f / 60.0;
    [950.0 + 500.0 * (0.9 * t).sin() + 60.0 * (5.3 * t).sin(), 530.0 + 300.0 * (1.3 * t + 0.7).sin() + 40.0 * (4.1 * t).sin()]
}

/// The sprite's centre at frame f (source px). ffmpeg's overlay on yuv420
/// places it on even pixels (chroma alignment), so its corner is the formula
/// rounded down to even.
fn truth(f: i64) -> [f64; 2] {
    let even = |v: f64| 2.0 * (v.floor() / 2.0).floor();
    path(f as f64).map(|v| even(v) + 10.5)
}

/// A rough pass at (continuous) frame f: the sprite plus a wander of up to ~6 px.
fn rough_point(f: f64, shift: f64) -> [f64; 2] {
    let t = f / 60.0;
    let c = path(f).map(|v| v + 10.5);
    [c[0] + 4.0 * (2.3 * t).sin() + 2.0 * (6.1 * t).cos() + shift, c[1] + 3.5 * (1.9 * t + 0.5).sin() - shift]
}

/// The rough pass at frame f in a 56 px box.
fn rough(f: i64, shift: f64) -> [f32; 6] {
    let t = f as f64 / 60.0;
    let c = truth(f);
    let (x, y) = (c[0] + 4.0 * (2.3 * t).sin() + 2.0 * (6.1 * t).cos() + shift, c[1] + 3.5 * (1.9 * t + 0.5).sin() - shift);
    [x, y, x - 28.0, y - 28.0, x + 28.0, y + 28.0].map(|v| v as f32)
}

/// A world with the fixture open (no trackers yet).
fn setup_core() -> Option<Core> {
    let Some(fixture) = fixture() else {
        eprintln!("skipped: {NAME} not found (cargo xtask fixtures, or set TT_FIXTURES)");
        return None;
    };
    let mut app = AppBuilder::new();
    app.add_module(CoreModules).add_module(TrackModule);
    let mut core = app.build();
    let index = Arc::new(VideoIndex::open(&fixture).expect("fixture opens"));
    let w = &mut core.world;
    {
        let mut t = w.resource_mut::<Transport>();
        t.fps = index.fps;
        t.frame_count = index.frame_count();
    }
    w.insert_resource(SourceSize { width: index.width as f64, height: index.height as f64 });
    w.insert_resource(Footage { original: index, proxy: None, decode: DecodeOptions::default() });
    Some(core)
}

/// The fixture, and a bare guide: a box signal on an entity (no operator).
fn setup() -> Option<(Core, Entity)> {
    let mut core = setup_core()?;
    let w = &mut core.world;
    let sig = w.resource_mut::<SignalStore>().create(BOX_CHANNELS);
    {
        let mut store = w.resource_mut::<SignalStore>();
        let s = store.get_mut(sig).expect("created");
        for f in 0..FRAMES {
            s.set(f, &rough(f, 0.0));
        }
    }
    let guide = w.spawn((Name::new("Guide"), Output(sig))).id();
    Some((core, guide))
}

/// A stroke as the sketch tool records it: the hand at `hand(frame)` while
/// the video plays `frames` at full speed, no lag.
fn stroke(tx: &mut tt_core::history::Tx<'_>, frames: std::ops::Range<i64>, falloff: f32, hand: impl Fn(f64) -> [f64; 2]) -> Entity {
    let secs = (frames.end - frames.start) as f64 / 60.0;
    let samples: Vec<f32> = (0..=(secs * 1000.0) as usize)
        .flat_map(|i| {
            let t = i as f64 / 1000.0;
            // The frame on screen at t shows its centre at frames.start + 60 t − 0.5.
            let p = hand(frames.start as f64 + 60.0 * t - 0.5);
            [t as f32, p[0] as f32, p[1] as f32]
        })
        .collect();
    let mut clock = ClockMap::default();
    for i in 0..=(secs * 100.0) as usize {
        let t = i as f64 / 100.0;
        clock.push(t, frames.start as f64 + 60.0 * t, true);
    }
    let stream = tx.create_signal(STREAM_CHANNELS);
    tx.signal(stream).write(0, &samples);
    let n = samples.len() / STREAM_CHANNELS;
    tx.spawn((Name::new("Stroke"), Capture { rate: 1.0, samples: n as u32 }, clock, Stroke { falloff, lag: 0.0, ..Stroke::default() }, Output(stream)))
}

/// A real `sketch` operator following the rough pass over the whole clip:
/// one stroke, and a pipeline that keeps the hand's path in a 56 px box.
fn sketch_guide(core: &mut Core) -> Entity {
    let params = SketchParams {
        lag: 0.0,
        steadiness: 50.0,
        responsiveness: 0.0,
        dead_zone: 0.0,
        gain: 0.0,
        pad: 28.0,
        min_half: 28.0,
        before: 0.0,
        after: 0.0,
        smooth_position: 0.0,
        smooth_size: 0.0,
        ..SketchParams::default()
    };
    let mut sketch = None;
    edit(&mut core.world, "New sketch", |tx| {
        let s = stroke(tx, 0..FRAMES, 0.2, |f| rough_point(f, 0.0));
        let out = tx.create_signal(BOX_CHANNELS);
        sketch = Some(tx.spawn((Name::new("Sketch"), Operator { kind: "sketch".into() }, Inputs(vec![("stroke".into(), s)]), Output(out), params)));
    });
    for _ in 0..3 {
        core.run_pre_ui();
    }
    sketch.expect("sketch")
}

/// Run app frames until the tracker is settled (or `until` says stop).
fn run(core: &mut Core, op: Entity, timeout: Duration, until: impl Fn(&World) -> bool) {
    let start = Instant::now();
    // A couple of frames so dirt propagates and jobs start.
    for _ in 0..3 {
        core.run_pre_ui();
    }
    while start.elapsed() < timeout {
        core.run_pre_ui();
        let w = &core.world;
        if settled(w, op) || until(w) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("tracker still busy after {timeout:?}");
}

fn output(w: &World, op: Entity) -> Vec<Option<([f64; 2], f32, FrameState)>> {
    let sig = w.resource::<SignalStore>().get(w.get::<Output>(op).expect("output").0).expect("signal");
    (0..FRAMES).map(|f| sig.get(f).map(|v| ([v[0] as f64, v[1] as f64], v[6], sig.state(f)))).collect()
}

fn status(w: &World, op: Entity) -> TrackStatus {
    w.get::<TrackStatus>(op).cloned().unwrap_or_default()
}

/// Errors against the truth after removing the anchor's offset (a tracker
/// follows the look it was seeded on, so it inherits the guide's error there).
fn errors(out: &[Option<([f64; 2], f32, FrameState)>], anchor: i64) -> Vec<f64> {
    let (a, _, _) = out[anchor as usize].expect("anchor tracked");
    let bias = [a[0] - truth(anchor)[0], a[1] - truth(anchor)[1]];
    let mut e: Vec<f64> = (0..FRAMES)
        .filter_map(|f| out[f as usize].map(|(p, _, _)| ((p[0] - bias[0] - truth(f)[0]).powi(2) + (p[1] - bias[1] - truth(f)[1]).powi(2)).sqrt()))
        .collect();
    e.sort_by(f64::total_cmp);
    e
}

fn all_valid(out: &[Option<([f64; 2], f32, FrameState)>]) -> bool {
    out.iter().flatten().all(|(_, _, s)| *s == FrameState::Valid)
}

#[test]
fn tracks_the_sprite_both_ways_from_the_anchor() {
    let Some((mut core, guide)) = setup() else { return };
    let op = add_tracker(&mut core.world, guide, 600, None).expect("tracker");
    let t = Instant::now();
    run(&mut core, op, Duration::from_secs(180), |_| false);
    let secs = t.elapsed().as_secs_f64();
    let w = &core.world;
    assert_eq!(coverage(w, op), Some(0..FRAMES), "tracked the guide's whole span");
    let out = output(w, op);
    let lost = out.iter().flatten().filter(|(_, s, _)| *s < 0.5).count();
    let e = errors(&out, 600);
    let (median, p95, max) = (e[e.len() / 2], e[e.len() * 95 / 100], e[e.len() - 1]);
    // Re-centred on the guide: the absolute error too (the guide's wander averages out).
    let mut abs: Vec<f64> = (0..FRAMES).filter_map(|f| out[f as usize].map(|(p, _, _)| (p[0] - truth(f)[0]).hypot(p[1] - truth(f)[1]))).collect();
    abs.sort_by(f64::total_cmp);
    eprintln!("absolute error (re-centred on the guide): median {:.3} px, max {:.3}", abs[abs.len() / 2], abs[abs.len() - 1]);
    assert!(abs[abs.len() / 2] < 0.6, "absolute median {:.3}", abs[abs.len() / 2]);
    eprintln!("{FRAMES} frames in {secs:.1} s ({:.0} fps): error median {median:.3} px, p95 {p95:.3}, max {max:.3}; {lost} low-score frames", FRAMES as f64 / secs);
    assert!(all_valid(&out));
    assert_eq!(lost, 0);
    assert!(median < 0.4 && p95 < 1.0 && max < 1.2, "median {median:.3}, p95 {p95:.3}, max {max:.3}");
    assert_ne!(w.get::<TrackBook>(op).expect("book").stamp, 0, "complete results are stamped");
}

#[test]
fn catch_up_mode_holds_at_the_playhead() {
    let Some((mut core, guide)) = setup() else { return };
    let op = add_tracker(&mut core.world, guide, 100, None).expect("tracker");
    core.world.get_mut::<Tracker>(op).expect("tracker").follow_playhead = true;
    core.world.resource_mut::<Transport>().seek(130);
    let frames_at = |w: &World| coverage(w, op).map_or(0..0, |r| r);
    run(&mut core, op, Duration::from_secs(60), |w| frames_at(w).end > 130);
    // Give it time to overshoot if it were going to.
    for _ in 0..40 {
        core.run_pre_ui();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(frames_at(&core.world), 100..131, "forward only up to the playhead; nothing behind the anchor while the playhead is past it");
    core.world.resource_mut::<Transport>().seek(60);
    run(&mut core, op, Duration::from_secs(60), |w| frames_at(w).start <= 60);
    for _ in 0..40 {
        core.run_pre_ui();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(frames_at(&core.world), 60..131, "backward only down to the playhead");
    // Held at the playhead for a while, the jobs let go of their decoders (and their slots).
    let start = Instant::now();
    while core.world.resource::<TrackJobs>().active() && start.elapsed() < Duration::from_secs(5) {
        core.run_pre_ui();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!core.world.resource::<TrackJobs>().active(), "parked jobs don't keep the host busy");
}

/// The app path: the guide is a real sketch, edited with a second stroke on
/// frames 700..760. The sketch recomputes (and reports) its whole extent, yet
/// the tracker resumes near the edit instead of re-tracking from the anchor.
#[test]
fn a_stroke_on_the_guide_resumes_tracking_at_the_stroke() {
    let Some(mut core) = setup_core() else { return };
    let sketch = sketch_guide(&mut core);
    let op = add_tracker(&mut core.world, sketch, 200, None).expect("tracker");
    run(&mut core, op, Duration::from_secs(180), |_| false);
    let before = output(&core.world, op);
    let off_before = core.world.get::<TrackBook>(op).expect("book").offset;
    assert!(all_valid(&before) && before.iter().all(Option::is_some), "the first pass is complete");
    let e = errors(&before, 200);
    eprintln!("sketch guide: error median {:.3} px, max {:.3}", e[e.len() / 2], e[e.len() - 1]);

    // A stroke 3 px off on frames 700..760 (falloff 0.1 s: ~6 frames either side).
    let sketch_out = core.world.get::<Output>(sketch).expect("output").0;
    let guide = |w: &World| -> Vec<Vec<f32>> {
        let s = w.resource::<SignalStore>().get(sketch_out).expect("sketch");
        (0..FRAMES).map(|f| s.get(f).expect("covered").to_vec()).collect()
    };
    let guide_before = guide(&core.world);
    edit(&mut core.world, "Stroke on Sketch", |tx| {
        let s = stroke(tx, 700..760, 0.1, |f| rough_point(f, 3.0));
        tx.modify::<Inputs>(sketch, |i| i.0.push(("stroke".into(), s)));
    });
    core.run_pre_ui();
    assert!(core.world.get::<Dirty>(sketch).is_none_or(|d| d.0.is_empty()), "the sketch re-evaluated");
    // The sketch recomputed everything; its values changed only around the stroke.
    let guide_after = guide(&core.world);
    let changed: Vec<i64> = (0..FRAMES).filter(|f| guide_before[*f as usize] != guide_after[*f as usize]).collect();
    let (first, last) = (changed[0], changed[changed.len() - 1]);
    assert!((680..700).contains(&first) && (760..780).contains(&last), "the guide changed on {first}..={last}");

    let st = status(&core.world, op);
    let fwd = st.forward.expect("a forward job re-tracks after the stroke");
    eprintln!("guide changed on frames {first}..={last}; the forward job resumed at {}", fwd.from);
    assert_eq!(fwd.from, first, "resumed at the first changed frame, not at the anchor");
    assert!(st.backward.is_none(), "nothing before the anchor changed");
    let mid = output(&core.world, op);
    for f in 0..fwd.from as usize {
        assert_eq!(mid[f].map(|m| m.2), Some(FrameState::Valid), "frame {f} is before the change: untouched");
        assert_eq!(mid[f].map(|m| m.0), before[f].map(|m| m.0));
    }
    assert!((fwd.from as usize..FRAMES as usize).all(|f| mid[f].is_some()), "results after the edit stay on screen (stale) while re-tracking");
    run(&mut core, op, Duration::from_secs(180), |_| false);
    let after = output(&core.world, op);
    assert!(all_valid(&after));
    // Where the tracker itself was (minus the re-centring, which the new frames moved) is unchanged before the stroke.
    let raw = |out: &[Option<([f64; 2], f32, FrameState)>], off: [f32; 2], f: usize| out[f].map(|(p, _, _)| [p[0] - off[0] as f64, p[1] - off[1] as f64]).expect("tracked");
    let off_after = core.world.get::<TrackBook>(op).expect("book").offset;
    for f in 0..first as usize {
        let (a, b) = (raw(&after, off_after, f), raw(&before, off_before, f));
        assert!((a[0] - b[0]).abs() < 1e-3 && (a[1] - b[1]).abs() < 1e-3, "frame {f} was never re-tracked: {a:?} vs {b:?}");
    }
    // (Seeded on this guide's point, the tracker slips onto the background
    // for three frames near 1044 in both passes: a limit of the template
    // strategy, not of the resume.)
    let e = errors(&after, 200);
    eprintln!("after the stroke: error median {:.3} px, max {:.3}", e[e.len() / 2], e[e.len() - 1]);
    assert!(e[e.len() / 2] < 1.0, "still on the sprite after re-tracking: median {:.3}", e[e.len() / 2]);
}

#[test]
fn unchanged_inputs_keep_their_results() {
    let Some((mut core, guide)) = setup() else { return };
    let op = add_tracker(&mut core.world, guide, 1100, None).expect("tracker");
    run(&mut core, op, Duration::from_secs(180), |_| false);
    let before = output(&core.world, op);
    // Every operator recomputes (as after an undo of an unrelated edit): nothing re-tracks.
    core.world.resource_mut::<Invalidations>().recompute(op, 0..FRAMES);
    core.run_pre_ui();
    core.run_pre_ui();
    assert_eq!(core.world.resource::<TrackJobs>().busy(), 0, "no jobs: the inputs are the ones the results came from");
    let after = output(&core.world, op);
    assert_eq!(before, after);
    assert!(all_valid(&after));

    // Deleting the guide stops the tracker; its results stay on screen, stale.
    edit(&mut core.world, "Delete", |tx| tx.delete(guide));
    for _ in 0..3 {
        core.run_pre_ui();
    }
    assert!(core.world.get::<OpError>(op).is_some(), "the guide was deleted");
    let orphaned = output(&core.world, op);
    assert!(orphaned.iter().all(|v| v.is_some_and(|(_, _, s)| s == FrameState::Stale)));
    // Undoing the delete brings them back as they were, without tracking.
    undo(&mut core.world);
    for _ in 0..5 {
        core.run_pre_ui();
        assert_eq!(core.world.resource::<TrackJobs>().busy(), 0, "nothing re-tracks");
    }
    assert!(core.world.get::<OpError>(op).is_none());
    assert_eq!(output(&core.world, op), before);
}

/// Opening another video swaps the footage a frame before its document
/// replaces this one: the jobs stop at once, and nothing is written or
/// cleared on the wrong video in between.
#[test]
fn another_video_stops_the_jobs_before_its_document_loads() {
    let Some((mut core, guide)) = setup() else { return };
    let op = add_tracker(&mut core.world, guide, 600, None).expect("tracker");
    run(&mut core, op, Duration::from_secs(60), |w| coverage(w, op).is_some_and(|r| r.end - r.start > 200));
    let before = output(&core.world, op);
    let reopened = Arc::new(VideoIndex::open(fixture().expect("fixture")).expect("fixture opens"));
    core.world.resource_mut::<Footage>().original = reopened;
    core.run_pre_ui();
    assert_eq!(core.world.resource::<TrackJobs>().busy(), 0, "the jobs stop");
    assert_eq!(output(&core.world, op), before, "nothing written or cleared");
    // (Here the document stays: the trackers plan afresh and finish.)
    run(&mut core, op, Duration::from_secs(180), |_| false);
    let out = output(&core.world, op);
    assert!(out.iter().all(Option::is_some) && all_valid(&out));
}

/// A forward-only tracker's results are saved (autosave sees them) and a
/// reopened project keeps them without tracking again.
#[test]
fn a_saved_forward_tracker_reopens_without_tracking() {
    let Some((mut core, guide)) = setup() else { return };
    let op = add_tracker(&mut core.world, guide, 600, None).expect("tracker");
    edit(&mut core.world, "Forward", |tx| tx.modify::<Tracker>(op, |t| t.direction = Direction::Forward));
    let edited = core.world.resource::<History>().revision();
    run(&mut core, op, Duration::from_secs(180), |_| false);
    let before = output(&core.world, op);
    assert_eq!(coverage(&core.world, op), Some(600..FRAMES), "forward only");
    assert!(all_valid(&before));
    let stamp = core.world.get::<TrackBook>(op).expect("book").stamp;
    assert_ne!(stamp, 0, "complete results are stamped");
    assert!(core.world.resource::<History>().revision() > edited, "finishing is a document change (autosave, save on exit)");

    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("tracker_{}.ttproj", std::process::id()));
    let _ = std::fs::remove_file(&path);
    tt_core::persist::save(&mut core.world, &path).expect("saved");
    drop(core);

    let mut core = setup_core().expect("fixture");
    tt_core::persist::load(&mut core.world, &path).expect("loaded");
    let _ = std::fs::remove_file(&path);
    let op = {
        let w = &mut core.world;
        let mut q = w.query::<(Entity, &Operator)>();
        q.iter(w).find(|(_, o)| o.kind == "track").map(|(e, _)| e).expect("the tracker was saved")
    };
    let loaded = core.world.resource::<History>().revision();
    for _ in 0..20 {
        core.run_pre_ui();
        let jobs = core.world.resource::<TrackJobs>();
        assert_eq!((jobs.busy(), jobs.threads()), (0, 0), "no job starts: the saved results match their inputs");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(settled(&core.world, op));
    let after = output(&core.world, op);
    assert_eq!(before, after, "the results survive the round trip");
    assert_eq!(core.world.get::<TrackBook>(op).expect("book").stamp, stamp);
    assert_eq!(core.world.resource::<History>().revision(), loaded, "opening changes nothing to save");
}

/// Undo and redo of "Track", and undoing the delete of a tracker mid-run,
/// leave a complete tracker (the redone one's output comes back empty; the
/// deleted one lost its unfinished frames).
#[test]
fn undo_redo_and_undeleting_a_tracker_complete_it() {
    let Some((mut core, guide)) = setup() else { return };
    let op = add_tracker(&mut core.world, guide, 600, None).expect("tracker");
    run(&mut core, op, Duration::from_secs(180), |_| false);
    let before = output(&core.world, op);
    assert_eq!(core.world.resource::<History>().undo_label(), Some("Track Guide"));
    undo(&mut core.world);
    for _ in 0..3 {
        core.run_pre_ui();
    }
    assert_eq!(core.world.resource::<TrackJobs>().busy(), 0);
    redo(&mut core.world);
    run(&mut core, op, Duration::from_secs(180), |_| false);
    let redone = output(&core.world, op);
    assert!(redone.iter().all(Option::is_some) && all_valid(&redone), "redo tracks again");
    let moved = before.iter().zip(&redone).map(|(a, b)| (a.expect("tracked").0[0] - b.expect("tracked").0[0]).abs()).fold(0.0, f64::max);
    assert!(moved < 1e-3, "the same results: {moved}");

    // Delete it while it re-tracks, then undo the delete.
    edit(&mut core.world, "Re-seed", |tx| tx.modify::<Tracker>(op, |t| t.anchor = 500));
    run(&mut core, op, Duration::from_secs(60), |w| status(w, op).forward.is_some_and(|s| s.at > 560));
    edit(&mut core.world, "Delete", |tx| tx.delete(op));
    for _ in 0..3 {
        core.run_pre_ui();
    }
    assert_eq!(core.world.resource::<TrackJobs>().busy(), 0, "a deleted tracker's jobs stop");
    undo(&mut core.world);
    run(&mut core, op, Duration::from_secs(180), |_| false);
    let back = output(&core.world, op);
    assert!(back.iter().all(Option::is_some) && all_valid(&back), "the undeleted tracker finishes");
    assert_ne!(core.world.get::<TrackBook>(op).expect("book").stamp, 0);
}

/// Re-seeding a tracker from a look on the guide's first frame: nothing runs
/// backward any more. The old backward job must stop, not keep writing its
/// results.
#[test]
fn reseeding_at_the_guides_start_stops_the_backward_job() {
    let Some((mut core, guide)) = setup() else { return };
    let op = add_tracker(&mut core.world, guide, 600, None).expect("tracker");
    run(&mut core, op, Duration::from_secs(60), |w| status(w, op).backward.is_some_and(|s| s.at < 560));
    tt_track::reseed_with_look(&mut core.world, op, tt_track::look::Look::new(0, truth(0), [10.5, 10.5])).expect("look");
    core.run_pre_ui();
    assert_eq!(core.world.get::<Tracker>(op).expect("tracker").anchor, 0);
    assert_eq!(core.world.resource::<History>().undo_label(), Some("Re-seed tracker"));
    let start = Instant::now();
    while !settled(&core.world, op) {
        assert!(start.elapsed() < Duration::from_secs(180), "still busy");
        let st = status(&core.world, op);
        assert!(st.backward.is_none(), "no backward job once the anchor is the first frame");
        assert!(st.forward.is_none_or(|s| s.from == 0), "the forward job starts at the new anchor");
        core.run_pre_ui();
        std::thread::sleep(Duration::from_millis(5));
    }
    let out = output(&core.world, op);
    assert!(out.iter().all(Option::is_some) && all_valid(&out));
    let e = errors(&out, 0);
    assert!(e[e.len() / 2] < 0.5, "median {:.3}", e[e.len() / 2]);
}

/// Settings that don't change the results keep the running jobs; a drag
/// re-plans once, when it ends.
#[test]
fn display_settings_keep_jobs_and_a_drag_replans_once() {
    let Some((mut core, guide)) = setup() else { return };
    let op = add_tracker(&mut core.world, guide, 300, None).expect("tracker");
    run(&mut core, op, Duration::from_secs(60), |w| status(w, op).forward.is_some_and(|s| s.at > 400));
    let from = status(&core.world, op).forward.expect("forward").from;
    assert_eq!(from, 300);
    type Toggle = fn(&mut Tracker);
    let toggles: [(&str, Toggle); 3] =
        [("Centre off", |t| t.center_on_guide = false), ("Follow on", |t| t.follow_playhead = true), ("Follow off", |t| t.follow_playhead = false)];
    for (label, f) in toggles {
        edit(&mut core.world, label, |tx| tx.modify::<Tracker>(op, f));
        core.run_pre_ui();
        let st = status(&core.world, op).forward.expect("still running");
        assert_eq!(st.from, from, "{label}: the forward job kept running");
        assert!(st.at > 400, "{label}: not restarted");
    }

    // A drag on `search`: an edit every frame inside one gesture.
    core.world.resource_mut::<History>().begin("Edit Tracker");
    let at = status(&core.world, op).forward.expect("forward").at;
    for i in 0..20 {
        edit(&mut core.world, "Edit Tracker", |tx| tx.modify::<Tracker>(op, |t| t.search = 1.0 + 0.02 * i as f32));
        core.run_pre_ui();
        let st = status(&core.world, op).forward.expect("forward");
        assert!(st.from == from && st.at >= at, "no restart mid-drag");
        assert!(core.world.resource::<TrackJobs>().threads() <= MAX_JOBS);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(core.world.resource::<TrackJobs>().active(), "the host keeps running frames while a re-plan waits");
    core.world.resource_mut::<History>().end();
    core.run_pre_ui();
    let st = status(&core.world, op).forward.expect("restarted");
    assert!(st.at < at, "the drag's final value re-tracks: at {} (was {at})", st.at);
    run(&mut core, op, Duration::from_secs(180), |_| false);
    let out = output(&core.world, op);
    assert!(out.iter().all(Option::is_some) && all_valid(&out));
    assert!((core.world.get::<Tracker>(op).expect("tracker").search - 1.38).abs() < 1e-6, "the drag's final value");
}

/// A look drawn exactly around the sprite: the tracker starts at its centre
/// and follows it, with nothing shifted afterwards.
#[test]
fn a_placed_look_tracks_exactly_where_it_was_put() {
    use tt_track::look::Look;
    let Some((mut core, guide)) = setup() else { return };
    let look = Look::new(600, truth(600), [10.5, 10.5]);
    let op = tt_track::add_tracker_with_look(&mut core.world, guide, look).expect("tracker");
    assert!(!core.world.get::<Tracker>(op).expect("tracker").center_on_guide, "placed: not re-centred");
    run(&mut core, op, Duration::from_secs(180), |_| false);
    let sig = core.world.resource::<SignalStore>().get(core.world.get::<Output>(op).expect("output").0).expect("signal");
    let mut e: Vec<f64> = (0..FRAMES).filter_map(|f| sig.get(f).map(|v| (v[0] as f64 - truth(f)[0]).hypot(v[1] as f64 - truth(f)[1]))).collect();
    let flagged = (0..FRAMES).filter_map(|f| sig.get(f)).filter(|v| tt_track::flags(v) != 0).count();
    e.sort_by(f64::total_cmp);
    println!("placed look: {} frames, absolute error median {:.3} px, p95 {:.3}, max {:.3}; {flagged} flagged", e.len(), e[e.len() / 2], e[e.len() * 95 / 100], e[e.len() - 1]);
    assert_eq!(e.len(), FRAMES as usize);
    assert!(sig.get(600).is_some_and(|v| (v[0] as f64 - truth(600)[0]).abs() < 1e-3), "the anchor is exactly the look's centre");
    assert!(e[e.len() / 2] < 0.35, "median {:.3}", e[e.len() / 2]);
    assert_eq!(flagged, 0);
}

/// Where the rough pass is wrong, frames are flagged, never removed: 120 px
/// off (the sprite far beyond where it searches) they are lost; 30 or 36 px
/// off (past the box's edge, but where the tracker last saw it and in the
/// guide's boxes a few frames away) the sprite is found and flagged outside
/// the guide's box.
#[test]
fn frames_the_guide_misses_are_flagged_not_removed() {
    use tt_track::look::Look;
    for (shift, want) in [(120.0f32, tt_track::LOST), (36.0, tt_track::OUTSIDE), (30.0, tt_track::OUTSIDE)] {
        let Some((mut core, guide)) = setup() else { return };
        let sig = core.world.get::<Output>(guide).expect("output").0;
        {
            let mut store = core.world.resource_mut::<SignalStore>();
            let s = store.get_mut(sig).expect("guide");
            for f in 700..760 {
                let mut v = rough(f, 0.0);
                for c in [0, 2, 4] {
                    v[c] += shift;
                }
                s.set(f, &v);
            }
        }
        let op = tt_track::add_tracker_with_look(&mut core.world, guide, Look::new(600, truth(600), [10.5, 10.5])).expect("tracker");
        run(&mut core, op, Duration::from_secs(180), |_| false);
        let out = core.world.resource::<SignalStore>().get(core.world.get::<Output>(op).expect("output").0).expect("signal");
        let flags: Vec<u32> = (705..755).map(|f| out.get(f).map_or(u32::MAX, tt_track::flags)).collect();
        let marked = flags.iter().filter(|f| **f != u32::MAX && **f & want != 0).count();
        println!("guide {shift} px off: {marked} of 50 frames flagged {want}; flags {flags:?}");
        assert!(flags.iter().all(|f| *f != u32::MAX), "every frame keeps a value");
        assert!(flags.iter().all(|f| *f != 0), "{shift} px off: every frame the guide misses is flagged");
        assert!(marked >= 40, "{shift} px off: flagged {want} on {marked} of 50");
        assert!((600..700).chain(790..900).all(|f| out.get(f).is_some_and(|v| tt_track::flags(v) == 0)), "the rest are trusted");
        if want == tt_track::OUTSIDE {
            let off = (705..755).map(|f| out.get(f).map_or(f64::INFINITY, |v| (v[0] as f64 - truth(f)[0]).hypot(v[1] as f64 - truth(f)[1]))).fold(0.0, f64::max);
            assert!(off < 1.0, "{shift} px off: still on the sprite (at most {off:.2} px away)");
        }
    }
}

/// A look is a pin: on its frame, the tracker is where the user showed the
/// subject, and it tracks on from there. Looks agree on one point: dragged
/// 4 px right of the sprite's centre, the second look is aligned to the
/// first (the centre), so the path neither jumps there nor after.
#[test]
fn a_look_pins_its_frame_and_tracking_goes_on_from_it() {
    use tt_track::look::Look;
    let Some((mut core, guide)) = setup() else { return };
    let op = tt_track::add_tracker_with_look(&mut core.world, guide, Look::new(600, truth(600), [10.5, 10.5])).expect("tracker");
    let at = [truth(800)[0] + 4.0, truth(800)[1]];
    tt_track::add_look(&mut core.world, op, Look::new(800, at, [10.5, 10.5])).expect("look");
    run(&mut core, op, Duration::from_secs(180), |_| false);
    let sig = core.world.resource::<SignalStore>().get(core.world.get::<Output>(op).expect("output").0).expect("signal");
    let v = sig.get(800).expect("frame 800");
    let err = |f: i64| sig.get(f).map_or(f64::INFINITY, |v| (v[0] as f64 - truth(f)[0]).hypot(v[1] as f64 - truth(f)[1]));
    println!("pinned frame 800: ({:.3}, {:.3}); the look was at ({:.3}, {:.3}), the sprite at {:?}; error {:.3} px, score {}, flags {}", v[0], v[1], at[0], at[1], truth(800), err(800), v[6], tt_track::flags(v));
    assert!(err(800) < 0.35, "pinned on the sprite's centre, the point the first look defines");
    assert_eq!((v[6], tt_track::flags(v)), (1.0, 0));
    let worst = (600..1000).map(err).fold(0.0, f64::max);
    println!("frames 600-999: worst error {worst:.2} px");
    assert!(worst < 1.0, "no jumps between the looks");
}
