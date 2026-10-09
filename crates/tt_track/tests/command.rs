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
use tt_core::op::{Dirty, Inputs};
use tt_core::tool::{ActiveTool, PointerFrame, Tool};
use tt_track::look::{Look, LookMasker, looks_of};
use tt_track::tool::TrackTool;
use tt_track::{TrackModule, Tracker, add_look, guide_of, is_tracker, reseed_with_look};

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
    // (New trackers wait for a button in the app; these start at once.)
    core.world.resource_mut::<tt_track::NewTrackers>().run = tt_track::TrackRun::Both;
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

    // T on a tracker past its guide's last frame, where it has no look: it
    // makes none (its own position there may be wrong); the Track tool asks
    // for one instead.
    let on_a = made.iter().copied().find(|t| guide_of(&core.world, *t) == Some(a)).expect("A's tracker");
    let revision = core.world.resource::<History>().revision();
    let looks = looks_of(&core.world, on_a).len();
    track_at(&mut core, 250, vec![on_a]);
    assert_eq!(core.world.resource::<History>().revision(), revision, "no edit");
    assert_eq!(looks_of(&core.world, on_a).len(), looks, "no look invented");
    assert_eq!(core.world.resource::<ActiveTool>().0, Tool::Track);
    assert_eq!(core.world.resource::<TrackTool>().reseed, Some(on_a), "the next drag re-seeds it");
    // The look the user shows it there: first (it seeds), the anchor on its frame.
    let shown = reseed_with_look(&mut core.world, on_a, Look::new(149, [101.0, 99.0], [8.0, 8.0])).expect("look");
    assert_eq!(core.world.get::<Tracker>(on_a).expect("tracker").anchor, 149);
    assert_eq!(looks_of(&core.world, on_a).first(), Some(&shown));
    assert_eq!(core.world.resource::<History>().undo_label(), Some("Re-seed tracker"));
    // T there again: already its anchor and first look, nothing to do (no edit).
    let revision = core.world.resource::<History>().revision();
    track_at(&mut core, 260, vec![on_a]);
    assert_eq!(core.world.resource::<History>().revision(), revision);
    // T on a frame with a look: that look moves first and the anchor goes there.
    let other = add_look(&mut core.world, on_a, Look::new(130, [100.0, 100.0], [8.0, 8.0])).expect("look");
    track_at(&mut core, 130, vec![on_a]);
    assert_eq!(core.world.get::<Tracker>(on_a).expect("tracker").anchor, 130);
    assert_eq!(looks_of(&core.world, on_a).first(), Some(&other));
    undo(&mut core.world);
    assert_eq!(core.world.get::<Tracker>(on_a).expect("tracker").anchor, 149);
    assert_eq!(looks_of(&core.world, on_a).first(), Some(&shown));
}

#[test]
fn deleting_or_restoring_a_look_re_tracks() {
    let mut app = AppBuilder::new();
    app.add_module(CoreModules).add_module(TrackModule);
    let mut core = app.build();
    // (New trackers wait for a button in the app; these start at once.)
    core.world.resource_mut::<tt_track::NewTrackers>().run = tt_track::TrackRun::Both;
    core.world.resource_mut::<Transport>().frame_count = 300;
    let a = sketch(&mut core, "A", 50..150);
    track_at(&mut core, 120, vec![a]);
    let t = live_trackers(&mut core)[0];
    let look = add_look(&mut core.world, t, Look::new(130, [100.0, 100.0], [8.0, 8.0])).expect("look");
    core.run_pre_ui();
    let clean = |core: &mut Core| core.world.get_mut::<Dirty>(t).expect("dirty").0 = Default::default();
    let dirty = |core: &Core| core.world.get::<Dirty>(t).is_some_and(|d| !d.0.is_empty());
    clean(&mut core);
    tt_core::commands::delete(&mut core.world, &[look]);
    core.run_pre_ui();
    assert!(dirty(&core), "deleting a look re-tracks its tracker");
    assert!(core.world.get::<Inputs>(t).expect("inputs").0.iter().any(|(_, p)| *p == look), "(the input stays, disabled: undo brings it back)");
    clean(&mut core);
    undo(&mut core.world);
    core.run_pre_ui();
    assert!(dirty(&core), "and so does bringing it back");
}

fn masked_everywhere(_: &World, _: &Look) -> Option<Vec<u8>> {
    Some(vec![255; tt_track::look::MASK_N * tt_track::look::MASK_N])
}

/// One drag on the video with the Track tool, from `a` to `b` (source px).
fn drag(core: &mut Core, a: [f64; 2], b: [f64; 2], shift: bool) {
    core.world.resource_mut::<tt_core::input::KeysHeld>().mods.shift = shift;
    *core.world.resource_mut::<PointerFrame>() =
        PointerFrame { samples: vec![[10.0, a[0], a[1]], [10.2, b[0], b[1]]], hover: Some(b), pressed: Some(10.0), released: Some(10.2), down: false, scale: 1.0, ..Default::default() };
    core.run_pre_ui();
    *core.world.resource_mut::<PointerFrame>() = PointerFrame::default();
    core.world.resource_mut::<tt_core::input::KeysHeld>().mods.shift = false;
}

#[test]
fn the_track_tool_patches_the_selected_tracker_with_auto_masked_looks() {
    let mut app = AppBuilder::new();
    app.add_module(CoreModules).add_module(TrackModule);
    let mut core = app.build();
    // (New trackers wait for a button in the app; these start at once.)
    core.world.resource_mut::<tt_track::NewTrackers>().run = tt_track::TrackRun::Both;
    core.world.resource_mut::<Transport>().frame_count = 300;
    core.world.insert_resource(LookMasker(Some(masked_everywhere)));
    let a = sketch(&mut core, "A", 50..150);
    core.world.resource_mut::<ActiveTool>().0 = Tool::Track;
    core.world.resource_mut::<Transport>().seek(100);
    core.run_pre_ui();

    // Nothing selected: a drag inside the sketch makes a tracker there, its look masked.
    drag(&mut core, [95.0, 95.0], [105.0, 105.0], false);
    let t = core.world.resource::<Selection>().primary().expect("the new tracker is selected");
    assert!(is_tracker(&core.world, t) && guide_of(&core.world, t) == Some(a));
    let first = looks_of(&core.world, t)[0];
    assert!(core.world.get::<Look>(first).expect("look").painted().is_some(), "masked automatically");

    // The tracker selected: a drag on another frame patches it (a look, masked).
    core.world.resource_mut::<Transport>().seek(130);
    core.run_pre_ui();
    drag(&mut core, [96.0, 96.0], [106.0, 106.0], false);
    let looks = looks_of(&core.world, t);
    assert_eq!(looks.len(), 2, "a patch, not a new tracker");
    assert_eq!(live_trackers(&mut core).len(), 1);
    let patch = core.world.get::<Look>(looks[1]).expect("look");
    assert_eq!((patch.frame, patch.center()), (130, [101.0, 101.0]));
    assert!(patch.painted().is_some());

    // Shift: a new tracker instead (on the same guide).
    drag(&mut core, [96.0, 96.0], [106.0, 106.0], true);
    assert_eq!(live_trackers(&mut core).len(), 2);

    // The setting off: looks stay unpainted (centre-weighted).
    core.world.resource_mut::<tt_track::look::LookDefaults>().auto_mask = false;
    let t2 = core.world.resource::<Selection>().primary().expect("selected");
    drag(&mut core, [96.0, 96.0], [106.0, 106.0], false);
    let last = *looks_of(&core.world, t2).last().expect("look");
    assert!(core.world.get::<Look>(last).expect("look").painted().is_none());
}

#[test]
fn the_wheel_zooms_in_the_track_tool_and_ctrl_wheel_sizes_the_click() {
    let mut app = AppBuilder::new();
    app.add_module(CoreModules).add_module(TrackModule);
    let mut core = app.build();
    // (New trackers wait for a button in the app; these start at once.)
    core.world.resource_mut::<tt_track::NewTrackers>().run = tt_track::TrackRun::Both;
    core.world.resource_mut::<ActiveTool>().0 = Tool::Track;
    let brush = |core: &Core| core.world.resource::<TrackTool>().brush;
    let before = brush(&core);
    let turn = |core: &mut Core, ctrl: bool| {
        core.world.resource_mut::<tt_core::input::KeysHeld>().mods.ctrl = ctrl;
        *core.world.resource_mut::<PointerFrame>() = PointerFrame { wheel: 2.0, hover: Some([50.0, 50.0]), scale: 1.0, ..Default::default() };
        core.run_pre_ui();
        core.world.resource::<PointerFrame>().wheel_taken
    };
    assert!(!turn(&mut core, false), "the wheel stays the viewport's zoom");
    assert_eq!(brush(&core), before);
    assert!(turn(&mut core, true), "Ctrl+wheel is the tool's");
    assert!(brush(&core) > before);
}

/// A tracker saved before `fuse` and `matching` existed loads with them at
/// their defaults (both ways on; matching as before).
#[test]
fn a_tracker_saved_before_fuse_and_matching_still_loads() {
    use bevy_ecs::reflect::AppTypeRegistry;
    use bevy_reflect::FromReflect;
    use bevy_reflect::serde::TypedReflectDeserializer;
    use serde::de::DeserializeSeed;
    use tt_track::{Matching, Tracker};

    let mut app = tt_core::AppBuilder::new();
    app.add_module(tt_core::CoreModules).add_module(tt_track::TrackModule);
    let core = app.build();
    let registry = core.world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let registration = registry.get_with_type_path(std::any::type_name::<Tracker>()).expect("registered");
    let old = "(anchor: 600, direction: Both, follow_playhead: false, feature: 0.4, search: 1.5, adapt: 0.25, min_score: 0.6, rendition: Auto, center_on_guide: false)";
    let mut de = ron::Deserializer::from_str(old).expect("ron");
    let value = TypedReflectDeserializer::new(registration, &registry).deserialize(&mut de).expect("an old save deserializes");
    let t = Tracker::from_reflect(value.as_ref()).expect("and converts");
    assert_eq!((t.anchor, t.search), (600, 1.5), "saved values kept");
    assert!(t.fuse);
    assert_eq!(t.matching, Matching::default());
}

/// The timeline menu's "Track N trackers … from frame F": a tracker with a
/// look on F starts again there; one without goes on from where it is; both
/// are asked to track; Pause stops them. One undo step for the re-seed.
#[test]
fn track_from_reseeds_where_a_look_is_and_runs_them() {
    let mut app = AppBuilder::new();
    app.add_module(CoreModules).add_module(TrackModule);
    let mut core = app.build();
    core.world.resource_mut::<Transport>().frame_count = 200;
    let s = sketch(&mut core, "Sketch 1", 0..200);
    let a = tt_track::add_tracker_with_look(&mut core.world, s, Look::new(50, [100.0, 100.0], [8.0, 8.0])).expect("a");
    let b = tt_track::add_tracker_with_look(&mut core.world, s, Look::new(50, [100.0, 100.0], [8.0, 8.0])).expect("b");
    add_look(&mut core.world, a, Look::new(120, [104.0, 100.0], [8.0, 8.0])).expect("a look on 120");
    let (asked, reseeded) = tt_track::track_from(&mut core.world, &[a, b, s], 120, tt_track::TrackRun::Forward);
    assert_eq!((asked, reseeded), (2, 1), "the sketch isn't a tracker; only a has a look on 120");
    assert_eq!(core.world.get::<Tracker>(a).map(|t| t.anchor), Some(120), "a starts again on 120");
    assert_eq!(core.world.get::<Tracker>(b).map(|t| t.anchor), Some(50), "b goes on from its anchor");
    assert!([a, b].iter().all(|t| tt_track::run_of(&core.world, *t) == tt_track::TrackRun::Forward));
    tt_track::track_from(&mut core.world, &[a, b], 120, tt_track::TrackRun::Paused);
    assert!([a, b].iter().all(|t| tt_track::run_of(&core.world, *t) == tt_track::TrackRun::Paused));
    assert_eq!(core.world.resource::<History>().undo_label(), Some("Re-seed trackers"));
    undo(&mut core.world);
    assert_eq!(core.world.get::<Tracker>(a).map(|t| t.anchor), Some(50), "undone");
}

/// A cursor tracker's patterns (on request: "shift + brush … for quickly
/// adding a new shape. call them Patterns … switchable with 1-9 keys"):
/// a brush teaches the pattern picked (the last painted at first), Shift+brush
/// starts a new one on the selected cursor tracker (not a new tracker), keys
/// 1–0 pick one, and brushing again on a frame adds to that pattern's paint there.
#[test]
fn a_cursor_trackers_brush_teaches_patterns() {
    let mut app = AppBuilder::new();
    app.add_module(CoreModules).add_module(TrackModule);
    let mut core = app.build();
    core.world.resource_mut::<tt_track::NewTrackers>().method = tt_track::Method::Cursor;
    core.world.resource_mut::<Transport>().frame_count = 300;
    core.world.resource_mut::<ActiveTool>().0 = Tool::Track;
    let at = |core: &mut Core, f: i64| {
        core.world.resource_mut::<Transport>().seek(f);
        core.run_pre_ui();
    };
    let pattern_on = |core: &mut Core, t: Entity, f: i64| -> Vec<u32> {
        looks_of(&core.world, t).iter().filter_map(|l| core.world.get::<Look>(*l)).filter(|l| l.frame == f).map(|l| l.pattern).collect()
    };

    // Nothing selected: a brush makes a cursor tracker, its paint pattern 1 (0).
    at(&mut core, 100);
    drag(&mut core, [200.0, 200.0], [230.0, 210.0], false);
    let t = core.world.resource::<Selection>().primary().expect("the new tracker is selected");
    assert_eq!(core.world.get::<Tracker>(t).map(|p| (p.method, p.min_score)), Some((tt_track::Method::Cursor, tt_track::CURSOR_MIN_SCORE)));
    assert_eq!(pattern_on(&mut core, t, 100), vec![0]);
    // Another frame: the same pattern.
    at(&mut core, 110);
    drag(&mut core, [300.0, 200.0], [320.0, 220.0], false);
    assert_eq!(pattern_on(&mut core, t, 110), vec![0]);
    // Shift+brush: a new pattern on this tracker, not a new tracker; and it is picked.
    at(&mut core, 120);
    drag(&mut core, [100.0, 100.0], [120.0, 120.0], true);
    assert_eq!(pattern_on(&mut core, t, 120), vec![1]);
    assert_eq!(live_trackers(&mut core), vec![t]);
    at(&mut core, 130);
    drag(&mut core, [150.0, 100.0], [170.0, 120.0], false);
    assert_eq!(pattern_on(&mut core, t, 130), vec![1], "the picked pattern: the new one");
    // Key 1: pattern 1 again; a second brush on the frame adds to its paint there.
    core.world.resource_mut::<PendingActions>().push(Action::Pattern(0));
    at(&mut core, 140);
    drag(&mut core, [150.0, 100.0], [170.0, 120.0], false);
    drag(&mut core, [400.0, 300.0], [420.0, 320.0], false);
    assert_eq!(pattern_on(&mut core, t, 140), vec![0], "one paint, added to");
    assert_eq!(tt_track::look::patterns_of(&core.world, t), vec![0, 1]);
    let names: Vec<String> = looks_of(&core.world, t).iter().filter_map(|l| core.world.get::<Name>(*l)).map(|n| n.to_string()).collect();
    assert_eq!(names, ["Pattern 1 \u{b7} paint 1", "Pattern 1 \u{b7} paint 2", "Pattern 2 \u{b7} paint 1", "Pattern 2 \u{b7} paint 2", "Pattern 1 \u{b7} paint 3"]);
    // Selecting a paint picks its pattern.
    let second = looks_of(&core.world, t)[2];
    core.world.resource_mut::<Selection>().select_only(second);
    at(&mut core, 150);
    drag(&mut core, [150.0, 100.0], [170.0, 120.0], false);
    assert_eq!(pattern_on(&mut core, t, 150), vec![1], "its paint selected: pattern 2, on that tracker");
    assert_eq!(live_trackers(&mut core), vec![t]);
}
