//! Derived views (ROADMAP M4): framing, sketching inside views, nesting.
//! The scripted hand sees the sprite as the active view shows it, 250 ms late.

use std::cell::RefCell;
use std::rc::Rc;

use bevy_ecs::entity::Entity;
use tt_core::history;
use tt_core::input::Action;
use tt_core::selection::Selection;
use tt_core::sketch::Through;
use tt_core::transport::Transport;
use tt_core::view::{ActiveView, FrameParams, home_of, map_at, view_of};

mod common;
use common::*;

const FPS: f64 = 60.0;

/// The sprite fixture's centre (source pixels) at video time t.
fn subject(t: f64) -> [f64; 2] {
    [
        (950.0 + 500.0 * (0.9 * t).sin() + 60.0 * (5.3 * t).sin()).floor() + 10.5,
        (530.0 + 300.0 * (1.3 * t + 0.7).sin() + 40.0 * (4.1 * t).sin()).floor() + 10.5,
    ]
}

fn truth(f: i64) -> [f64; 2] {
    subject(f as f64 / FPS)
}

/// Record a sketch of the sprite from `from` for `ui_frames` app frames at ½
/// speed, the hand following what the active view shows. Returns the sketch.
fn sketch_sprite(d: &mut Driver, from: i64, ui_frames: usize, new: bool) -> Entity {
    let view = d.core.world.resource::<ActiveView>().0;
    let shown_at: Vec<[f64; 2]> = (0..600).map(|f| map_at(&d.core.world, view, f).from_source(truth(f))).collect();
    d.core.world.resource_mut::<Transport>().seek(from);
    let seen = Rc::new(RefCell::new(vec![(0.0f64, from)]));
    let hand = {
        let seen = seen.clone();
        move |t: f64| {
            let s = seen.borrow();
            let i = s.partition_point(|(w, _)| *w <= t - 0.25).saturating_sub(1);
            let p = shown_at[s[i].1.clamp(0, 599) as usize];
            let tremor = |ph: f64| 1.5 * ((9.0 * std::f64::consts::TAU * t + ph).sin() * 0.6 + (12.3 * std::f64::consts::TAU * t + 2.0 * ph).sin() * 0.4);
            [p[0] + tremor(0.0), p[1] + tremor(1.3)]
        }
    };
    // The hand is already on the subject when it presses.
    seen.borrow_mut()[0].0 = d.now - 1.0;
    d.frame(&hand, Input { shift: new, ..PRESS });
    d.frame(&hand, Input { action: Some(Action::TogglePlay), ..HOLD });
    for _ in 0..ui_frames {
        let f = d.transport().frame();
        seen.borrow_mut().push((d.now, f));
        d.frame(&hand, HOLD);
    }
    d.frame(&hand, Input { action: Some(Action::TogglePlay), ..HOLD });
    d.frames(60, &hand, HOLD); // hold a beat past the end: the hand catches up
    d.frame(&hand, UP);
    d.core.world.resource::<Selection>().primary().expect("the new sketch is selected")
}

fn enter(d: &mut Driver) -> Entity {
    d.frame(still(0.0, 0.0), Input { action: Some(Action::EnterView), ..UP });
    d.frames(3, still(0.0, 0.0), UP);
    d.core.world.resource::<ActiveView>().0.expect("in a view")
}

/// Share of `frames` on which the sprite sits in the central 30% of `view`.
fn central(d: &Driver, view: Entity, frames: impl Iterator<Item = i64>) -> (f64, usize) {
    let (mut inside, mut n) = (0, 0);
    for f in frames {
        let m = map_at(&d.core.world, Some(view), f);
        let p = m.from_source(truth(f));
        let [w, h] = m.canvas;
        n += 1;
        if (p[0] - w / 2.0).abs() <= 0.15 * w && (p[1] - h / 2.0).abs() <= 0.15 * h {
            inside += 1;
        }
    }
    (inside as f64 / n.max(1) as f64, n)
}

/// Frames where a sketch has a value.
fn covered(d: &Driver, s: Entity) -> Vec<i64> {
    (0..600).filter(|f| d.value(s, *f).is_some()).collect()
}

fn median_error(d: &Driver, s: Entity) -> f64 {
    let mut e: Vec<f64> = covered(d, s).iter().map(|f| {
        let v = d.value(s, *f).unwrap();
        let t = truth(*f);
        (v[0] as f64 - t[0]).hypot(v[1] as f64 - t[1])
    }).collect();
    e.sort_by(f64::total_cmp);
    e[e.len() / 2]
}

#[test]
fn entering_a_view_frames_its_sketch() {
    let mut d = Driver::new();
    let s1 = sketch_sprite(&mut d, 100, 1000, false);
    let v1 = enter(&mut d);
    assert_eq!(view_of(&mut d.core.world, s1), Some(v1));
    let frames = covered(&d, s1);
    let (share, n) = central(&d, v1, frames.iter().copied());
    println!("level 1: {n} frames, sprite in the central 30% of the view on {:.1}%", share * 100.0);
    assert!(n > 150);
    assert!(share >= 0.99, "{:.1}%", share * 100.0);
}

#[test]
fn a_sketch_drawn_inside_a_view_lives_in_source_pixels() {
    let mut d = Driver::new();
    let s1 = sketch_sprite(&mut d, 100, 1000, false);
    let v1 = enter(&mut d);
    let s2 = sketch_sprite(&mut d, 110, 900, true);
    assert_ne!(s1, s2);
    assert_eq!(home_of(&d.core.world, s2), Some(v1), "drawn in view 1");
    let w = &mut d.core.world;
    assert_eq!(w.query::<&Through>().iter(w).count(), 1, "the stroke recorded view 1's framing");
    let (e1, e2) = (median_error(&d, s1), median_error(&d, s2));
    println!("median point error: level 1 {e1:.2} px, level 2 (drawn in view 1) {e2:.2} px (source pixels)");
    assert!(e2 < 1.5, "{e2:.2}");
}

#[test]
fn three_levels_deep_the_subject_stays_central() {
    let mut d = Driver::new();
    sketch_sprite(&mut d, 100, 1000, false);
    enter(&mut d);
    sketch_sprite(&mut d, 110, 900, true);
    enter(&mut d);
    let s3 = sketch_sprite(&mut d, 120, 800, true);
    let v3 = enter(&mut d);
    let chain = tt_core::view::chain(&d.core.world, Some(v3));
    assert_eq!(chain.len(), 3, "three views deep");
    let frames = covered(&d, s3);
    let (share, n) = central(&d, v3, frames.iter().copied());
    let zoom = map_at(&d.core.world, Some(v3), frames[frames.len() / 2]).a;
    println!("level 3: {n} frames, sprite in the central 30% on {:.1}%; the view shows {:.2} source px per view px", share * 100.0, zoom);
    assert!(share >= 0.99, "{:.1}%", share * 100.0);
}

#[test]
fn retuning_a_parent_view_never_moves_a_child_sketch() {
    let mut d = Driver::new();
    sketch_sprite(&mut d, 100, 1000, false);
    let v1 = enter(&mut d);
    let s2 = sketch_sprite(&mut d, 110, 900, true);
    let before: Vec<_> = covered(&d, s2).iter().map(|f| d.value(s2, *f).unwrap()).collect();
    let view_before = map_at(&d.core.world, Some(v1), 150);
    history::edit(&mut d.core.world, "Re-tune", |tx| tx.modify::<FrameParams>(v1, |p| {
        p.pan_damping = 0.6;
        p.fit = 0.3;
    }));
    d.frames(3, still(0.0, 0.0), UP);
    assert_ne!(map_at(&d.core.world, Some(v1), 150), view_before, "the parent view changed");
    let after: Vec<_> = covered(&d, s2).iter().map(|f| d.value(s2, *f).unwrap()).collect();
    assert_eq!(before.len(), after.len());
    for (b, a) in before.iter().zip(&after) {
        assert!((b[0] - a[0]).abs() < 1e-3 && (b[1] - a[1]).abs() < 1e-3, "the child's point stays: {b:?} vs {a:?}");
    }
}

#[test]
fn shift_tab_backs_out_and_undo_hands_back_the_viewport() {
    let mut d = Driver::new();
    let s1 = sketch_sprite(&mut d, 100, 600, false);
    let v1 = enter(&mut d);
    let s2 = sketch_sprite(&mut d, 110, 500, true);
    let v2 = enter(&mut d);
    d.frame(still(0.0, 0.0), Input { action: Some(Action::ExitView), ..UP });
    assert_eq!(d.core.world.resource::<ActiveView>().0, Some(v1), "back to the parent");
    assert_eq!(d.core.world.resource::<Selection>().primary(), Some(s2), "the sketch we left is selected, so Tab goes back in");
    d.frame(still(0.0, 0.0), Input { action: Some(Action::EnterView), ..UP });
    assert_eq!(d.core.world.resource::<ActiveView>().0, Some(v2), "Tab re-enters the same view (no new one)");
    // Undo the view's creation: the viewport falls back to the parent.
    history::undo(&mut d.core.world);
    d.frames(2, still(0.0, 0.0), UP);
    assert_eq!(d.core.world.resource::<ActiveView>().0, Some(v1));
    d.frame(still(0.0, 0.0), Input { action: Some(Action::ExitView), ..UP });
    assert_eq!(d.core.world.resource::<ActiveView>().0, None, "then the source");
    assert_eq!(d.core.world.resource::<Selection>().primary(), Some(s1));
}

#[test]
fn tab_then_hold_nests_a_new_sketch_instead_of_editing_the_parent() {
    let mut d = Driver::new();
    let s1 = sketch_sprite(&mut d, 100, 600, false);
    let v1 = enter(&mut d);
    assert!(d.core.world.resource::<Selection>().entities.is_empty(), "entering a view clears the selection");
    let s2 = sketch_sprite(&mut d, 110, 400, false); // no Shift
    assert_ne!(s2, s1, "a new sketch");
    assert_eq!(home_of(&d.core.world, s2), Some(v1), "nested in the view");
    assert_eq!(d.strokes(s1), 1, "the parent was not edited");
}

#[test]
fn a_jump_while_drawing_in_a_view_records_only_where_it_lands() {
    let mut d = Driver::new();
    sketch_sprite(&mut d, 100, 600, false);
    enter(&mut d);
    d.core.world.resource_mut::<Transport>().seek(120);
    d.frame(still(300.0, 200.0), PRESS);
    d.frames(10, still(300.0, 200.0), HOLD);
    d.frame(still(300.0, 200.0), Input { action: Some(Action::Seek(560)), ..HOLD });
    d.frames(10, still(300.0, 200.0), HOLD);
    let n = d.core.world.resource::<tt_core::capture::LiveCapture>().0.as_ref().unwrap().through.len();
    assert!(n <= 3, "a 440-frame jump recorded {n} framings (only the frames shown count)");
    d.frame(still(300.0, 200.0), UP);
}
