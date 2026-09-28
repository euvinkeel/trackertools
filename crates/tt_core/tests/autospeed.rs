//! Anticipatory speed (DESIGN §8.4), driven the way the app drives the tool:
//! one PointerFrame per app frame at 175 Hz with 1 kHz samples, 1 canvas
//! pixel = 1 screen point.

use std::cell::{Cell, RefCell};
use std::f64::consts::TAU;
use std::rc::Rc;

use bevy_ecs::entity::Entity;
use tt_core::autospeed::{AutoSpeed, AutoSpeedState};
use tt_core::input::Action;
use tt_core::selection::Selection;
use tt_core::transport::Transport;

mod common;
use common::*;

/// A driver with auto speed on, at `rate` (the manual rate), with room to play.
fn auto_driver(rate: f64) -> Driver {
    let mut d = Driver::new();
    d.core.world.resource_mut::<AutoSpeed>().enabled = true;
    let mut t = d.core.world.resource_mut::<Transport>();
    t.frame_count = 20_000;
    t.rate = rate;
    d
}

fn rate(d: &Driver) -> f64 {
    d.transport().rate
}

/// Press, then Space: recording while playing.
fn start(d: &mut Driver, hand: impl Fn(f64) -> [f64; 2] + Copy, from: i64, new: bool) {
    d.core.world.resource_mut::<Transport>().seek(from);
    d.frame(hand, Input { shift: new, ..PRESS });
    d.frame(hand, Input { action: Some(Action::TogglePlay), ..HOLD });
}

/// A hand still on (400, 300) until `t0`, then racing right at 1500 pt/s with a ±30 pt jiggle.
fn goes_wild(t0: f64) -> impl Fn(f64) -> [f64; 2] + Copy {
    move |t| {
        let calm = still(400.0, 300.0)(t);
        if t < t0 {
            return calm;
        }
        let u = t - t0;
        [calm[0] + 1500.0 * u + 30.0 * (TAU * 5.0 * u).sin(), calm[1] + 30.0 * (TAU * 4.3 * u).cos()]
    }
}

#[test]
fn a_hand_that_suddenly_speeds_up_and_jiggles_slows_playback_at_once() {
    let mut d = auto_driver(1.0);
    let t0 = d.now + 2.0;
    let hand = goes_wild(t0);
    start(&mut d, hand, 50, true);
    while d.now < t0 {
        d.frame(hand, HOLD);
    }
    let calm_rate = rate(&d);
    let mut below = None;
    let mut lowest = f64::INFINITY;
    while d.now < t0 + 1.0 {
        d.frame(hand, HOLD);
        lowest = lowest.min(rate(&d));
        if below.is_none() && rate(&d) < 0.5 {
            below = Some(d.now - t0);
        }
    }
    let reason = d.core.world.resource::<AutoSpeedState>().reason;
    println!("calm: ×{calm_rate:.2}; after the hand went wild: below ×0.5 in {:.3} s, down to ×{lowest:.2} ({reason})", below.unwrap_or(f64::NAN));
    assert!(calm_rate > 1.0, "a calm hand had let it speed up: ×{calm_rate:.2}");
    assert!(below.is_some_and(|s| s <= 0.3), "below ×0.5 within 0.3 s: {below:?}");
    d.frame(hand, UP);
}

#[test]
fn a_still_hand_lets_playback_climb_toward_the_fastest() {
    let mut d = auto_driver(0.25);
    let hand = still(400.0, 300.0);
    start(&mut d, hand, 50, true);
    let t0 = d.now;
    let mut at_half_second = None;
    while d.now < t0 + 4.0 {
        d.frame(hand, HOLD);
        if at_half_second.is_none() && d.now >= t0 + 0.5 {
            at_half_second = Some(rate(&d));
        }
    }
    let fastest = d.core.world.resource::<AutoSpeed>().fastest as f64;
    println!("still hand from ×0.25: ×{:.2} after 0.5 s, ×{:.2} after 4 s (fastest ×{fastest})", at_half_second.unwrap(), rate(&d));
    assert!(at_half_second.unwrap() < 1.0, "it climbs slowly");
    assert!(rate(&d) > 0.9 * fastest, "×{:.2}", rate(&d));
    assert_eq!(d.core.world.resource::<AutoSpeedState>().reason, "calm");
    d.frame(hand, UP);
}

#[test]
fn the_release_restores_the_manual_rate() {
    for cancel in [false, true] {
        let mut d = auto_driver(0.5);
        let t0 = d.now + 0.5;
        let hand = goes_wild(t0);
        start(&mut d, hand, 50, true);
        d.frames(300, hand, HOLD);
        assert!((rate(&d) - 0.5).abs() > 0.1, "auto speed moved the rate: ×{:.2}", rate(&d));
        if cancel {
            d.frame(hand, Input { action: Some(Action::Cancel), ..HOLD });
        }
        d.frame(hand, UP);
        assert_eq!(rate(&d), 0.5, "back to the rate you had set (cancelled: {cancel})");
        assert!(!d.core.world.resource::<AutoSpeedState>().acting());
        d.frames(10, hand, UP);
        assert_eq!(rate(&d), 0.5, "and it stays");
    }
}

#[test]
fn q_or_e_during_a_stroke_hands_the_rate_back_until_the_release() {
    let mut d = auto_driver(1.0);
    let t0 = d.now + 2.0;
    let hand = goes_wild(t0);
    start(&mut d, hand, 50, true);
    d.frames(250, hand, HOLD); // calm: it speeds up past 1×
    let auto = rate(&d);
    assert!(auto > 1.0, "×{auto:.2}");
    d.frame(hand, Input { action: Some(Action::SlowerPlayback), ..HOLD });
    assert_eq!(rate(&d), 1.0, "Q steps down from the auto rate ×{auto:.2}");
    assert!(!d.core.world.resource::<AutoSpeedState>().acting());
    while d.now < t0 + 1.0 {
        d.frame(hand, HOLD); // the hand races: auto speed would brake hard
        assert_eq!(rate(&d), 1.0, "yours for the rest of the stroke");
    }
    d.frame(hand, UP);
    assert_eq!(rate(&d), 1.0, "the release keeps your choice");
    // The next stroke is driven again.
    let hand = still(400.0, 300.0);
    start(&mut d, hand, 200, true);
    d.frames(200, hand, HOLD);
    assert!(d.core.world.resource::<AutoSpeedState>().acting() && rate(&d) > 1.1, "×{:.2}", rate(&d));
    d.frame(hand, UP);
    assert_eq!(rate(&d), 1.0);
}

/// Record a stroke: the hand follows `subject` (source pixels at a frame) as
/// it was on screen 0.25 s earlier, with tremor; Space taps to play for
/// `ui_frames` app frames, then pause and release. `each` sees the driver
/// after every app frame. Returns the sketch.
fn follow(d: &mut Driver, subject: impl Fn(f64) -> [f64; 2] + 'static, from: i64, new: bool, ui_frames: usize, mut each: impl FnMut(&Driver)) -> Entity {
    d.core.world.resource_mut::<Transport>().seek(from);
    let shown = Rc::new(RefCell::new(vec![(d.now - 1.0, from as f64), (d.now, from as f64)]));
    let hand = {
        let shown = shown.clone();
        move |t: f64| {
            let s = shown.borrow();
            let w = t - 0.25;
            let i = s.partition_point(|(t, _)| *t <= w).clamp(1, s.len() - 1);
            let ((t0, p0), (t1, p1)) = (s[i - 1], s[i]);
            let playhead = if w >= t1 { p1 } else { p0 + (p1 - p0) * ((w - t0) / (t1 - t0)).clamp(0.0, 1.0) };
            let p = subject(playhead.floor());
            let tremor = |ph: f64| 1.5 * ((9.0 * TAU * t + ph).sin() * 0.6 + (12.3 * TAU * t + 2.0 * ph).sin() * 0.4);
            [p[0] + tremor(0.0), p[1] + tremor(1.3)]
        }
    };
    let mut step = |d: &mut Driver, input: Input| {
        d.frame(&hand, input);
        shown.borrow_mut().push((d.now, d.transport().playhead));
        each(d);
    };
    step(d, Input { shift: new, ..PRESS });
    step(d, Input { action: Some(Action::TogglePlay), ..HOLD });
    for _ in 0..ui_frames {
        step(d, HOLD);
    }
    step(d, Input { action: Some(Action::TogglePlay), ..HOLD });
    for _ in 0..60 {
        step(d, HOLD);
    }
    step(d, UP);
    d.core.world.resource::<Selection>().primary().expect("the sketch is selected")
}

/// Slow drift, and a fast stretch over frames 300–330 (20 px per frame).
fn dash(f: f64) -> [f64; 2] {
    let x = 400.0 + 0.5 * f + 20.0 * (f - 300.0).clamp(0.0, 30.0);
    [x, 400.0]
}

#[test]
fn a_fast_stretch_ahead_in_the_sketch_being_edited_slows_playback_before_it_arrives() {
    let mut d = Driver::new();
    d.core.world.resource_mut::<Transport>().frame_count = 20_000;
    // The sketch, recorded at ½× without auto speed.
    let s = follow(&mut d, dash, 150, true, 1500, |_| {});
    assert!(d.value(s, 290).is_some() && d.value(s, 340).is_some(), "the sketch covers the dash");
    // Editing it from frame 200 at 1×, auto speed on; the hand follows the same subject.
    d.core.world.resource_mut::<AutoSpeed>().enabled = true;
    d.core.world.resource_mut::<Transport>().rate = 1.0;
    // (playhead, rate, what limits it) after every app frame.
    type Log = Vec<(f64, f64, &'static str)>;
    let log: Rc<RefCell<Log>> = Rc::default();
    let rec = log.clone();
    let s2 = follow(&mut d, dash, 200, false, 900, move |d| {
        rec.borrow_mut().push((d.transport().playhead, d.transport().rate, d.core.world.resource::<AutoSpeedState>().reason));
    });
    assert_eq!(s2, s, "the stroke edited the sketch");
    let log = log.borrow();
    let at = |f: f64| log.iter().find(|(p, _, _)| *p >= f).copied().expect("reached");
    let early = log.iter().filter(|(p, _, _)| (210.0..250.0).contains(p)).map(|(_, r, _)| *r).fold(0.0, f64::max);
    let (_, r295, why) = at(295.0);
    let braking = log.iter().filter(|(p, _, _)| (250.0..300.0).contains(p)).count() as f64 / UI_HZ;
    println!(
        "rate while the dash is still far ahead ×{early:.2}; at frame 295 (5 frames before it) ×{r295:.2} ({why}); at 300 ×{:.2}; frames 250–300 took {braking:.2} s",
        at(300.0).1
    );
    assert!(early > 1.0, "not slowed while the dash is beyond the look-ahead: ×{early:.2}");
    assert!(r295 < 0.5, "slowed before the dash arrives: ×{r295:.2}");
    assert_eq!(why, "ahead", "because of what is ahead");
}

/// The sprite fixture's centre (source pixels) at a frame (as in tests/sketch.rs).
fn sprite(f: f64) -> [f64; 2] {
    let t = f / 60.0;
    [
        (950.0 + 500.0 * (0.9 * t).sin() + 60.0 * (5.3 * t).sin()).floor() + 10.5,
        (530.0 + 300.0 * (1.3 * t + 0.7).sin() + 40.0 * (4.1 * t).sin()).floor() + 10.5,
    ]
}

#[test]
fn a_sketch_recorded_under_auto_speed_is_still_accurate() {
    let mut d = auto_driver(0.5);
    let (lo, hi) = (Rc::new(Cell::new(f64::INFINITY)), Rc::new(Cell::new(0.0f64)));
    let (l, h) = (lo.clone(), hi.clone());
    let s = follow(&mut d, sprite, 100, true, 1400, move |d| {
        let r = d.transport().rate;
        l.set(l.get().min(r));
        h.set(h.get().max(r));
    });
    assert_eq!(rate(&d), 0.5, "the release restored ×0.5");
    let mut errors: Vec<f64> = (0..2000)
        .filter_map(|f| {
            let v = d.value(s, f)?;
            let t = sprite(f as f64);
            Some((v[0] as f64 - t[0]).hypot(v[1] as f64 - t[1]))
        })
        .collect();
    errors.sort_by(f64::total_cmp);
    let q = |p: f64| errors[((errors.len() - 1) as f64 * p).round() as usize];
    println!("under auto speed (×{:.2}–×{:.2}): {} frames, point error median {:.2} px, p95 {:.2} px", lo.get(), hi.get(), errors.len(), q(0.5), q(0.95));
    assert!(hi.get() / lo.get() > 2.0, "the rate varied");
    assert!(errors.len() > 150, "{} frames", errors.len());
    assert!(q(0.5) < 2.0, "median {:.2} px", q(0.5));
    assert!(q(0.95) < 4.0, "p95 {:.2} px", q(0.95));
}
