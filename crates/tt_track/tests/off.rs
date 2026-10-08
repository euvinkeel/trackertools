//! Switching trackers off (`tt_track::off`): where a tracker is off its
//! output is flagged, so a subject goes on with its other members there;
//! switched on again, it counts again. Two manual dots (no jobs, no video).

use bevy_ecs::prelude::*;
use tt_core::history::edit;
use tt_core::op::Output;
use tt_core::signal::SignalStore;
use tt_core::transport::Transport;
use tt_core::{AppBuilder, Core, CoreModules};
use tt_track::TrackModule;
use tt_track::human::{spawn_manual_dot, write_drawn};
use tt_track::off::{TrackerOff, switch_everywhere, switch_from};

fn core() -> Core {
    let mut app = AppBuilder::new();
    app.add_module(CoreModules).add_module(TrackModule);
    let mut core = app.build();
    core.world.resource_mut::<Transport>().frame_count = 100;
    core
}

/// A manual dot moving `speed` px a frame to the right from x0 on frames 0–59.
fn dot(core: &mut Core, name: &str, x0: f64, speed: f64) -> Entity {
    let mut made = None;
    edit(&mut core.world, "dot", |tx| {
        let d = spawn_manual_dot(tx, name.into(), 0);
        let points: Vec<(i64, Option<[f64; 2]>)> = (0..60).map(|f| (f, Some([x0 + speed * f as f64, 300.0]))).collect();
        write_drawn(tx, d, &points);
        made = Some(d);
    });
    made.expect("a dot")
}

fn x(core: &Core, e: Entity, f: i64) -> f64 {
    let w = &core.world;
    w.resource::<SignalStore>().get(w.get::<Output>(e).expect("output").0).and_then(|s| s.get(f)).expect("a value")[0] as f64
}

fn flags(core: &Core, e: Entity, f: i64) -> u32 {
    let w = &core.world;
    tt_track::flags(w.resource::<SignalStore>().get(w.get::<Output>(e).expect("output").0).and_then(|s| s.get(f)).expect("a value"))
}

#[test]
fn a_tracker_switched_off_leaves_the_subject_to_the_others() {
    let mut core = core();
    let a = dot(&mut core, "slow", 100.0, 1.0);
    let b = dot(&mut core, "fast", 400.0, 3.0);
    core.run_pre_ui();
    let s = tt_core::subject::make_subject(&mut core.world, &[a, b], 0).expect("a subject");
    core.run_pre_ui();
    // Both count: it moves by their mean, 2 px a frame.
    assert!((x(&core, s, 10) - x(&core, s, 0) - 20.0).abs() < 1e-3);

    // The fast one off from 20 to 40: there the slow one alone carries it (1 px a frame).
    switch_from(&mut core.world, &[b], 20, true);
    switch_from(&mut core.world, &[b], 40, false);
    core.run_pre_ui();
    core.run_pre_ui();
    assert_eq!(flags(&core, b, 19), 0);
    assert_eq!(flags(&core, b, 25), tt_track::OFF, "flagged where off");
    assert_eq!(flags(&core, b, 40), 0, "on again");
    assert!((x(&core, s, 19) - x(&core, s, 10) - 18.0).abs() < 1e-3, "both before");
    assert!((x(&core, s, 39) - x(&core, s, 21) - 18.0).abs() < 1e-3, "the slow one alone while the fast one is off: {}", x(&core, s, 39) - x(&core, s, 21));
    assert!((x(&core, s, 50) - x(&core, s, 41) - 18.0).abs() < 1e-3, "both again after");
    // Its own results are untouched: switched off, its point is where it was.
    assert!((x(&core, b, 25) - (400.0 + 75.0)).abs() < 1e-3);

    // Undo: on everywhere again; then off everywhere, and on everywhere.
    tt_core::history::undo(&mut core.world);
    tt_core::history::undo(&mut core.world);
    core.run_pre_ui();
    assert_eq!(flags(&core, b, 25), 0, "undone");
    switch_everywhere(&mut core.world, &[a], true);
    core.run_pre_ui();
    assert!((0..60).all(|f| flags(&core, a, f) == tt_track::OFF));
    switch_everywhere(&mut core.world, &[a], false);
    core.run_pre_ui();
    assert_eq!(core.world.get::<TrackerOff>(a).map(|o| o.0.len()), Some(0));
    assert_eq!(flags(&core, a, 30), 0);
}
