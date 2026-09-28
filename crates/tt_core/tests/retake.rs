//! Retakes: holding on a frame says where the mouse *should* have been there.
//! The frame's point goes exactly there, its neighbours keep theirs, and the
//! region re-derives around the new data (it jumps and grows to include it).
//! Stepping or playing while holding retakes each frame shown.

mod common;

use bevy_ecs::entity::Entity;
use common::*;
use tt_core::input::Action;
use tt_core::transport::Transport;

/// A first pass along a gently wavy line, followed while playing (frames ~100–220).
fn base(d: &mut Driver) -> (Entity, Vec<Option<[f32; 6]>>) {
    let line = |t: f64| [200.0 + 60.0 * t, 300.0 + 3.0 * (9.0 * t).sin()];
    d.core.world.resource_mut::<Transport>().seek(100);
    d.frame(line, PRESS);
    d.frame(line, Input { action: Some(Action::TogglePlay), ..HOLD });
    d.frames(500, line, HOLD);
    d.frame(line, Input { action: Some(Action::TogglePlay), ..UP });
    let s = d.sketches()[0];
    let before = (0..300).map(|f| d.value(s, f)).collect();
    (s, before)
}

fn contains(b: [f32; 6], p: [f32; 2]) -> bool {
    b[2] <= p[0] && p[0] <= b[4] && b[3] <= p[1] && p[1] <= b[5]
}

#[test]
fn a_paused_hold_puts_the_frame_where_the_mouse_was_and_the_region_takes_it_in() {
    let mut d = Driver::new();
    let (s, before) = base(&mut d);
    let old = before[130].unwrap();
    d.core.world.resource_mut::<Transport>().seek(130);
    let spot = still(old[0] as f64 + 60.0, old[1] as f64 - 25.0);
    d.frame(spot, PRESS);
    d.frames(90, spot, HOLD);
    d.frame(spot, UP);

    let now = d.value(s, 130).unwrap();
    assert!((now[0] - old[0] - 60.0).abs() < 1.5 && (now[1] - old[1] + 25.0).abs() < 1.5, "the frame went to the mouse: {now:?} from {old:?}");
    for f in (100..130).chain(131..300).filter(|f| before[*f].is_some()) {
        let (b, a) = (before[f].unwrap(), d.value(s, f as i64).unwrap());
        assert_eq!([a[0], a[1]], [b[0], b[1]], "frame {f} keeps its point");
    }
    let new = [now[0], now[1]];
    assert!(contains(now, new) && contains(now, [before[129].unwrap()[0], before[129].unwrap()[1]]), "the region spans the retake and the path beside it: {now:?}");
    for f in [127, 129, 131, 134] {
        assert!(contains(d.value(s, f).unwrap(), new), "frame {f}'s region grew to take in the retake");
    }
    let far = d.value(s, 160).unwrap();
    assert_eq!(far, before[160].unwrap(), "far away nothing changed");
}

#[test]
fn stepping_while_holding_retakes_frame_by_frame() {
    let mut d = Driver::new();
    let (s, before) = base(&mut d);
    let b = |f: usize| before[f].unwrap();
    let at = |f: usize, dx: f64| still(b(f)[0] as f64 + dx, b(f)[1] as f64);
    d.core.world.resource_mut::<Transport>().seek(130);
    d.frame(at(130, 60.0), PRESS);
    d.frames(40, at(130, 60.0), HOLD);
    // The step and the move to the next spot come together, as with a real hand.
    d.frame(at(131, 30.0), Input { action: Some(Action::StepForward), ..HOLD });
    d.frames(40, at(131, 30.0), HOLD);
    d.frame(at(132, -20.0), Input { action: Some(Action::StepForward), ..HOLD });
    d.frames(40, at(132, -20.0), HOLD);
    d.frame(at(132, -20.0), UP);
    for (f, dx) in [(130usize, 60.0f32), (131, 30.0), (132, -20.0)] {
        let moved = d.value(s, f as i64).unwrap()[0] - b(f)[0];
        assert!((moved - dx).abs() < 1.5, "frame {f} went to its own spot: moved {moved}, wanted {dx}");
    }
    for f in [129usize, 133] {
        assert_eq!(d.value(s, f as i64).unwrap()[0], b(f)[0], "frame {f} keeps its point");
    }
}

#[test]
fn a_quick_drag_retakes_to_where_it_stopped() {
    let mut d = Driver::new();
    let (s, before) = base(&mut d);
    let old = before[130].unwrap();
    d.core.world.resource_mut::<Transport>().seek(130);
    let (t0, x0, y0) = (d.now, old[0] as f64, old[1] as f64);
    let drag = move |t: f64| [x0 + 60.0 * ((t - t0) / 0.1).clamp(0.0, 1.0), y0];
    d.frame(drag, PRESS);
    d.frames(40, drag, HOLD); // 0.1 s of drag, then ~0.13 s still
    d.frame(drag, UP);
    let moved = d.value(s, 130).unwrap()[0] - old[0];
    assert!((moved - 60.0).abs() < 1.5, "moved {moved}");
}

#[test]
fn playing_while_holding_retakes_the_frames_it_passes() {
    let mut d = Driver::new();
    let (s, before) = base(&mut d);
    // Play from 150 at ½× holding 40 px above the path (the hand's lag is 0.25 s).
    d.core.world.resource_mut::<Transport>().seek(150);
    let above = |t: f64| [200.0 + 60.0 * t, 260.0];
    d.frame(above, PRESS);
    d.frame(above, Input { action: Some(Action::TogglePlay), ..HOLD });
    d.frames(200, above, HOLD);
    d.frame(above, Input { action: Some(Action::TogglePlay), ..UP });
    let retaken: Vec<usize> = (140..300).filter(|f| d.value(s, *f as i64).is_some_and(|v| (v[1] - 260.0).abs() < 1.0)).collect();
    assert!(retaken.len() > 20, "{} frames retaken", retaken.len());
    let (lo, hi) = (retaken[0], *retaken.last().unwrap());
    assert!(lo >= 150 && (lo..=hi).all(|f| retaken.contains(&f)), "one run from where playback began: {lo}..={hi}");
    for f in (100..lo).chain(hi + 1..300).filter(|f| before[*f].is_some()) {
        assert_eq!(d.value(s, f as i64).unwrap()[1], before[f].unwrap()[1], "frame {f} outside the retake keeps its point");
    }
}
