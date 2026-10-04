//! The human layer (`tt_track::human`), manual dots, trackers with no guide
//! and CoTracker's reset points.
//!
//! - Drawn frames are the tracker's output there, its automatic results
//!   elsewhere; erasing brings them back; both are undo steps; drawing never
//!   touches the automatic layer.
//! - The Draw tool, headless: a hold makes a manual dot, frames skipped while
//!   playing fill in a line, the hold is one undo step.
//! - A CoTracker has one reset point a frame: placing another on that frame moves it.
//! - On the sprite fixture (`cargo xtask fixtures`; skipped without it): a
//!   template tracker with no sketch follows the sprite over the whole clip,
//!   and so does CoTracker (skipped without its Python and weights).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bevy_ecs::prelude::*;
use tt_core::history::{History, edit, undo};
use tt_core::op::{Inputs, OpError, Operator, Output};
use tt_core::selection::Selection;
use tt_core::signal::{FrameState, SignalStore};
use tt_core::span::{Span, set_span};
use tt_core::tool::{ActiveTool, PointerFrame, Tool};
use tt_core::transport::Transport;
use tt_core::view::SourceSize;
use tt_core::{AppBuilder, Core, CoreModules};
use tt_media::{DecodeOptions, VideoIndex};
use tt_track::human::{AutoOutput, HumanLayer, drawn_at, erase_drawn, write_drawn};
use tt_track::look::{Look, looks_of};
use tt_track::runner::{Footage, coverage, settled};
use tt_track::{Method, NewTrackers, TRACK_CHANNELS, TrackModule, TrackRun, Tracker};

fn core(frames: i64) -> Core {
    let mut app = AppBuilder::new();
    app.add_module(CoreModules).add_module(TrackModule);
    let mut core = app.build();
    core.world.resource_mut::<Transport>().frame_count = frames;
    core
}

fn value(w: &World, op: Entity, f: i64) -> Option<([f32; 8], FrameState)> {
    let sig = w.resource::<SignalStore>().get(w.get::<Output>(op)?.0)?;
    sig.get(f).map(|v| (std::array::from_fn(|c| v[c]), sig.state(f)))
}

fn auto_value(w: &World, op: Entity, f: i64) -> Option<[f32; 8]> {
    let sig = w.resource::<SignalStore>().get(w.get::<AutoOutput>(op)?.0)?;
    sig.get(f).map(|v| std::array::from_fn(|c| v[c]))
}

/// A paused tracker with automatic results on frames 0–49 (x = 100 + f, y = 50; frame 30 lost and stale).
fn tracker_with_results(core: &mut Core) -> Entity {
    let w = &mut core.world;
    let (out, auto) = {
        let mut store = w.resource_mut::<SignalStore>();
        (store.create(TRACK_CHANNELS), store.create(TRACK_CHANNELS))
    };
    {
        let mut store = w.resource_mut::<SignalStore>();
        let a = store.get_mut(auto).expect("created");
        for f in 0..50 {
            let x = 100.0 + f as f32;
            a.set(f, &[x, 50.0, x - 5.0, 45.0, x + 5.0, 55.0, 0.9, if f == 30 { 1.0 } else { 0.0 }]);
        }
        a.mark_stale(30..31);
    }
    let op = w
        .spawn((Operator { kind: "track".into() }, Inputs(Vec::new()), Output(out), AutoOutput(auto), Tracker::at(0), tt_track::runner::TrackBook::default(), TrackRun::Paused))
        .id();
    for _ in 0..2 {
        core.run_pre_ui();
    }
    op
}

#[test]
fn drawn_frames_are_the_output_there_and_erasing_brings_the_results_back() {
    let mut core = core(100);
    let op = tracker_with_results(&mut core);
    let w = &core.world;
    assert_eq!(value(w, op, 10), Some(([110.0, 50.0, 105.0, 45.0, 115.0, 55.0, 0.9, 0.0], FrameState::Valid)), "its results are its output");
    assert_eq!(value(w, op, 30).map(|(_, s)| s), Some(FrameState::Stale), "as they are: stale too");

    // Draw frames 28–32 (over the lost, stale 30) and 70 (where it has no result).
    edit(&mut core.world, "Draw", |tx| write_drawn(tx, op, &(28..=32).map(|f| (f, Some([300.0, 200.0 + f as f64]))).chain([(70, Some([400.0, 400.0]))]).collect::<Vec<_>>()));
    core.run_pre_ui();
    let w = &core.world;
    assert_eq!(value(w, op, 30), Some(([300.0, 230.0, 295.0, 225.0, 305.0, 235.0, 1.0, 0.0], FrameState::Valid)), "the drawn point, the result's box size, trusted");
    assert_eq!(value(w, op, 70).map(|(v, _)| [v[0], v[1], v[6], v[7]]), Some([400.0, 400.0, 1.0, 0.0]), "drawn where nothing was tracked");
    assert_eq!(value(w, op, 27).map(|(v, _)| v[0]), Some(127.0), "elsewhere its own result");
    assert_eq!(auto_value(w, op, 30).map(|v| v[0]), Some(130.0), "drawing never touches the automatic results");
    assert_eq!(drawn_at(w, op, 29), Some([300.0, 229.0]));
    // Consumers see it: a subject's points, the stabilizer's.
    assert!(tt_core::subject::points_of(w, op).contains(&(30, [300.0, 230.0])), "drawn frames count as good points");

    // Erase one frame: its result shows again (stale, as it is).
    assert!(erase_drawn(&mut core.world, op, Some(30..31)));
    core.run_pre_ui();
    assert_eq!(value(&core.world, op, 30).map(|(v, s)| (v[0], s)), Some((130.0, FrameState::Stale)));
    // Undo the erase, then the drawing: as it was.
    undo(&mut core.world);
    core.run_pre_ui();
    assert_eq!(value(&core.world, op, 30).map(|(v, _)| v[0]), Some(300.0));
    undo(&mut core.world);
    core.run_pre_ui();
    let w = &core.world;
    assert_eq!(value(w, op, 30).map(|(v, s)| (v[0], s)), Some((130.0, FrameState::Stale)));
    assert_eq!(value(w, op, 70), None, "nothing where it never tracked");
    assert!(w.get::<HumanLayer>(op).is_none(), "the layer came with the first drawing, and went with it");

    // Stale marks from outside (an input changed) don't dim drawn frames for long.
    edit(&mut core.world, "Draw", |tx| write_drawn(tx, op, &[(5, Some([1.0, 2.0]))]));
    core.run_pre_ui();
    let out = core.world.get::<Output>(op).expect("output").0;
    core.world.resource_mut::<SignalStore>().get_mut(out).expect("signal").mark_stale(0..50);
    core.run_pre_ui();
    assert_eq!(value(&core.world, op, 5).map(|(_, s)| s), Some(FrameState::Valid), "drawn frames depend on nothing");
}

/// One app frame of the Draw tool: the pointer at `at` (shown space = source), with the button's transitions.
fn pointer(core: &mut Core, t: f64, at: [f64; 2], pressed: bool, down: bool, released: bool) {
    *core.world.resource_mut::<PointerFrame>() = PointerFrame {
        samples: if released { Vec::new() } else { vec![[t, at[0], at[1]]] },
        hover: Some(at),
        pressed: pressed.then_some(t),
        down,
        released: released.then_some(t),
        scale: 1.0,
        ..PointerFrame::default()
    };
    core.run_pre_ui();
}

#[test]
fn the_draw_tool_makes_a_manual_dot_and_a_hold_is_one_undo_step() {
    let mut core = core(100);
    core.world.resource_mut::<ActiveTool>().0 = Tool::Draw;
    core.world.resource_mut::<Transport>().seek(10);
    pointer(&mut core, 1.0, [100.0, 100.0], true, true, false);
    let dot = core.world.resource::<Selection>().primary().expect("a new dot, selected");
    assert!(core.world.get::<Tracker>(dot).is_some_and(|t| t.method == Method::Manual));
    assert_eq!(drawn_at(&core.world, dot, 10), Some([100.0, 100.0]));
    // Holding still on a paused frame: it follows the hand there.
    pointer(&mut core, 1.1, [104.0, 100.0], false, true, false);
    assert_eq!(drawn_at(&core.world, dot, 10), Some([104.0, 100.0]));
    // Playing fast, frames 11 and 12 go by between two pointer reports: filled in a line.
    core.world.resource_mut::<Transport>().seek(13);
    pointer(&mut core, 1.2, [134.0, 100.0], false, true, false);
    let w = &core.world;
    assert_eq!([11, 12, 13].map(|f| drawn_at(w, dot, f).map(|p| p[0].round())), [Some(114.0), Some(124.0), Some(134.0)]);
    pointer(&mut core, 1.3, [134.0, 100.0], false, false, true);
    let w = &core.world;
    assert_eq!(value(w, dot, 12).map(|(v, s)| (v[0].round(), v[6], s)), Some((124.0, 1.0, FrameState::Valid)), "its output is what was drawn");
    assert_eq!(value(w, dot, 14), None, "and nothing else");
    assert!(settled(w, dot), "a manual dot has nothing to track");
    assert_eq!(w.resource::<History>().undo_label(), Some("Draw Manual dot 1"));
    undo(&mut core.world);
    core.run_pre_ui();
    assert!(core.world.get::<bevy_ecs::entity_disabling::Disabled>(dot).is_some(), "one undo takes the whole hold back, the dot too");

    // Alt+hold on a selected dot erases.
    tt_core::history::redo(&mut core.world);
    core.world.resource_mut::<Selection>().select_only(dot);
    core.world.resource_mut::<tt_core::input::KeysHeld>().mods.alt = true;
    core.world.resource_mut::<Transport>().seek(12);
    pointer(&mut core, 2.0, [0.0, 0.0], true, true, false);
    pointer(&mut core, 2.1, [0.0, 0.0], false, false, true);
    assert_eq!(drawn_at(&core.world, dot, 12), None);
    assert!(drawn_at(&core.world, dot, 11).is_some());
}

#[test]
fn a_cotracker_has_one_reset_point_a_frame() {
    let mut core = core(100);
    core.world.resource_mut::<NewTrackers>().method = Method::CoTracker;
    let op = tt_track::add_unguided_tracker(&mut core.world, Look::new(20, [100.0, 100.0], [8.0, 8.0])).expect("a tracker with no sketch");
    let w = &mut core.world;
    assert!(tt_track::guide_of(w, op).is_none());
    let moved = tt_track::set_reset_point(w, op, Look::new(20, [110.0, 90.0], [8.0, 8.0])).expect("moved");
    assert_eq!(looks_of(w, op), vec![moved], "the same frame: moved, not added");
    assert_eq!(w.get::<Look>(moved).map(|l| (l.x, l.y)), Some((110.0, 90.0)));
    let other = tt_track::set_reset_point(w, op, Look::new(40, [150.0, 90.0], [8.0, 8.0])).expect("added");
    assert_eq!(looks_of(w, op), vec![moved, other]);
    let names: Vec<String> = looks_of(w, op).iter().map(|l| w.get::<bevy_ecs::name::Name>(*l).map(|n| n.to_string()).unwrap_or_default()).collect();
    assert_eq!(names, ["Reset point 1", "Reset point 2"]);
    assert_eq!(w.resource::<History>().undo_label(), Some("Add reset point"));
}

fn fixture() -> Option<PathBuf> {
    let name = "sprite_1080p60.mp4";
    if let Some(dir) = std::env::var_os("TT_FIXTURES") {
        return Some(PathBuf::from(dir).join(name)).filter(|p| p.exists());
    }
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().map(|d| d.join("fixtures").join(name)).find(|p| p.exists())
}

fn truth(f: i64) -> [f64; 2] {
    let t = f as f64 / 60.0;
    let p = [950.0 + 500.0 * (0.9 * t).sin() + 60.0 * (5.3 * t).sin(), 530.0 + 300.0 * (1.3 * t + 0.7).sin() + 40.0 * (4.1 * t).sin()];
    p.map(|v| 2.0 * (v.floor() / 2.0).floor() + 10.5)
}

/// The sprite fixture open, trackers starting at once.
fn sprite() -> Option<Core> {
    let Some(fixture) = fixture() else {
        eprintln!("skipped: sprite_1080p60.mp4 not found (cargo xtask fixtures)");
        return None;
    };
    let mut core = core(0);
    core.world.resource_mut::<NewTrackers>().run = TrackRun::Both;
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

fn run_until_settled(core: &mut Core, op: Entity, timeout: Duration) -> f64 {
    let start = Instant::now();
    for _ in 0..3 {
        core.run_pre_ui();
    }
    while !settled(&core.world, op) {
        assert!(start.elapsed() < timeout, "still tracking after {timeout:?}");
        core.run_pre_ui();
        std::thread::sleep(Duration::from_millis(5));
    }
    start.elapsed().as_secs_f64()
}

/// Errors against the truth over `frames`, sorted.
fn errors(w: &World, op: Entity, frames: std::ops::Range<i64>) -> Vec<f64> {
    let out = w.resource::<SignalStore>().get(w.get::<Output>(op).expect("output").0).expect("signal");
    let mut e: Vec<f64> = frames.map(|f| out.get(f).map_or(f64::INFINITY, |v| (v[0] as f64 - truth(f)[0]).hypot(v[1] as f64 - truth(f)[1]))).collect();
    e.sort_by(f64::total_cmp);
    e
}

#[test]
fn a_template_tracker_with_no_sketch_follows_the_sprite_over_the_whole_frame() {
    let Some(mut core) = sprite() else { return };
    let op = tt_track::add_unguided_tracker(&mut core.world, Look::new(600, truth(600), [10.5, 10.5])).expect("tracker");
    let secs = run_until_settled(&mut core, op, Duration::from_secs(600));
    let w = &core.world;
    assert!(w.get::<OpError>(op).is_none(), "{:?}", w.get::<OpError>(op));
    assert_eq!(coverage(w, op), Some(0..1200), "both ways over the whole video: its guide is the frame");
    let e = errors(w, op, 0..1200);
    eprintln!("no sketch: 1200 frames in {secs:.1} s; error median {:.3} px, p99 {:.3}, max {:.3}", e[600], e[1188], e[1199]);
    assert!(e[600] < 0.3, "median {}", e[600]);
    assert!(e[1188] < 1.0, "p99 {}", e[1188]);
}

#[test]
fn cotracker_with_no_sketch_follows_the_sprite() {
    let Some(mut core) = sprite() else { return };
    let (python, _) = tt_track::job::worker_command();
    let check = "import os, torch, av, cv2; p = os.environ.get('TT_COTRACKER_WEIGHTS') or os.path.join(torch.hub.get_dir(), 'checkpoints', 'scaled_online.pth'); print(os.path.exists(p))";
    let ready = std::process::Command::new(&python).args(["-c", check]).output().is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "True");
    if !ready && tt_track::job::cotracker_model().is_none_or(|m| !m.is_file()) {
        eprintln!("skipped: no CoTracker Python and weights here");
        return;
    }
    core.world.resource_mut::<NewTrackers>().method = Method::CoTracker;
    let op = tt_track::add_unguided_tracker(&mut core.world, Look::new(600, truth(600), [10.5, 10.5])).expect("tracker");
    set_span(&mut core.world, op, Span::new(570, 630));
    let secs = run_until_settled(&mut core, op, Duration::from_secs(900));
    let w = &core.world;
    assert!(w.get::<OpError>(op).is_none(), "{:?}", w.get::<OpError>(op));
    let e = errors(w, op, 570..631);
    eprintln!("CoTracker, no sketch: 61 frames in {secs:.1} s; error median {:.2} px, max {:.2}", e[30], e[60]);
    // The model sees the whole 1920 px frame in its 512 px input (3.75 px a
    // model pixel; the 21 px sprite is ~6 of them): about a model pixel off.
    // Inside a sketch it sees the sketch's region, closer (tests/cotracker.rs).
    assert!(e[30] < 4.0, "median {}", e[30]);
    assert!(e[60] < 12.0, "max {}", e[60]);
    assert!(value(w, op, 600).is_some_and(|(v, _)| (v[0] as f64 - truth(600)[0]).abs() < 1e-3), "the reset point is exact (not aligned)");
}
