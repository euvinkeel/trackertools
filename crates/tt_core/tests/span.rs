//! Lifetimes on the timeline (`tt_core::span`): trimming an entity hides its
//! frames outside the span from everything that reads it, without touching
//! its data; extending brings them back; edits undo; snapping sees the edges.

use bevy_ecs::entity::Entity;
use tt_core::commands::{snap_points, time_span};
use tt_core::history::{self, History};
use tt_core::input::Action;
use tt_core::op::Output;
use tt_core::persist::{load, save};
use tt_core::signal::{Signal, SignalStore};
use tt_core::span::{Edge, Span, begin_drag, end_drag, live_span, move_edge, natural_span, output, set_span, span_of};
use tt_core::view::{ensure_view, map_at};

mod common;
use common::*;

/// A sketch recorded over about frames 0..85 (the hand circling), and its view.
fn sketch_and_view(d: &mut Driver) -> (Entity, Entity) {
    d.frame(circle, PRESS);
    d.frame(circle, Input { action: Some(Action::TogglePlay), ..HOLD });
    d.frames(500, circle, HOLD);
    d.frame(circle, Input { action: Some(Action::TogglePlay), ..HOLD });
    d.frames(60, circle, HOLD);
    d.frame(circle, UP);
    let sketch = d.sketches()[0];
    let view = ensure_view(&mut d.core.world, sketch);
    d.frames(3, still(0.0, 0.0), UP);
    (sketch, view)
}

fn raw(d: &Driver, e: Entity) -> Signal {
    let w = &d.core.world;
    w.resource::<SignalStore>().get(w.get::<Output>(e).expect("output").0).expect("signal").clone()
}

#[test]
fn trimming_hides_frames_downstream_and_keeps_the_data() {
    let mut d = Driver::new();
    let (sketch, view) = sketch_and_view(&mut d);
    let (lo, hi) = natural_span(&d.core.world, sketch).expect("recorded");
    assert!(hi - lo > 60, "a sketch over {lo}..={hi}");
    let before = raw(&d, sketch);
    let view_hull = raw(&d, view).present_hull();
    assert_eq!(view_hull, Some((lo, hi)), "the view covers the sketch");

    // Trim both ends: one undo step each.
    let (a, b) = (lo + 20, hi - 25);
    move_edge(&mut d.core.world, sketch, Edge::First, a);
    move_edge(&mut d.core.world, sketch, Edge::Last, b);
    d.frames(3, still(0.0, 0.0), UP);
    assert_eq!(span_of(&d.core.world, sketch), Span::new(a, b));
    assert!(raw(&d, sketch).same_as(&before), "the sketch's own data is untouched");
    assert_eq!(output(&d.core.world, sketch).and_then(|s| s.present_hull()), Some((a, b)), "readers see only the span");
    assert_eq!(raw(&d, view).present_hull(), Some((a, b)), "the view re-derived from the trimmed sketch");
    assert_eq!(time_span(&d.core.world, sketch), Some((a, b)));
    // Outside its (trimmed) sketch a view holds the nearest framing.
    let w = &d.core.world;
    assert_eq!(map_at(w, Some(view), a - 20), map_at(w, Some(view), a));
    // Snapping sees the new edges (the stroke's own ends, lo and hi, are still there: it isn't trimmed).
    let points = snap_points(&mut d.core.world);
    assert_eq!(points, vec![lo, a, b, hi]);

    // Undo both trims: everything as it was.
    assert_eq!(d.core.world.resource::<History>().undo_label(), Some("Trim Sketch 1"));
    history::undo(&mut d.core.world);
    history::undo(&mut d.core.world);
    d.frames(3, still(0.0, 0.0), UP);
    assert_eq!(span_of(&d.core.world, sketch), Span::default());
    assert!(d.core.world.get::<Span>(sketch).is_none(), "untrimmed: no component");
    assert_eq!(raw(&d, view).present_hull(), view_hull, "the view is whole again");

    // Redo, then extend past the data's end: that side follows the data again.
    history::redo(&mut d.core.world);
    move_edge(&mut d.core.world, sketch, Edge::First, lo - 50);
    d.frames(3, still(0.0, 0.0), UP);
    assert_eq!(span_of(&d.core.world, sketch), Span::default());
    assert_eq!(raw(&d, view).present_hull(), view_hull);
}

#[test]
fn a_drag_is_one_undo_step() {
    let mut d = Driver::new();
    let (sketch, _) = sketch_and_view(&mut d);
    let (lo, hi) = natural_span(&d.core.world, sketch).expect("recorded");
    let steps = d.core.world.resource::<History>().undo_label().map(str::to_string);
    begin_drag(&mut d.core.world, sketch);
    for f in (hi - 40..hi).rev() {
        move_edge(&mut d.core.world, sketch, Edge::Last, f);
        d.frame(still(0.0, 0.0), UP);
    }
    end_drag(&mut d.core.world);
    assert_eq!(live_span(&d.core.world, sketch), Some((lo, hi - 40)));
    assert_eq!(d.core.world.resource::<History>().undo_label(), Some("Trim Sketch 1"));
    history::undo(&mut d.core.world);
    assert_eq!(live_span(&d.core.world, sketch), Some((lo, hi)), "one undo takes the whole drag back");
    assert_eq!(d.core.world.resource::<History>().undo_label().map(str::to_string), steps);
}

#[test]
fn spans_are_saved() {
    let mut d = Driver::new();
    let (sketch, view) = sketch_and_view(&mut d);
    set_span(&mut d.core.world, sketch, Span { first: Some(40), last: None });
    set_span(&mut d.core.world, view, Span { first: None, last: Some(90) });
    let path = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("span_{}.ttproj", std::process::id()));
    let _ = std::fs::remove_file(&path);
    save(&mut d.core.world, &path).expect("saved");
    let mut e = Driver::new();
    load(&mut e.core.world, &path).expect("loaded");
    let _ = std::fs::remove_file(&path);
    let mut spans: Vec<Span> = e.core.world.query::<&Span>().iter(&e.core.world).copied().collect();
    spans.sort_by_key(|s| s.first);
    assert_eq!(spans, vec![Span { first: None, last: Some(90) }, Span { first: Some(40), last: None }]);
}
