//! The Track command (T): one undo step whatever is selected, and anchors
//! inside the guide's frames. No video needed.

use std::ops::Range;

use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::name::Name;
use bevy_ecs::prelude::*;
use tt_core::history::{History, redo, undo};
use tt_core::input::{Action, PendingActions};
use tt_core::op::{Operator, Output};
use tt_core::selection::Selection;
use tt_core::signal::SignalStore;
use tt_core::sketch::BOX_CHANNELS;
use tt_core::transport::Transport;
use tt_core::{AppBuilder, Core, CoreModules};
use tt_track::{TrackModule, Tracker, guide_of, is_tracker};

/// A sketch whose boxes cover `frames` (as evaluated; it has no strokes to recompute from).
fn sketch(core: &mut Core, name: &str, frames: Range<i64>) -> Entity {
    let w = &mut core.world;
    let sig = w.resource_mut::<SignalStore>().create(BOX_CHANNELS);
    let mut store = w.resource_mut::<SignalStore>();
    let s = store.get_mut(sig).expect("created");
    for f in frames {
        s.set(f, &[100.0, 100.0, 80.0, 80.0, 120.0, 120.0]);
    }
    w.spawn((Name::new(name.to_string()), Operator { kind: "sketch".into() }, Output(sig))).id()
}

fn track_at(core: &mut Core, frame: i64, selected: Vec<Entity>) {
    core.world.resource_mut::<Selection>().entities = selected;
    core.world.resource_mut::<Transport>().seek(frame);
    core.world.resource_mut::<PendingActions>().push(Action::Track);
    core.run_pre_ui();
}

fn live_trackers(core: &mut Core) -> Vec<Entity> {
    let w = &mut core.world;
    let mut q = w.query_filtered::<Entity, Without<Disabled>>();
    let all: Vec<Entity> = q.iter(w).collect();
    all.into_iter().filter(|e| is_tracker(w, *e)).collect()
}

#[test]
fn tracking_several_sketches_is_one_undo_step_and_reseeds_stay_in_the_guide() {
    let mut app = AppBuilder::new();
    app.add_module(CoreModules).add_module(TrackModule);
    let mut core = app.build();
    core.world.resource_mut::<Transport>().frame_count = 300;
    let a = sketch(&mut core, "A", 50..150);
    let b = sketch(&mut core, "B", 100..200);

    track_at(&mut core, 120, vec![a, b]);
    let made = live_trackers(&mut core);
    assert_eq!(made.len(), 2);
    assert_eq!(core.world.resource::<History>().undo_label(), Some("Track 2 sketches"));
    assert_eq!(core.world.resource::<Selection>().entities.len(), 2, "the new trackers are selected");
    undo(&mut core.world);
    core.run_pre_ui();
    assert!(live_trackers(&mut core).is_empty(), "one undo removes both");
    redo(&mut core.world);
    core.run_pre_ui();
    assert_eq!(live_trackers(&mut core).len(), 2);

    // T on a tracker past its guide's last frame: re-seeded on the last one.
    let on_a = made.iter().copied().find(|t| guide_of(&core.world, *t) == Some(a)).expect("A's tracker");
    track_at(&mut core, 250, vec![on_a]);
    assert_eq!(core.world.get::<Tracker>(on_a).expect("tracker").anchor, 149);
    assert_eq!(core.world.resource::<History>().undo_label(), Some("Re-seed tracker"));
    // Again: already there, nothing to do (no edit).
    let revision = core.world.resource::<History>().revision();
    track_at(&mut core, 260, vec![on_a]);
    assert_eq!(core.world.resource::<History>().revision(), revision);
    undo(&mut core.world);
    assert_eq!(core.world.get::<Tracker>(on_a).expect("tracker").anchor, 120);
}
