//! Subjects in a running core (tt_core::subject): made from points, kept up
//! to date by the graph, dragged in the Select tool (an offset key, one undo
//! step), saved and loaded.

mod common;

use bevy_ecs::name::Name;
use bevy_ecs::prelude::*;
use common::{Driver, HOLD, PRESS, UP};
use tt_core::history::{History, undo};
use tt_core::op::Output;
use tt_core::persist::{load, save};
use tt_core::signal::SignalStore;
use tt_core::subject::{OffsetKey, Subject, add_members, make_subject, members_of, remove_members, value_at};
use tt_core::tool::{ActiveTool, Tool};
use tt_core::transport::Transport;

/// A box producer (what a sketch or a tracker is to a subject) with a point on the frames `at` gives one.
fn producer(w: &mut World, name: &str, at: impl Fn(i64) -> Option<[f64; 2]>) -> Entity {
    let sig = w.resource_mut::<SignalStore>().create(6);
    {
        let mut store = w.resource_mut::<SignalStore>();
        let s = store.get_mut(sig).expect("created");
        for f in 0..600 {
            if let Some([x, y]) = at(f) {
                s.set(f, &[x, y, x - 10.0, y - 10.0, x + 10.0, y + 10.0].map(|v| v as f32));
            }
        }
    }
    w.spawn((Name::new(name.to_string()), Output(sig))).id()
}

/// Something moving right 2 px a frame; two points on it 40 px apart, the second one only on frames 100–199.
fn two_points(d: &mut Driver) -> (Entity, Entity) {
    let w = &mut d.core.world;
    let a = producer(w, "A", |f| Some([100.0 + 2.0 * f as f64, 300.0]));
    let b = producer(w, "B", |f| (100..200).contains(&f).then_some([140.0 + 2.0 * f as f64, 300.0]));
    (a, b)
}

#[test]
fn a_subject_follows_its_members_motion_without_jumping_when_one_leaves() {
    let mut d = Driver::new();
    let (a, b) = two_points(&mut d);
    let s = make_subject(&mut d.core.world, &[a, b], 150).expect("a subject");
    d.frames(2, |_| [0.0, 0.0], UP);
    let w = &d.core.world;
    assert_eq!(members_of(w, s), vec![a, b]);
    // Made on frame 150, where both are: their mean (120 px right of A's point).
    let x = |f: i64| value_at(w, s, f).expect("a value")[0];
    assert!((x(150) - (100.0 + 300.0 + 20.0)).abs() < 1e-3, "{}", x(150));
    // B leaves after frame 199 and wasn't there before 100: no jump, just A's motion.
    for f in 1..600 {
        assert!((x(f) - x(f - 1) - 2.0).abs() < 1e-3, "frame {f}: {} → {}", x(f - 1), x(f));
    }
}

#[test]
fn members_are_added_and_removed_as_one_undo_step_each() {
    let mut d = Driver::new();
    let (a, b) = two_points(&mut d);
    let s = make_subject(&mut d.core.world, &[a], 0).expect("a subject");
    assert_eq!(add_members(&mut d.core.world, s, &[a, b, s]), 1, "only B is new (A is in it; it can't be in itself)");
    assert_eq!(members_of(&d.core.world, s), vec![a, b]);
    assert_eq!(remove_members(&mut d.core.world, s, &[a]), 1);
    assert_eq!(members_of(&d.core.world, s), vec![b]);
    undo(&mut d.core.world);
    assert_eq!(members_of(&d.core.world, s), vec![a, b]);
}

#[test]
fn dragging_a_subject_keys_its_offset_on_the_shown_frame() {
    let mut d = Driver::new();
    let (a, b) = two_points(&mut d);
    let s = make_subject(&mut d.core.world, &[a, b], 120).expect("a subject");
    d.core.world.resource_mut::<ActiveTool>().0 = Tool::Select;
    d.core.world.resource_mut::<Transport>().seek(120);
    d.frames(2, |_| [0.0, 0.0], UP);
    let before = value_at(&d.core.world, s, 120).expect("a value");
    // Press on its point, drag 30 px right and 5 down, let go.
    let (x0, y0) = (before[0], before[1]);
    let t0 = d.now;
    // (Still for the press's first 20 ms: the press lands where the point is.)
    let path = move |t: f64| {
        let u = ((t - t0 - 0.02) / 0.1).clamp(0.0, 1.0);
        [x0 + 30.0 * u, y0 + 5.0 * u]
    };
    d.frame(path, PRESS);
    d.frames(30, path, HOLD);
    d.frame(path, UP);
    let w = &d.core.world;
    let after = value_at(w, s, 120).expect("a value");
    assert!((after[0] - x0 - 30.0).abs() < 0.01 && (after[1] - y0 - 5.0).abs() < 0.01, "{before:?} → {after:?}");
    // A pin on 120 and, at the reach's edges (0.5 s: 30 frames), keys holding what was there.
    let keys = &w.get::<Subject>(s).expect("subject").offsets;
    assert_eq!(keys.iter().map(|k| k.frame).collect::<Vec<_>>(), vec![90, 120, 150]);
    // Only the frames within the reach moved.
    assert!((value_at(w, s, 300).expect("a value")[0] - (100.0 + 600.0 + 20.0)).abs() < 0.01);
    assert_eq!(w.resource::<History>().undo_label(), Some("Move Subject 1"), "one undo step");
    undo(&mut d.core.world);
    assert!(d.core.world.get::<Subject>(s).expect("subject").offsets.is_empty());
}

#[test]
fn a_subject_and_its_keys_save_and_load() {
    let mut d = Driver::new();
    let (a, b) = two_points(&mut d);
    let s = make_subject(&mut d.core.world, &[a, b], 120).expect("a subject");
    tt_core::subject::set_offset_key(&mut d.core.world, s, OffsetKey { frame: 130, x: 4.0, y: -3.0, angle: 12.5 });
    d.frames(2, |_| [0.0, 0.0], UP);
    let want: Vec<_> = [100, 130, 250].iter().map(|f| value_at(&d.core.world, s, *f).expect("a value")).collect();
    let path = std::env::temp_dir().join(format!("tt_subject_{}.ttproj", std::process::id()));
    save(&mut d.core.world, &path).expect("saves");
    let mut e = Driver::new();
    load(&mut e.core.world, &path).expect("loads");
    e.frames(2, |_| [0.0, 0.0], UP);
    let w = &mut e.core.world;
    let mut q = w.query::<(Entity, &Subject)>();
    let (s2, subject) = q.iter(w).map(|(e, s)| (e, s.clone())).next().expect("the subject came back");
    assert_eq!(subject.offsets, vec![OffsetKey { frame: 130, x: 4.0, y: -3.0, angle: 12.5 }]);
    assert_eq!(members_of(w, s2).len(), 2);
    let got: Vec<_> = [100, 130, 250].iter().map(|f| value_at(w, s2, *f).expect("a value")).collect();
    assert_eq!(got, want);
    let _ = std::fs::remove_file(path);
}

/// Tab on a subject enters a view that keeps it centred, the same as on a sketch.
#[test]
fn tab_follows_a_subject() {
    use tt_core::input::Action;
    use tt_core::selection::Selection;
    use tt_core::view::{ActiveView, followed, map_at};
    let mut d = Driver::new();
    let (a, b) = two_points(&mut d);
    let s = make_subject(&mut d.core.world, &[a, b], 150).expect("a subject");
    d.frames(2, |_| [0.0, 0.0], UP);
    d.core.world.resource_mut::<Selection>().select_only(s);
    d.frame(|_| [0.0, 0.0], common::Input { action: Some(Action::EnterView), ..UP });
    d.frames(3, |_| [0.0, 0.0], UP);
    let view = d.core.world.resource::<ActiveView>().0.expect("in a view");
    assert_eq!(followed(&d.core.world, view), Some(s));
    for f in [20, 150, 400] {
        let want = value_at(&d.core.world, s, f).expect("a value");
        let m = map_at(&d.core.world, Some(view), f);
        let centre = m.to_source([m.canvas[0] / 2.0, m.canvas[1] / 2.0]);
        assert!((centre[0] - want[0]).abs() < 1.0 && (centre[1] - want[1]).abs() < 1.0, "frame {f}: centre {centre:?}, subject {want:?}");
    }
}

/// Dragging a subject inside its own view (the view centred on it): the
/// view holds still while the drag lasts, so the subject moves exactly as
/// far as the pointer did (it used to run away: each step moved the view
/// under the pointer), and the view catches up after.
#[test]
fn dragging_a_subject_in_its_own_view_does_not_run_away() {
    use tt_core::input::Action;
    use tt_core::selection::Selection;
    use tt_core::view::{ActiveView, ViewsHeld, map_at};
    let mut d = Driver::new();
    let (a, b) = two_points(&mut d);
    let s = make_subject(&mut d.core.world, &[a, b], 120).expect("a subject");
    d.core.world.resource_mut::<Transport>().seek(150);
    d.frames(2, |_| [0.0, 0.0], UP);
    d.core.world.resource_mut::<Selection>().select_only(s);
    d.frame(|_| [0.0, 0.0], common::Input { action: Some(Action::EnterView), ..UP });
    d.frames(3, |_| [0.0, 0.0], UP);
    let view = d.core.world.resource::<ActiveView>().0.expect("in its view");
    d.core.world.resource_mut::<ActiveTool>().0 = Tool::Select;
    d.core.world.resource_mut::<Selection>().select_only(s);
    // (The whole path moves: a bend the view's own smoothing would round off isn't what this is about.)
    d.core.world.resource_mut::<tt_core::subject::SubjectReach>().0 = tt_core::subject::REACHES.len() as f32;
    let before = value_at(&d.core.world, s, 150).expect("a value");
    // Where the subject is in the view's pixels: press there, drag 30 px right, slowly.
    let m = map_at(&d.core.world, Some(view), 150);
    let at = m.from_source([before[0], before[1]]);
    let t0 = d.now;
    let path = move |t: f64| {
        let u = ((t - t0 - 0.02) / 0.2).clamp(0.0, 1.0);
        [at[0] + 30.0 * u, at[1]]
    };
    d.frame(path, PRESS);
    d.frames(60, path, HOLD);
    assert!(d.core.world.resource::<ViewsHeld>().0.is_some(), "the views hold while it drags");
    d.frame(path, UP);
    d.frames(3, |_| [0.0, 0.0], UP);
    let after = value_at(&d.core.world, s, 150).expect("a value");
    let moved = (after[0] - before[0]) / m.a;
    assert!((moved - 30.0).abs() < 0.5, "as far as the pointer: {moved} view px");
    assert!(d.core.world.resource::<ViewsHeld>().0.is_none(), "and they let go after");
    let m2 = map_at(&d.core.world, Some(view), 150);
    let centre = m2.to_source([m2.canvas[0] / 2.0, m2.canvas[1] / 2.0]);
    assert!((centre[0] - after[0]).abs() < 1.0, "the view caught up: centred on it again");
}

#[test]
fn a_subject_never_takes_a_member_that_reads_it() {
    let mut d = Driver::new();
    let (a, b) = two_points(&mut d);
    let s = make_subject(&mut d.core.world, &[a], 0).expect("a subject");
    let t = make_subject(&mut d.core.world, &[b, s], 0).expect("a subject of A's subject and B");
    // S in T: T reads S, so T in S would be a cycle (and so would anything reading T).
    assert_eq!(add_members(&mut d.core.world, s, &[t]), 0, "a cycle is refused");
    assert!(!tt_core::subject::can_join(&d.core.world, s, t));
    assert!(tt_core::subject::can_join(&d.core.world, t, a), "A doesn't read T: it can join");
    assert_eq!(add_members(&mut d.core.world, s, &[b, b]), 1, "B once");
    d.frames(2, |_| [0.0, 0.0], UP);
    assert!(d.core.world.get::<tt_core::op::OpError>(s).is_none() && d.core.world.get::<tt_core::op::OpError>(t).is_none());
}

/// Press on the selected subject's path at another frame than the shown one
/// and drag: the path bends there and nowhere beyond the reach; the wheel
/// while dragging narrows the reach; one undo step.
#[test]
fn dragging_its_path_bends_it_there_within_the_reach() {
    use tt_core::selection::Selection;
    use tt_core::subject::SubjectReach;
    let mut d = Driver::new();
    let (a, b) = two_points(&mut d);
    let s = make_subject(&mut d.core.world, &[a, b], 120).expect("a subject");
    d.core.world.resource_mut::<ActiveTool>().0 = Tool::Select;
    d.core.world.resource_mut::<Transport>().seek(120);
    d.core.world.resource_mut::<Selection>().select_only(s);
    d.frames(2, |_| [0.0, 0.0], UP);
    let before: Vec<[f64; 11]> = (0..600).map(|f| value_at(&d.core.world, s, f).expect("a value")).collect();
    // Frame 170's point: 100 px right of the shown frame's, on the path drawn 90 frames either side.
    let (x0, y0) = (before[170][0], before[170][1]);
    let t0 = d.now;
    let path = move |t: f64| {
        let u = ((t - t0 - 0.02) / 0.1).clamp(0.0, 1.0);
        [x0, y0 - 20.0 * u]
    };
    d.frame(path, PRESS);
    d.frames(10, path, HOLD);
    // Three notches down: 0.5 s → 0.1 s (6 frames).
    d.frame(path, common::Input { wheel: -3.0, ..HOLD });
    d.frames(30, path, HOLD);
    d.frame(path, UP);
    d.frames(2, |_| [0.0, 0.0], UP);
    let w = &d.core.world;
    assert_eq!(w.resource::<SubjectReach>().seconds(), Some(0.1), "the reach is kept for the next drag");
    let keys: Vec<i64> = w.get::<Subject>(s).expect("subject").offsets.iter().map(|k| k.frame).collect();
    assert_eq!(keys, vec![164, 170, 176], "a pin where it was taken hold of, the reach's edges around it");
    let after = |f: i64| value_at(w, s, f).expect("a value");
    assert!((after(170)[1] - (y0 - 20.0)).abs() < 0.01 && (after(170)[0] - x0).abs() < 0.01, "through the pointer: {:?}", after(170));
    for f in (0..164).chain(177..600) {
        assert_eq!(after(f), before[f as usize], "frame {f} is beyond the reach");
    }
    // Eased: in between it rises without a corner, never past the pin.
    let dy: Vec<f64> = (164..=176).map(|f| before[f as usize][1] - after(f)[1]).collect();
    assert!(dy.iter().all(|v| (0.0..=20.0 + 1e-6).contains(v)) && dy[1] < 3.0 && dy[5] > 17.0, "{dy:?}");
    assert_eq!(w.resource::<History>().undo_label(), Some("Move Subject 1"));
    undo(&mut d.core.world);
    assert!(d.core.world.get::<Subject>(s).expect("subject").offsets.is_empty());
}

/// The wheel's last notch reaches the whole path: one key, as before reaches existed.
#[test]
fn the_widest_reach_moves_the_whole_path() {
    use tt_core::subject::{REACHES, SubjectReach};
    let mut d = Driver::new();
    let (a, b) = two_points(&mut d);
    let s = make_subject(&mut d.core.world, &[a, b], 120).expect("a subject");
    d.core.world.resource_mut::<ActiveTool>().0 = Tool::Select;
    d.core.world.resource_mut::<Transport>().seek(120);
    d.core.world.resource_mut::<SubjectReach>().0 = REACHES.len() as f32;
    d.frames(2, |_| [0.0, 0.0], UP);
    let v = value_at(&d.core.world, s, 120).expect("a value");
    let t0 = d.now;
    let path = move |t: f64| [v[0] + 30.0 * ((t - t0 - 0.02) / 0.1).clamp(0.0, 1.0), v[1]];
    d.frame(path, PRESS);
    d.frames(30, path, HOLD);
    d.frame(path, UP);
    d.frames(2, |_| [0.0, 0.0], UP);
    let w = &d.core.world;
    assert_eq!(w.get::<Subject>(s).expect("subject").offsets.len(), 1);
    assert!((value_at(w, s, 300).expect("a value")[0] - (100.0 + 600.0 + 20.0 + 30.0)).abs() < 0.01);
}

/// A subject saved before smoothing and easing existed opens as it was: not smoothed, linear between keys.
#[test]
fn a_subject_from_before_smoothing_loads_as_it_was() {
    let mut d = Driver::new();
    let s: Subject = common::load_component(&mut d.core.world, "(anchor: 5, turn: true, half: (48.0, 48.0), offsets: [(frame: 5, x: 1.0, y: 2.0, angle: 0.0)])");
    assert_eq!((s.smooth, s.ease), (0.0, tt_core::subject::Ease::Linear));
    assert_eq!(Subject::default().ease, tt_core::subject::Ease::Smooth, "new subjects ease");
}

/// A click on the selected subject's path keeps it selected (it used to
/// clear the selection: nothing else is under it); Alt+click on a pin takes it out.
#[test]
fn a_click_on_its_path_keeps_it_and_alt_click_removes_a_pin() {
    use tt_core::selection::Selection;
    let mut d = Driver::new();
    let (a, b) = two_points(&mut d);
    let s = make_subject(&mut d.core.world, &[a, b], 120).expect("a subject");
    tt_core::subject::set_offset_key(&mut d.core.world, s, OffsetKey { frame: 160, x: 0.0, y: 15.0, angle: 0.0 });
    d.core.world.resource_mut::<ActiveTool>().0 = Tool::Select;
    d.core.world.resource_mut::<Transport>().seek(120);
    d.core.world.resource_mut::<Selection>().select_only(s);
    d.frames(2, |_| [0.0, 0.0], UP);
    let v = value_at(&d.core.world, s, 140).expect("a value");
    let at = move |_: f64| [v[0], v[1]];
    d.frame(at, PRESS);
    d.frame(at, UP);
    assert_eq!(d.core.world.resource::<Selection>().primary(), Some(s), "still selected");
    let pin = value_at(&d.core.world, s, 160).expect("a value");
    let at = move |_: f64| [pin[0] + 2.0, pin[1]];
    d.frame(at, common::Input { alt: true, ..PRESS });
    d.frame(at, common::Input { alt: true, ..UP });
    assert!(d.core.world.get::<Subject>(s).expect("subject").offsets.is_empty(), "the pin is gone");
    assert_eq!(d.core.world.resource::<History>().undo_label(), Some("Unkey Subject 1"));
}

/// Ctrl+Z in the middle of a drag undoes the drag so far and ends it: moving
/// on until the release changes nothing (it used to go on keying, an undo step a frame).
#[test]
fn undo_during_a_drag_ends_it() {
    use tt_core::input::Action;
    let mut d = Driver::new();
    let (a, b) = two_points(&mut d);
    let s = make_subject(&mut d.core.world, &[a, b], 120).expect("a subject");
    d.core.world.resource_mut::<ActiveTool>().0 = Tool::Select;
    d.core.world.resource_mut::<Transport>().seek(120);
    d.frames(2, |_| [0.0, 0.0], UP);
    let v = value_at(&d.core.world, s, 120).expect("a value");
    let t0 = d.now;
    let path = move |t: f64| [v[0] + 300.0 * ((t - t0 - 0.02) / 0.3).clamp(0.0, 1.0), v[1]];
    d.frame(path, PRESS);
    d.frames(20, path, HOLD);
    d.frame(path, common::Input { action: Some(Action::Undo), ..HOLD });
    d.frames(20, path, HOLD);
    d.frame(path, UP);
    let w = &d.core.world;
    assert!(w.get::<Subject>(s).expect("subject").offsets.is_empty(), "no keys after the undo");
    assert_eq!(w.resource::<History>().undo_label(), Some("Make Subject 1"), "no undo steps from after it");
    assert!(!w.resource::<History>().in_gesture());
}

/// A key's edit while a drag goes on (here: marking the in point) is its own
/// undo step, not part of the drag's.
#[test]
fn an_edit_during_a_drag_is_its_own_undo_step() {
    use tt_core::input::Action;
    let mut d = Driver::new();
    let (a, b) = two_points(&mut d);
    let s = make_subject(&mut d.core.world, &[a, b], 120).expect("a subject");
    d.core.world.resource_mut::<ActiveTool>().0 = Tool::Select;
    d.core.world.resource_mut::<Transport>().seek(120);
    d.frames(2, |_| [0.0, 0.0], UP);
    let v = value_at(&d.core.world, s, 120).expect("a value");
    let t0 = d.now;
    let path = move |t: f64| [v[0] + 30.0 * ((t - t0 - 0.02) / 0.1).clamp(0.0, 1.0), v[1]];
    d.frame(path, PRESS);
    d.frames(10, path, HOLD);
    d.frame(path, common::Input { action: Some(Action::MarkIn), ..HOLD });
    d.frames(10, path, HOLD);
    d.frame(path, UP);
    // The drag after the key, the key, the drag before it: each its own step.
    let moved = |w: &World| !w.get::<Subject>(s).expect("subject").offsets.is_empty();
    assert_eq!(undo(&mut d.core.world).as_deref(), Some("Move Subject 1"));
    assert!(moved(&d.core.world) && tt_core::marks::marks(&d.core.world).mark_in == Some(120), "the in point stays");
    undo(&mut d.core.world);
    assert!(moved(&d.core.world) && tt_core::marks::marks(&d.core.world).mark_in.is_none(), "the in point alone");
    assert_eq!(undo(&mut d.core.world).as_deref(), Some("Move Subject 1"));
    assert!(!moved(&d.core.world));
}

/// A new subject never takes the name of one still there (they were named by
/// how many there were: after a delete, the next repeated a live one's name).
#[test]
fn new_subjects_never_repeat_a_live_name() {
    let mut d = Driver::new();
    let (a, b) = two_points(&mut d);
    let s1 = make_subject(&mut d.core.world, &[a], 0).expect("a subject");
    let s2 = make_subject(&mut d.core.world, &[b], 0).expect("a subject");
    tt_core::commands::delete(&mut d.core.world, &[s1]);
    let s3 = make_subject(&mut d.core.world, &[a, b], 0).expect("a subject");
    let name = |e| d.core.world.get::<Name>(e).expect("named").to_string();
    assert_eq!((name(s2), name(s3)), ("Subject 2".to_string(), "Subject 3".to_string()));
}

/// H on a sketch in a subject: from the playhead on it carries the subject
/// no more (the subject goes on with its other member), the sketch's own
/// data stays as it was, and undo brings it back. Off keys are saved under
/// the name they had when only trackers had them, so older projects keep theirs.
#[test]
fn a_sketch_switched_off_stops_carrying_its_subject_there() {
    use bevy_reflect::TypePath;
    use common::circle;
    use tt_core::input::Action;
    use tt_core::selection::Selection;
    let mut d = Driver::new();
    d.frame(circle, PRESS);
    d.frame(circle, common::Input { action: Some(Action::TogglePlay), ..HOLD });
    d.frames(500, circle, HOLD);
    d.frame(circle, common::Input { action: Some(Action::TogglePlay), ..HOLD });
    d.frames(60, circle, HOLD);
    d.frame(circle, UP);
    let sketch = d.sketches()[0];
    let (lo, hi) = tt_core::span::natural_span(&d.core.world, sketch).expect("recorded");
    let (a, _) = two_points(&mut d);
    let s = make_subject(&mut d.core.world, &[a, sketch], lo).expect("a subject");
    d.frames(2, |_| [0.0, 0.0], UP);
    let sketch_before = d.value(sketch, hi);
    let mid = (lo + hi) / 2;
    d.core.world.resource_mut::<Transport>().seek(mid);
    d.core.world.resource_mut::<Selection>().select_only(sketch);
    d.frame(|_| [0.0, 0.0], common::Input { action: Some(Action::SwitchTracker), ..UP });
    d.frames(2, |_| [0.0, 0.0], UP);
    let x = |w: &World, f: i64| value_at(w, s, f).expect("a value");
    for f in mid + 1..=hi {
        let (p, q) = (x(&d.core.world, f - 1), x(&d.core.world, f));
        assert!((q[0] - p[0] - 2.0).abs() < 1e-3 && (q[1] - p[1]).abs() < 1e-3, "frame {f}: only A's motion: {p:?} -> {q:?}");
    }
    assert_eq!(d.value(sketch, hi), sketch_before, "the sketch's own data is as it was");
    assert!(tt_core::off::is_off(&d.core.world, sketch, hi) && !tt_core::off::is_off(&d.core.world, sketch, mid - 1));
    undo(&mut d.core.world);
    d.frames(2, |_| [0.0, 0.0], UP);
    assert!(!tt_core::off::is_off(&d.core.world, sketch, hi), "undone");
    let moved = (mid + 1..=hi).any(|f| (x(&d.core.world, f)[1] - x(&d.core.world, f - 1)[1]).abs() > 1e-3);
    assert!(moved, "the sketch carries it again (its circle moves it up and down)");
    assert_eq!(tt_core::off::Off::type_path(), "tt_track::off::TrackerOff", "saved as before");
}

/// A tracker's red frames (lost, outside its sketch's box) still carry its
/// subject; only frames where it is switched off leave it out. Exports' points
/// leave out both.
#[test]
fn red_frames_still_carry_a_subject_and_only_off_frames_leave_it_out() {
    use tt_core::subject::points_of;
    let mut d = Driver::new();
    let w = &mut d.core.world;
    // A still at x = 100; B moving 3 px a frame from x = 400, lost (flag 1) on 100–149, off (flag 4) on 200–249.
    let track = |w: &mut World, name: &str, at: &dyn Fn(i64) -> ([f64; 2], f32)| {
        let sig = w.resource_mut::<SignalStore>().create(8);
        {
            let mut store = w.resource_mut::<SignalStore>();
            let s = store.get_mut(sig).expect("created");
            for f in 0..300 {
                let ([x, y], flags) = at(f);
                s.set(f, &[x as f32, y as f32, x as f32 - 5.0, y as f32 - 5.0, x as f32 + 5.0, y as f32 + 5.0, 0.9, flags]);
            }
        }
        w.spawn((Name::new(name.to_string()), Output(sig))).id()
    };
    let a = track(w, "A", &|_| ([100.0, 300.0], 0.0));
    let flags = |f: i64| if (100..150).contains(&f) { 1.0 } else if (200..250).contains(&f) { 4.0 } else { 0.0 };
    let b = track(w, "B", &|f| ([400.0 + 3.0 * f as f64, 300.0], flags(f)));
    let s = make_subject(&mut d.core.world, &[a, b], 0).expect("a subject");
    d.frames(2, |_| [0.0, 0.0], UP);
    let w = &d.core.world;
    let step = |f: i64| value_at(w, s, f).expect("a value")[0] - value_at(w, s, f - 1).expect("a value")[0];
    // Two members, one moving 3 px a frame: the subject moves their mean, 1.5 px a frame, lost frames too.
    for f in (1..100).chain(101..150).chain(151..200) {
        assert!((step(f) - 1.5).abs() < 1e-3, "frame {f}: {}", step(f));
    }
    // Switched off: only A carries it there, and A stands still.
    for f in 201..250 {
        assert!(step(f).abs() < 1e-3, "frame {f}, B off: {}", step(f));
    }
    assert_eq!(points_of(w, b, true).len(), 250, "a subject's: all but the 50 off frames");
    assert_eq!(points_of(w, b, false).len(), 200, "an export's: neither lost nor off");
}
