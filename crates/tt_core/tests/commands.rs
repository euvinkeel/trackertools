//! Commands on the selection: delete (with what belongs), duplicate, rename,
//! select all; each one undo step.

use bevy_ecs::entity::Entity;
use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::name::Name;
use tt_core::commands::{delete, duplicate, rename, strokes_of};
use tt_core::history::{self, History};
use tt_core::input::Action;
use tt_core::selection::Selection;
use tt_core::sketch::SketchParams;
use tt_core::transport::Transport;
use tt_core::view::{ActiveView, view_of};

mod common;
use common::*;

fn line(t: f64) -> [f64; 2] {
    [200.0 + 60.0 * t, 300.0 + 10.0 * (3.0 * t).sin()]
}

/// A sketch recorded while playing from frame 100 (about 60 frames).
fn record(d: &mut Driver, new: bool) -> Entity {
    d.core.world.resource_mut::<Transport>().seek(100);
    d.frame(line, Input { shift: new, ..PRESS });
    d.frame(line, Input { action: Some(Action::TogglePlay), ..HOLD });
    d.frames(350, line, HOLD);
    d.frame(line, Input { action: Some(Action::TogglePlay), ..HOLD });
    d.frames(60, line, HOLD);
    d.frame(line, UP);
    d.core.world.resource::<Selection>().primary().unwrap()
}

/// A one-frame edit: hold 30 px off the path at frame 130.
fn edit_at_130(d: &mut Driver, s: Entity) {
    d.core.world.resource_mut::<Selection>().select_only(s);
    d.core.world.resource_mut::<Transport>().seek(130);
    let v = d.value(s, 130).unwrap();
    let p = still(v[0] as f64 + 30.0, v[1] as f64);
    d.frame(p, PRESS);
    d.frames(60, p, HOLD);
    d.frame(p, UP);
}

fn values(d: &Driver, s: Entity) -> Vec<Option<[f32; 6]>> {
    (0..300).map(|f| d.value(s, f)).collect()
}

fn disabled(d: &Driver, e: Entity) -> bool {
    d.core.world.get::<Disabled>(e).is_some()
}

#[test]
fn deleting_a_sketch_takes_its_strokes_and_view_and_undo_brings_them_back() {
    let mut d = Driver::new();
    let s = record(&mut d, false);
    edit_at_130(&mut d, s);
    let strokes = strokes_of(&d.core.world, s);
    assert_eq!(strokes.len(), 2);
    d.core.world.resource_mut::<Selection>().select_only(s);
    d.frame(still(0.0, 0.0), Input { action: Some(Action::EnterView), ..UP });
    let view = view_of(&mut d.core.world, s).expect("a view");
    d.frame(still(0.0, 0.0), Input { action: Some(Action::ExitView), ..UP });
    let before = values(&d, s);

    d.core.world.resource_mut::<Selection>().select_only(s);
    d.frame(still(0.0, 0.0), Input { action: Some(Action::Delete), ..UP });
    assert!(disabled(&d, s) && strokes.iter().all(|c| disabled(&d, *c)) && disabled(&d, view), "the sketch, its strokes and its view went");
    assert_eq!(d.core.world.resource::<History>().undo_label(), Some("Delete Sketch 1"));
    assert!(d.core.world.resource::<Selection>().entities.is_empty(), "the selection lets go of deleted entities");

    history::undo(&mut d.core.world);
    d.frames(2, still(0.0, 0.0), UP);
    assert!(!disabled(&d, s) && strokes.iter().all(|c| !disabled(&d, *c)) && !disabled(&d, view));
    assert_eq!(values(&d, s), before, "undo restores the sketch exactly");
}

#[test]
fn deleting_a_stroke_re_derives_its_sketch_without_it() {
    let mut d = Driver::new();
    let s = record(&mut d, false);
    let original = values(&d, s);
    edit_at_130(&mut d, s);
    assert_ne!(values(&d, s), original, "the edit changed the path");
    let edit = strokes_of(&d.core.world, s)[1];
    assert_eq!(delete(&mut d.core.world, &[edit]), 1);
    d.frames(2, still(0.0, 0.0), UP);
    assert!(!disabled(&d, s), "the sketch stays");
    assert_eq!(strokes_of(&d.core.world, s).len(), 1);
    assert_eq!(values(&d, s), original, "the path is as it was before the edit");
    history::undo(&mut d.core.world);
    d.frames(2, still(0.0, 0.0), UP);
    assert_eq!(strokes_of(&d.core.world, s).len(), 2, "undo puts the stroke back");
}

#[test]
fn duplicate_copies_a_sketch_with_its_strokes_independently() {
    let mut d = Driver::new();
    let s = record(&mut d, false);
    edit_at_130(&mut d, s);
    let copies = duplicate(&mut d.core.world, &[s]);
    d.frames(2, still(0.0, 0.0), UP);
    assert_eq!(copies.len(), 1);
    let c = copies[0];
    assert_eq!(d.core.world.get::<Name>(c).map(|n| n.to_string()).as_deref(), Some("Sketch 1 copy"));
    assert_eq!(strokes_of(&d.core.world, c).len(), 2);
    assert_eq!(values(&d, c), values(&d, s), "the copy has the same path");
    assert_eq!(d.core.world.resource::<Selection>().entities, vec![c], "the copy is selected");
    // Re-tuning the copy leaves the original alone.
    let original = values(&d, s);
    history::edit(&mut d.core.world, "Re-tune", |tx| tx.modify::<SketchParams>(c, |p| p.steadiness = 4.0));
    d.frames(2, still(0.0, 0.0), UP);
    assert_ne!(values(&d, c), original);
    assert_eq!(values(&d, s), original);
    // A second copy gets its own name; undo takes copies away.
    let again = duplicate(&mut d.core.world, &[s]);
    assert_eq!(d.core.world.get::<Name>(again[0]).map(|n| n.to_string()).as_deref(), Some("Sketch 1 copy 2"));
    history::undo(&mut d.core.world);
    assert!(disabled(&d, again[0]));
}

#[test]
fn a_nested_sketch_survives_its_parent_being_deleted() {
    let mut d = Driver::new();
    let parent = record(&mut d, false);
    d.core.world.resource_mut::<Selection>().select_only(parent);
    d.frame(still(0.0, 0.0), Input { action: Some(Action::EnterView), ..UP });
    d.frames(2, still(0.0, 0.0), UP);
    let nested = record(&mut d, true);
    let before = values(&d, nested);
    assert!(before.iter().flatten().count() > 40);
    delete(&mut d.core.world, &[parent]);
    d.frames(3, still(0.0, 0.0), UP);
    assert_eq!(d.core.world.resource::<ActiveView>().0, None, "the viewport fell back to the source");
    let after = values(&d, nested);
    for (b, a) in before.iter().zip(&after) {
        if let (Some(b), Some(a)) = (b, a) {
            assert!((b[0] - a[0]).abs() < 1e-3 && (b[1] - a[1]).abs() < 1e-3, "the nested sketch's points stay");
        }
    }
    assert_eq!(before.iter().flatten().count(), after.iter().flatten().count());
}

#[test]
fn select_all_rename_and_duplicate_keys() {
    let mut d = Driver::new();
    let a = record(&mut d, false);
    let b = record(&mut d, true);
    d.frame(still(0.0, 0.0), Input { action: Some(Action::SelectAll), ..UP });
    let mut sel = d.core.world.resource::<Selection>().entities.clone();
    sel.sort();
    let mut both = vec![a, b];
    both.sort();
    assert_eq!(sel, both);
    d.frame(still(0.0, 0.0), Input { action: Some(Action::Duplicate), ..UP });
    assert_eq!(d.sketches().len(), 4, "both duplicated in one step");
    assert_eq!(d.core.world.resource::<History>().undo_label(), Some("Duplicate"));
    assert!(rename(&mut d.core.world, a, "Face"));
    assert_eq!(d.core.world.get::<Name>(a).map(|n| n.to_string()).as_deref(), Some("Face"));
    assert!(!rename(&mut d.core.world, a, "  "), "an empty name is refused");
    history::undo(&mut d.core.world);
    assert_eq!(d.core.world.get::<Name>(a).map(|n| n.to_string()).as_deref(), Some("Sketch 1"));
}
