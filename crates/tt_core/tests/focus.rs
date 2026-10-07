//! SpringFocus in a running core (tt_core::focus): made on one thing,
//! moved over to another on a later frame (one undo step), followed by a
//! view with Tab, saved and loaded.

mod common;

use bevy_ecs::name::Name;
use bevy_ecs::prelude::*;
use common::{Driver, UP};
use tt_core::focus::{FocusParams, focus_on, make_focus, set_focus};
use tt_core::history::{History, undo};
use tt_core::op::{Inputs, Output};
use tt_core::persist::{load, save};
use tt_core::signal::SignalStore;

/// A box producer (what a tracker is to it) at `at(f)` on frames 0–599.
fn producer(w: &mut World, name: &str, at: impl Fn(i64) -> [f64; 2]) -> Entity {
    let sig = w.resource_mut::<SignalStore>().create(6);
    {
        let mut store = w.resource_mut::<SignalStore>();
        let s = store.get_mut(sig).expect("created");
        for f in 0..600 {
            let [x, y] = at(f);
            s.set(f, &[x, y, x - 10.0, y - 10.0, x + 10.0, y + 10.0].map(|v| v as f32));
        }
    }
    w.spawn((Name::new(name.to_string()), Output(sig))).id()
}

fn x(w: &World, e: Entity, f: i64) -> f64 {
    w.resource::<SignalStore>().get(w.get::<Output>(e).unwrap().0).unwrap().get(f).expect("a value")[0] as f64
}

#[test]
fn a_springfocus_moves_from_one_thing_to_the_next() {
    let mut d = Driver::new();
    let w = &mut d.core.world;
    let still = producer(w, "Still", |_| [500.0, 300.0]);
    let mouse = producer(w, "Mouse", |f| [900.0 + 2.0 * f as f64, 400.0]);
    let focus = make_focus(w, still, 0);
    d.frames(2, |_| [0.0, 0.0], UP);
    assert_eq!(x(&d.core.world, focus, 200), 500.0, "on the still thing");

    // From frame 120, over to the mouse: one second (60 frames at 60 fps).
    focus_on(&mut d.core.world, focus, 120, mouse);
    d.frames(2, |_| [0.0, 0.0], UP);
    let w = &d.core.world;
    assert_eq!(w.resource::<History>().undo_label(), Some("Focus on Mouse"));
    assert_eq!(w.get::<Inputs>(focus).unwrap().0.len(), 2, "both are its inputs");
    assert_eq!(x(w, focus, 119), 500.0);
    let mid = x(w, focus, 150);
    assert!(mid > 500.0 && mid < 900.0 + 300.0, "on its way: {mid}");
    assert!((x(w, focus, 300) - (900.0 + 600.0)).abs() < 1e-3, "then exactly on the mouse");
    // Smooth: no frame jumps more than the move needs.
    let steps: Vec<f64> = (100..300).map(|f| x(w, focus, f + 1) - x(w, focus, f)).collect();
    assert!(steps.iter().all(|s| *s >= -1e-6 && *s < 60.0), "{steps:?}");

    // A slower move: changed in its spring, then undone.
    set_focus(&mut d.core.world, focus, "slower", |p| p.move_time = 3.0);
    d.frames(2, |_| [0.0, 0.0], UP);
    let slow = x(&d.core.world, focus, 200);
    assert!(slow > 500.0 && slow < 1290.0, "a third of the way into a 3 s move: not there yet ({slow})");
    undo(&mut d.core.world);
    d.frames(2, |_| [0.0, 0.0], UP);
    assert!((x(&d.core.world, focus, 200) - 1300.0).abs() < 1e-3);

    // Tab: a view follows it.
    use tt_core::input::Action;
    use tt_core::view::{ActiveView, followed, map_at};
    d.core.world.resource_mut::<tt_core::selection::Selection>().select_only(focus);
    d.frame(|_| [0.0, 0.0], common::Input { action: Some(Action::EnterView), ..UP });
    d.frames(3, |_| [0.0, 0.0], UP);
    let view = d.core.world.resource::<ActiveView>().0.expect("in its view");
    assert_eq!(followed(&d.core.world, view), Some(focus));
    let m = map_at(&d.core.world, Some(view), 300);
    let centre = m.to_source([m.canvas[0] / 2.0, m.canvas[1] / 2.0]);
    assert!((centre[0] - 1500.0).abs() < 2.0, "the view sits on the mouse after the move: {centre:?}");

    // Saved and loaded: its keys (and their targets) come back.
    let path = std::env::temp_dir().join(format!("tt_focus_{}.ttproj", std::process::id()));
    save(&mut d.core.world, &path).expect("saves");
    let mut e = Driver::new();
    load(&mut e.core.world, &path).expect("loads");
    e.frames(2, |_| [0.0, 0.0], UP);
    let w = &mut e.core.world;
    let mut q = w.query::<(Entity, &FocusParams)>();
    let (f2, p) = q.iter(w).map(|(e, p)| (e, p.clone())).next().expect("it came back");
    assert_eq!(p.keys.len(), 2);
    let names: Vec<String> = p.keys.iter().map(|k| w.get::<Name>(k.target).map(|n| n.to_string()).unwrap_or_default()).collect();
    assert_eq!(names, ["Still", "Mouse"]);
    assert!((x(w, f2, 300) - 1500.0).abs() < 1e-3);
    let _ = std::fs::remove_file(path);
}
