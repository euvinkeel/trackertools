//! A tracker's lifetime (`tt_core::span::Span`) on the sprite fixture: its
//! jobs stop at the span's edges, trimming keeps its results (hidden),
//! extending brings them back without tracking them again, and extending
//! into frames it never had resumes from the nearest result. And which way
//! it tracks is asked (`TrackRun`): a new one waits, switching keeps every
//! result, a pause stops it. Skipped when the fixture hasn't been generated
//! (`cargo xtask fixtures`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bevy_ecs::name::Name;
use bevy_ecs::prelude::*;
use tt_core::history::{History, undo};
use tt_core::op::Output;
use tt_core::signal::{FrameState, SignalStore};
use tt_core::sketch::BOX_CHANNELS;
use tt_core::span::{Edge, Span, move_edge, output, set_span};
use tt_core::transport::Transport;
use tt_core::view::SourceSize;
use tt_core::{AppBuilder, Core, CoreModules};
use tt_media::{DecodeOptions, VideoIndex};
use tt_track::look::Look;
use tt_track::runner::{Footage, TrackJobs, TrackStatus, coverage, settled};
use tt_track::TrackModule;

const NAME: &str = "sprite_1080p60.mp4";
const FRAMES: i64 = 1200;

fn fixture() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("TT_FIXTURES") {
        return Some(PathBuf::from(dir).join(NAME)).filter(|p| p.exists());
    }
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().map(|d| d.join("fixtures").join(NAME)).find(|p| p.exists())
}

fn truth(f: i64) -> [f64; 2] {
    let t = f as f64 / 60.0;
    let p = [950.0 + 500.0 * (0.9 * t).sin() + 60.0 * (5.3 * t).sin(), 530.0 + 300.0 * (1.3 * t + 0.7).sin() + 40.0 * (4.1 * t).sin()];
    p.map(|v| 2.0 * (v.floor() / 2.0).floor() + 10.5)
}

/// The fixture and a rough guide (a bare box signal) over the whole clip.
fn setup() -> Option<(Core, Entity)> {
    let Some(fixture) = fixture() else {
        eprintln!("skipped: {NAME} not found (cargo xtask fixtures, or set TT_FIXTURES)");
        return None;
    };
    let mut app = AppBuilder::new();
    app.add_module(CoreModules).add_module(TrackModule);
    let mut core = app.build();
    // (New trackers wait for a button in the app; these start at once.)
    core.world.resource_mut::<tt_track::NewTrackers>().run = tt_track::TrackRun::Both;
    let index = Arc::new(VideoIndex::open(&fixture).expect("fixture opens"));
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
            let t = f as f64 / 60.0;
            let c = truth(f);
            let (x, y) = (c[0] + 4.0 * (2.3 * t).sin(), c[1] + 3.5 * (1.9 * t + 0.5).sin());
            s.set(f, &[x, y, x - 28.0, y - 28.0, x + 28.0, y + 28.0].map(|v| v as f32));
        }
    }
    let guide = w.spawn((Name::new("Guide"), Output(sig))).id();
    Some((core, guide))
}

fn run(core: &mut Core, op: Entity, timeout: Duration) {
    let start = Instant::now();
    for _ in 0..3 {
        core.run_pre_ui();
    }
    while start.elapsed() < timeout {
        core.run_pre_ui();
        if settled(&core.world, op) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("tracker still busy after {timeout:?}");
}

fn values(w: &World, op: Entity) -> Vec<Option<([f32; 2], FrameState)>> {
    let sig = w.resource::<SignalStore>().get(w.get::<Output>(op).expect("output").0).expect("signal");
    (0..FRAMES).map(|f| sig.get(f).map(|v| ([v[0], v[1]], sig.state(f)))).collect()
}

fn status(w: &World, op: Entity) -> TrackStatus {
    w.get::<TrackStatus>(op).cloned().unwrap_or_default()
}

/// Frames whose error against the truth is under 1 px.
fn on_sprite(v: &[Option<([f32; 2], FrameState)>], frames: std::ops::Range<i64>) -> bool {
    frames.into_iter().all(|f| v[f as usize].is_some_and(|(p, _)| (p[0] as f64 - truth(f)[0]).hypot(p[1] as f64 - truth(f)[1]) < 1.0))
}

#[test]
fn jobs_stop_at_the_span_and_extending_brings_back_what_it_had() {
    let Some((mut core, guide)) = setup() else { return };
    let op = tt_track::add_tracker_with_look(&mut core.world, guide, Look::new(600, truth(600), [10.5, 10.5])).expect("tracker");
    set_span(&mut core.world, op, Span::new(400, 800));
    run(&mut core, op, Duration::from_secs(180));
    assert_eq!(coverage(&core.world, op), Some(400..801), "tracked only inside its span");
    let v = values(&core.world, op);
    assert!(on_sprite(&v, 400..801));
    assert_ne!(core.world.get::<tt_track::runner::TrackBook>(op).expect("book").stamp, 0, "complete over its span: stamped");

    // Extend the end: the forward job resumes after the last frame it had.
    move_edge(&mut core.world, op, Edge::Last, 1000);
    core.run_pre_ui();
    core.run_pre_ui();
    let st = status(&core.world, op);
    assert_eq!(st.forward.map(|s| s.from), Some(801), "resumed at the old edge, not the anchor");
    assert!(st.backward.is_none(), "nothing to do before the anchor");
    run(&mut core, op, Duration::from_secs(180));
    assert_eq!(coverage(&core.world, op), Some(400..1001));
    let extended = values(&core.world, op);
    assert!(on_sprite(&extended, 400..1001));
    assert_eq!(&extended[400..801], &v[400..801], "the frames it had are untouched");

    // Trim it back: no jobs, the results stay (hidden from readers).
    move_edge(&mut core.world, op, Edge::Last, 700);
    for _ in 0..5 {
        core.run_pre_ui();
        assert_eq!(core.world.resource::<TrackJobs>().busy(), 0, "trimming tracks nothing");
    }
    assert_eq!(coverage(&core.world, op), Some(400..1001), "the data stays");
    assert_eq!(output(&core.world, op).and_then(|s| s.present_hull()), Some((400, 700)), "readers see only the span");
    // Extending again (or undoing the trim) brings them back as they were, without tracking.
    undo(&mut core.world);
    assert_eq!(core.world.resource::<History>().redo_label(), Some("Trim Tracker 1"));
    for _ in 0..5 {
        core.run_pre_ui();
        assert_eq!(core.world.resource::<TrackJobs>().busy(), 0, "nothing re-tracks");
    }
    assert!(settled(&core.world, op));
    let back = values(&core.world, op);
    assert_eq!(back, extended, "all of it back, valid");
    assert!(back[400..1001].iter().all(|v| v.is_some_and(|(_, s)| s == FrameState::Valid)));
}

/// An anchor before the span still starts the tracker (its path depends on
/// where it began): the frames in between are its lead-in, and nothing past
/// the far edge is tracked.
#[test]
fn a_span_after_the_anchor_tracks_the_lead_in_and_stops_at_the_edge() {
    let Some((mut core, guide)) = setup() else { return };
    let op = tt_track::add_tracker_with_look(&mut core.world, guide, Look::new(600, truth(600), [10.5, 10.5])).expect("tracker");
    set_span(&mut core.world, op, Span::new(700, 750));
    run(&mut core, op, Duration::from_secs(180));
    assert_eq!(coverage(&core.world, op), Some(600..751));
    assert!(on_sprite(&values(&core.world, op), 700..751));
}

/// A trimmed guide guides only where it lives: the tracker's frames end
/// where the guide's span does.
#[test]
fn a_trimmed_guide_limits_the_tracker() {
    let Some((mut core, guide)) = setup() else { return };
    set_span(&mut core.world, guide, Span::new(500, 700));
    let op = tt_track::add_tracker_with_look(&mut core.world, guide, Look::new(600, truth(600), [10.5, 10.5])).expect("tracker");
    run(&mut core, op, Duration::from_secs(180));
    assert_eq!(coverage(&core.world, op), Some(500..701));
    // Untrimming the guide lets it track the rest.
    set_span(&mut core.world, guide, Span::default());
    run(&mut core, op, Duration::from_secs(180));
    assert_eq!(coverage(&core.world, op), Some(0..FRAMES));
    assert!(on_sprite(&values(&core.world, op), 0..FRAMES));
}

/// As in the app: a new tracker waits until asked which way to track; each
/// way keeps what the other tracked; tracking only backward still has the
/// anchor's frame; Pause stops a running job and keeps what it did.
#[test]
fn a_tracker_tracks_only_the_way_it_is_asked() {
    use tt_track::{TrackRun, set_run};
    let Some((mut core, guide)) = setup() else { return };
    core.world.resource_mut::<tt_track::NewTrackers>().run = TrackRun::Paused;
    let op = tt_track::add_tracker_with_look(&mut core.world, guide, Look::new(600, truth(600), [10.5, 10.5])).expect("tracker");
    set_span(&mut core.world, op, Span::new(450, 750));
    for _ in 0..10 {
        core.run_pre_ui();
        assert_eq!(core.world.resource::<TrackJobs>().busy(), 0, "a new tracker waits");
    }
    assert_eq!(coverage(&core.world, op), None, "nothing tracked before it is asked");

    set_run(&mut core.world, op, TrackRun::Forward);
    run(&mut core, op, Duration::from_secs(180));
    assert_eq!(coverage(&core.world, op), Some(600..751), "forward: from its look to the span's end");
    set_run(&mut core.world, op, TrackRun::Backward);
    run(&mut core, op, Duration::from_secs(180));
    assert_eq!(coverage(&core.world, op), Some(450..751), "then backward: the forward results stay");
    assert!(on_sprite(&values(&core.world, op), 450..751));

    // Only backward, from the start: the anchor's frame comes too.
    let other = tt_track::add_tracker_with_look(&mut core.world, guide, Look::new(600, truth(600), [10.5, 10.5])).expect("tracker");
    set_span(&mut core.world, other, Span::new(500, 700));
    set_run(&mut core.world, other, TrackRun::Backward);
    run(&mut core, other, Duration::from_secs(180));
    assert_eq!(coverage(&core.world, other), Some(500..601), "backward, and its anchor");

    // More to track forward; Pause stops it where it is, and it stays stopped.
    move_edge(&mut core.world, op, Edge::Last, 1150);
    set_run(&mut core.world, op, TrackRun::Forward);
    let start = Instant::now();
    while coverage(&core.world, op).is_none_or(|c| c.end < 800) {
        assert!(start.elapsed() < Duration::from_secs(120), "tracking forward again");
        core.run_pre_ui();
        std::thread::sleep(Duration::from_millis(5));
    }
    set_run(&mut core.world, op, TrackRun::Paused);
    core.run_pre_ui();
    assert_eq!(core.world.resource::<TrackJobs>().busy(), 0, "paused: its job stopped");
    let paused_at = coverage(&core.world, op).expect("results").end;
    assert!(paused_at < 1151, "stopped before the end: {paused_at}");
    for _ in 0..20 {
        core.run_pre_ui();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(coverage(&core.world, op).map(|c| c.end), Some(paused_at), "nothing more after the pause");
    assert!(settled(&core.world, op), "paused: nothing to do");
    // Asked again, it goes on from there to the end.
    set_run(&mut core.world, op, TrackRun::Forward);
    run(&mut core, op, Duration::from_secs(180));
    assert_eq!(coverage(&core.world, op), Some(450..1151));
    assert!(on_sprite(&values(&core.world, op), 450..1151));
}
