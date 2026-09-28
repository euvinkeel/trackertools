//! End to end on the sprite fixture (`cargo xtask fixtures`): a guide that is
//! only roughly right (a rough pass), a tracker run by background jobs forward
//! and backward from its anchor through the long-GOP source, measured against
//! the analytic truth. Skipped when the fixture hasn't been generated.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bevy_ecs::name::Name;
use bevy_ecs::prelude::*;
use tt_core::op::{Dirty, Invalidations, Output};
use tt_core::signal::{FrameState, SignalStore};
use tt_core::sketch::BOX_CHANNELS;
use tt_core::transport::Transport;
use tt_core::view::SourceSize;
use tt_core::{AppBuilder, Core, CoreModules};
use tt_media::{DecodeOptions, VideoIndex};
use tt_track::runner::{Footage, TrackJobs, coverage};
use tt_track::{TrackModule, Tracker, add_tracker};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/sprite_1080p60.mp4");
const FRAMES: i64 = 1200;

/// The sprite's centre at frame f (source px). ffmpeg's overlay on yuv420
/// places it on even pixels (chroma alignment), so its corner is the formula
/// rounded down to even.
fn truth(f: i64) -> [f64; 2] {
    let t = f as f64 / 60.0;
    let even = |v: f64| 2.0 * (v.floor() / 2.0).floor();
    [
        even(950.0 + 500.0 * (0.9 * t).sin() + 60.0 * (5.3 * t).sin()) + 10.5,
        even(530.0 + 300.0 * (1.3 * t + 0.7).sin() + 40.0 * (4.1 * t).sin()) + 10.5,
    ]
}

/// A rough pass: the truth plus a wander of up to ~6 px, in a 56 px box.
fn rough(f: i64, shift: f64) -> [f32; 6] {
    let t = f as f64 / 60.0;
    let c = truth(f);
    let (x, y) = (c[0] + 4.0 * (2.3 * t).sin() + 2.0 * (6.1 * t).cos() + shift, c[1] + 3.5 * (1.9 * t + 0.5).sin() - shift);
    [x, y, x - 28.0, y - 28.0, x + 28.0, y + 28.0].map(|v| v as f32)
}

fn setup() -> Option<(Core, Entity)> {
    if !Path::new(FIXTURE).exists() {
        eprintln!("skipped: {FIXTURE} missing (cargo xtask fixtures)");
        return None;
    }
    let mut app = AppBuilder::new();
    app.add_module(CoreModules).add_module(TrackModule);
    let mut core = app.build();
    let index = Arc::new(VideoIndex::open(FIXTURE).expect("fixture opens"));
    let w = &mut core.world;
    {
        let mut t = w.resource_mut::<Transport>();
        t.fps = index.fps;
        t.frame_count = index.frame_count();
    }
    w.insert_resource(SourceSize { width: index.width as f64, height: index.height as f64 });
    w.insert_resource(Footage { original: index, proxy: None, decode: DecodeOptions::default() });
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

/// Run app frames until the tracker is idle (or `until` says stop).
fn run(core: &mut Core, op: Entity, timeout: Duration, until: impl Fn(&World) -> bool) {
    let start = Instant::now();
    // A couple of frames so dirt propagates and jobs start.
    for _ in 0..3 {
        core.run_pre_ui();
    }
    while start.elapsed() < timeout {
        core.run_pre_ui();
        let w = &core.world;
        let idle = w.resource::<TrackJobs>().busy() == 0 && w.get::<Dirty>(op).is_none_or(|d| d.0.is_empty());
        if idle || until(w) {
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
    assert!(out.iter().flatten().all(|(_, _, s)| *s == FrameState::Valid));
    assert_eq!(lost, 0);
    assert!(median < 0.4 && p95 < 1.0 && max < 1.2, "median {median:.3}, p95 {p95:.3}, max {max:.3}");
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
}

#[test]
fn an_edit_ahead_of_the_anchor_retracks_only_from_there() {
    let Some((mut core, guide)) = setup() else { return };
    let op = add_tracker(&mut core.world, guide, 200, None).expect("tracker");
    run(&mut core, op, Duration::from_secs(180), |_| false);
    let before = output(&core.world, op);
    // Nudge the guide on frames 700..760 (the rough pass redrawn there).
    let sig = core.world.get::<Output>(guide).expect("output").0;
    {
        let mut store = core.world.resource_mut::<SignalStore>();
        let s = store.get_mut(sig).expect("guide");
        for f in 700..760 {
            s.set(f, &rough(f, 3.0));
        }
    }
    core.world.resource_mut::<Invalidations>().output_changed(guide, 700..760);
    core.run_pre_ui();
    let mid = output(&core.world, op);
    for f in 0..700 {
        assert_eq!(mid[f].map(|m| m.2), Some(FrameState::Valid), "frame {f} is before the edit: untouched");
        assert_eq!(mid[f].map(|m| m.0), before[f].map(|m| m.0));
    }
    assert!((700..FRAMES as usize).all(|f| mid[f].is_some()), "results after the edit stay on screen (stale) while re-tracking");
    run(&mut core, op, Duration::from_secs(180), |_| false);
    let after = output(&core.world, op);
    assert!(after.iter().flatten().all(|(_, _, s)| *s == FrameState::Valid));
    let e = errors(&after, 200);
    assert!(e[e.len() / 2] < 0.4, "still on the sprite after re-tracking: median {:.3}", e[e.len() / 2]);
}

#[test]
fn unchanged_inputs_keep_their_results() {
    let Some((mut core, guide)) = setup() else { return };
    let op = add_tracker(&mut core.world, guide, 1100, None).expect("tracker");
    run(&mut core, op, Duration::from_secs(180), |_| false);
    let before = output(&core.world, op);
    // What a reopened project does: every operator recomputes from scratch.
    core.world.resource_mut::<Invalidations>().recompute(op, 0..FRAMES);
    core.run_pre_ui();
    core.run_pre_ui();
    assert_eq!(core.world.resource::<TrackJobs>().busy(), 0, "no jobs: the inputs are the ones the results came from");
    let after = output(&core.world, op);
    assert_eq!(before, after);
    assert!(after.iter().flatten().all(|(_, _, s)| *s == FrameState::Valid));
}
