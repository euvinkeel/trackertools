//! The Sketch tool driven through the world, as the app drives it: one
//! PointerFrame per app frame at 175 Hz with 1 kHz samples.

use tt_core::capture::LiveCapture;
use tt_core::history::{self, History};
use tt_core::input::Action;
use tt_core::selection::Selection;
use tt_core::sketch::{Capture, Stroke, falloff_weight};
use tt_core::tool::{ActiveTool, PointerFrame, Tool};
use tt_core::transport::Transport;

mod common;
use common::*;

#[test]
fn pressing_records_without_touching_the_transport() {
    let mut d = Driver::new();
    d.core.world.resource_mut::<Transport>().seek(100);
    d.frame(still(400.0, 200.0), PRESS);
    d.frames(90, still(400.0, 200.0), HOLD);
    assert!(!d.transport().playing, "the press does not start playback");
    assert_eq!(d.transport().frame(), 100);
    d.frame(still(400.0, 200.0), UP);

    let sketches = d.sketches();
    assert_eq!(sketches.len(), 1, "a stroke with nothing selected starts a sketch");
    let s = sketches[0];
    assert_eq!(d.strokes(s), 1);
    let v = d.value(s, 100).expect("the held frame has a value");
    assert!((v[0] - 400.0).abs() < 1.0 && (v[1] - 200.0).abs() < 1.0, "{v:?}");
    assert!(d.value(s, 99).is_none() && d.value(s, 101).is_none(), "a hold edits one instant");
    assert_eq!(d.core.world.resource::<Selection>().primary(), Some(s));
    assert_eq!(d.core.world.resource::<History>().undo_label(), Some("New sketch"));
}

#[test]
fn holding_while_the_video_plays_records_across_frames() {
    let mut d = Driver::new();
    d.core.world.resource_mut::<Transport>().seek(100);
    d.frame(circle, PRESS);
    // Space (the host queues TogglePlay) plays while the button stays down.
    d.frame(circle, Input { action: Some(Action::TogglePlay), ..HOLD });
    d.frames(350, circle, HOLD);
    assert!(d.transport().playing);
    d.frame(circle, Input { action: Some(Action::TogglePlay), ..HOLD });
    d.frames(50, circle, HOLD);
    let paused_on = d.transport().frame();
    let preview = d.core.world.resource::<LiveCapture>().0.as_ref().unwrap().preview.clone().unwrap();
    d.frame(circle, UP);
    assert!(!d.transport().playing && d.transport().frame() == paused_on, "the release leaves the transport alone");

    let s = d.sketches()[0];
    // ≈ 2 s at half speed ≈ 60 frames, all with values; the committed sketch matches the preview.
    let covered = (90..200).filter(|f| d.value(s, *f).is_some()).count();
    assert!((55..=70).contains(&covered), "covered {covered} frames");
    assert!(d.value(s, paused_on).is_some(), "the frame held at the end has a value");
    let (first, values) = preview;
    for (i, p) in values.iter().enumerate() {
        let f = first + i as i64;
        if let (Some(p), Some(v)) = (p, d.value(s, f)) {
            assert!((p[0] - v[0] as f64).abs() < 0.5 && (p[1] - v[1] as f64).abs() < 0.5, "frame {f}");
        }
    }
}

#[test]
fn a_stroke_edits_the_selected_sketch_with_falloff_and_undoes_alone() {
    let mut d = Driver::new();
    // A first stroke along a line, followed while playing.
    let line = |t: f64| [200.0 + 60.0 * t, 300.0];
    d.core.world.resource_mut::<Transport>().seek(100);
    d.frame(line, PRESS);
    d.frame(line, Input { action: Some(Action::TogglePlay), ..HOLD });
    d.frames(500, line, HOLD);
    d.frame(line, Input { action: Some(Action::TogglePlay), ..UP });
    let s = d.sketches()[0];
    let before: Vec<Option<[f32; 6]>> = (0..300).map(|f| d.value(s, f)).collect();
    assert!(before[120].is_some() && before[130].is_some() && before[145].is_some());

    // Edit frame 130: hold 40 px to the right of where the path is.
    d.core.world.resource_mut::<Transport>().seek(130);
    let target = before[130].unwrap();
    let edit = still(target[0] as f64 + 40.0, target[1] as f64);
    d.frame(edit, PRESS);
    d.frames(90, edit, HOLD);
    d.frame(edit, UP);

    assert_eq!(d.sketches(), vec![s], "the stroke went onto the selected sketch");
    assert_eq!(d.strokes(s), 2);
    let moved = |d: &Driver, f: i64| d.value(s, f).unwrap()[0] - before[f as usize].unwrap()[0];
    assert!((moved(&d, 130) - 40.0).abs() < 1.5, "the held frame moved by the edit: {}", moved(&d, 130));
    let radius = 0.2 * 60.0; // the default falloff, in frames
    for k in [3i64, 6, 9] {
        let expect = moved(&d, 130) * falloff_weight(k as f64, radius) as f32;
        assert!((moved(&d, 130 + k) - expect).abs() < 0.5 && (moved(&d, 130 - k) - expect).abs() < 0.5, "frame 130±{k}");
    }
    assert_eq!(moved(&d, 145), 0.0, "past the falloff nothing moved");
    assert_eq!(d.core.world.resource::<History>().undo_label(), Some("Stroke on Sketch 1"));

    history::undo(&mut d.core.world);
    d.frames(2, edit, UP);
    assert_eq!(d.strokes(s), 1);
    assert_eq!((0..300).map(|f| d.value(s, f)).collect::<Vec<_>>(), before, "undo restores the path exactly");
    history::redo(&mut d.core.world);
    d.frames(2, edit, UP);
    assert_eq!(d.strokes(s), 2);
    let w = &mut d.core.world;
    assert_eq!(w.query::<&Capture>().iter(w).count(), 2);
}

#[test]
fn shift_starts_a_new_sketch_and_alt_a_deselects() {
    let mut d = Driver::new();
    let hold = |d: &mut Driver, f: i64, shift: bool| {
        d.core.world.resource_mut::<Transport>().seek(f);
        d.frame(still(300.0, 300.0), Input { shift, ..PRESS });
        d.frames(45, still(300.0, 300.0), HOLD);
        d.frame(still(300.0, 300.0), UP);
    };
    hold(&mut d, 50, false);
    hold(&mut d, 60, false);
    assert_eq!(d.sketches().len(), 1, "the second stroke edits the selected sketch");
    hold(&mut d, 70, true);
    assert_eq!(d.sketches().len(), 2, "Shift at the press starts a new sketch");
    d.frame(still(0.0, 0.0), Input { action: Some(Action::DeselectAll), ..UP });
    assert!(d.core.world.resource::<Selection>().entities.is_empty());
    hold(&mut d, 80, false);
    assert_eq!(d.sketches().len(), 3, "with nothing selected a stroke starts a new sketch");
}

#[test]
fn the_wheel_sets_size_falloff_or_both_as_chosen() {
    use tt_core::capture::{SketchDefaults, WheelMode};
    let mut d = Driver::new();
    let stroke = |d: &Driver| d.core.world.resource::<LiveCapture>().0.as_ref().unwrap().stroke.clone();
    // Default: the wheel sizes the region.
    d.frame(circle, PRESS);
    d.frame(circle, Input { wheel: 2.0, ..HOLD });
    let s = stroke(&d);
    assert!((s.scale - 1.25 * 1.25).abs() < 1e-6 && s.falloff == 0.2, "size ×{}, falloff {}", s.scale, s.falloff);
    d.frame(circle, Input { action: Some(Action::Cancel), ..HOLD });
    d.frame(circle, UP);
    for (mode, scale, falloff) in [(WheelMode::Falloff, 1.0, 0.2 * 1.25), (WheelMode::Both, 1.25, 0.2 * 1.25)] {
        d.core.world.resource_mut::<SketchDefaults>().wheel = mode;
        d.frame(circle, PRESS);
        d.frame(circle, Input { wheel: 1.0, ..HOLD });
        let s = stroke(&d);
        assert!((s.scale - scale).abs() < 1e-6 && (s.falloff - falloff).abs() < 1e-6, "{mode:?}: size ×{}, falloff {}", s.scale, s.falloff);
        d.frame(circle, Input { action: Some(Action::Cancel), ..HOLD });
        d.frame(circle, UP);
    }
}

#[test]
fn a_bigger_size_makes_a_bigger_region_and_carries_to_the_next_stroke() {
    use tt_core::capture::SketchDefaults;
    let mut d = Driver::new();
    let hold = |d: &mut Driver, f: i64, wheel: f32, new: bool| {
        d.core.world.resource_mut::<Transport>().seek(f);
        d.frame(still(300.0, 300.0), Input { shift: new, ..PRESS });
        d.frame(still(300.0, 300.0), Input { wheel, ..HOLD });
        d.frames(60, still(300.0, 300.0), HOLD);
        d.frame(still(300.0, 300.0), UP);
        d.core.world.resource::<tt_core::selection::Selection>().primary().unwrap()
    };
    let a = hold(&mut d, 50, 0.0, true);
    let small = d.value(a, 50).unwrap();
    let b = hold(&mut d, 50, 3.0, true); // ×1.25³ ≈ 1.95
    let big = d.value(b, 50).unwrap();
    let ratio = (big[4] - big[2]) / (small[4] - small[2]);
    assert!((ratio - 1.953).abs() < 0.02, "the region grew ×{ratio:.3}");
    assert!((d.core.world.resource::<SketchDefaults>().stroke.scale - 1.953).abs() < 0.01, "the next stroke starts at that size");
}

#[test]
fn the_wheel_sets_the_falloff_and_esc_cancels() {
    let mut d = Driver::new();
    d.core.world.resource_mut::<tt_core::capture::SketchDefaults>().wheel = tt_core::capture::WheelMode::Falloff;
    d.frame(circle, PRESS);
    d.frame(circle, Input { wheel: 2.0, ..HOLD });
    let falloff = d.core.world.resource::<LiveCapture>().0.as_ref().unwrap().stroke.falloff;
    assert!((falloff - 0.2 * 1.25 * 1.25).abs() < 1e-6, "{falloff}");
    d.frames(20, circle, HOLD);
    d.frame(circle, Input { action: Some(Action::Cancel), ..HOLD });
    assert!(d.core.world.resource::<LiveCapture>().0.is_none());
    d.frame(circle, UP);
    assert!(d.sketches().is_empty(), "a cancelled stroke leaves nothing");
    assert!(!d.core.world.resource::<History>().can_undo());
    assert_eq!(d.core.world.resource::<ActiveTool>().0, Tool::Sketch, "Esc cancelled the stroke, not the tool");
    d.frame(circle, Input { action: Some(Action::Cancel), ..UP });
    assert_eq!(d.core.world.resource::<ActiveTool>().0, Tool::Select, "a second Esc leaves the tool");
}

/// A hold of `n` app frames at `(x, y)` on frame `f`.
fn hold_at(d: &mut Driver, f: i64, x: f64, y: f64, n: usize, input: Input) {
    d.core.world.resource_mut::<Transport>().seek(f);
    d.frame(still(x, y), Input { press: true, down: true, ..input });
    d.frames(n, still(x, y), HOLD);
    d.frame(still(x, y), UP);
}

#[test]
fn a_quick_click_selects_instead_of_recording() {
    let mut d = Driver::new();
    hold_at(&mut d, 50, 300.0, 300.0, 60, HOLD);
    let a = d.sketches()[0];
    d.frame(still(0.0, 0.0), Input { action: Some(Action::DeselectAll), ..UP });
    assert!(d.core.world.resource::<Selection>().entities.is_empty());
    // A 3-frame click on the sketch's region selects it and records nothing.
    hold_at(&mut d, 50, 302.0, 301.0, 2, HOLD);
    assert_eq!(d.sketches(), vec![a], "no new sketch");
    assert_eq!(d.strokes(a), 1, "no stroke");
    assert_eq!(d.core.world.resource::<Selection>().primary(), Some(a));
    // A click on empty video clears the selection.
    hold_at(&mut d, 50, 900.0, 900.0, 2, HOLD);
    assert!(d.core.world.resource::<Selection>().entities.is_empty());
    assert_eq!(d.sketches(), vec![a]);
}

#[test]
fn ctrl_moves_the_point_but_keeps_the_size() {
    let mut d = Driver::new();
    // A jiggly hold makes a big region at frame 100.
    d.core.world.resource_mut::<Transport>().seek(100);
    let jiggle = |t: f64| [300.0 + 30.0 * (47.0 * t).sin(), 300.0 + 30.0 * (53.0 * t).cos()];
    d.frame(jiggle, PRESS);
    d.frames(90, jiggle, HOLD);
    d.frame(jiggle, UP);
    let s = d.sketches()[0];
    let before = d.value(s, 100).unwrap();
    let width = before[4] - before[2];
    assert!(width > 60.0, "the jiggle made a big region: {width}");
    // A quiet Ctrl-hold 50 px to the right moves the point only.
    hold_at(&mut d, 100, before[0] as f64 + 50.0, before[1] as f64, 60, Input { ctrl: true, ..HOLD });
    let after = d.value(s, 100).unwrap();
    assert!((after[0] - before[0] - 50.0).abs() < 1.5, "moved {}", after[0] - before[0]);
    assert!((after[4] - after[2] - width).abs() < 0.01, "size kept: {} vs {width}", after[4] - after[2]);
    // Without Ctrl the quiet hold also sets the size (hold-to-simulate: tight).
    hold_at(&mut d, 100, before[0] as f64, before[1] as f64, 60, HOLD);
    let tight = d.value(s, 100).unwrap();
    assert!(tight[4] - tight[2] < 40.0, "a quiet hold makes it tight: {}", tight[4] - tight[2]);
}

#[test]
fn a_stroke_whose_sketch_is_undone_meanwhile_starts_a_new_sketch() {
    let mut d = Driver::new();
    hold_at(&mut d, 50, 300.0, 300.0, 60, HOLD);
    let a = d.sketches()[0];
    d.core.world.resource_mut::<Transport>().seek(60);
    d.frame(still(310.0, 300.0), PRESS);
    d.frames(20, still(310.0, 300.0), HOLD);
    // Ctrl+Z while holding: the sketch being edited is undone.
    d.frame(still(310.0, 300.0), Input { action: Some(Action::Undo), ..HOLD });
    d.frames(40, still(310.0, 300.0), HOLD);
    d.frame(still(310.0, 300.0), UP);
    let live: Vec<_> = d.sketches();
    assert_eq!(live.len(), 1, "one enabled sketch: the new one");
    assert_ne!(live[0], a);
    assert_eq!(d.strokes(a), 1, "the undone sketch was not written to");
    assert!(d.value(live[0], 60).is_some());
}

#[test]
fn deselecting_in_the_same_frame_as_the_press_starts_a_new_sketch() {
    let mut d = Driver::new();
    hold_at(&mut d, 50, 300.0, 300.0, 60, HOLD);
    hold_at(&mut d, 70, 300.0, 300.0, 60, Input { action: Some(Action::DeselectAll), ..HOLD });
    assert_eq!(d.sketches().len(), 2);
}

#[test]
fn a_live_stroke_takes_the_wheel() {
    let mut d = Driver::new();
    d.frame(circle, PRESS);
    assert!(d.core.world.resource::<PointerFrame>().wheel_taken);
    d.frames(40, circle, HOLD);
    d.frame(circle, UP);
    assert!(d.core.world.resource::<PointerFrame>().wheel_taken, "also on the release frame");
    d.frame(circle, UP);
    assert!(!d.core.world.resource::<PointerFrame>().wheel_taken);
}

/// A subject moving right at 240 px per second of video.
fn subject_x(video_t: f64) -> f64 {
    300.0 + 240.0 * video_t
}

/// The same deliberate jiggle in both modes: ±20 px, a few hertz.
fn jiggle(t: f64) -> [f64; 2] {
    [20.0 * (std::f64::consts::TAU * 3.0 * t).sin(), 20.0 * (std::f64::consts::TAU * 2.3 * t).cos()]
}

#[test]
fn a_paused_jiggle_sizes_the_box_like_the_same_jiggle_during_playback() {
    // Recording while playing at ½ speed: the hand follows the subject 250 ms
    // late (what it saw), jiggling.
    let mut d = Driver::new();
    d.core.world.resource_mut::<Transport>().seek(100);
    let shown = std::rc::Rc::new(std::cell::RefCell::new(vec![(d.now, 100.0f64)]));
    let hand = |shown: &std::rc::Rc<std::cell::RefCell<Vec<(f64, f64)>>>| {
        let shown = shown.clone();
        move |t: f64| {
            let s = shown.borrow();
            let i = s.partition_point(|(w, _)| *w <= t - 0.25).saturating_sub(1);
            let f = s[i].1;
            let j = jiggle(t);
            [subject_x(f / 60.0) + j[0], 300.0 + j[1]]
        }
    };
    let follow = hand(&shown);
    d.frame(&follow, PRESS);
    d.frame(&follow, Input { action: Some(Action::TogglePlay), ..HOLD });
    for _ in 0..700 {
        let f = d.transport().playhead;
        shown.borrow_mut().push((d.now, f.floor()));
        d.frame(&follow, HOLD);
    }
    d.frame(&follow, Input { action: Some(Action::TogglePlay), ..UP });
    let s = d.sketches()[0];
    let f = 150;
    let played = d.value(s, f).expect("frame 150 recorded");
    let (w_play, h_play) = (played[4] - played[2], played[5] - played[3]);

    // The same jiggle, paused on frame 150, as an edit of that sketch.
    d.core.world.resource_mut::<Transport>().seek(f);
    let x = subject_x(f as f64 / 60.0);
    let paused = move |t: f64| {
        let j = jiggle(t);
        [x + j[0], 300.0 + j[1]]
    };
    d.frame(paused, PRESS);
    d.frames(175, paused, HOLD);
    d.frame(paused, UP);
    let held = d.value(s, f).unwrap();
    let (w_hold, h_hold) = (held[4] - held[2], held[5] - held[3]);

    // And on its own (a new sketch: jiggle only, no neighbouring motion).
    d.frame(paused, Input { action: Some(Action::DeselectAll), ..UP });
    d.frame(paused, PRESS);
    d.frames(175, paused, HOLD);
    d.frame(paused, UP);
    let second = d.sketches().into_iter().find(|e| *e != s).expect("a second sketch");
    let alone = d.value(second, f).unwrap();
    println!(
        "box at frame {f}: recorded while playing {w_play:.0}×{h_play:.0}; paused jiggle edit {w_hold:.0}×{h_hold:.0}; paused jiggle alone {:.0}×{:.0}",
        alone[4] - alone[2],
        alone[5] - alone[3]
    );
    assert!((0.75..=1.33).contains(&(w_hold / w_play)), "width: paused {w_hold:.0} vs playing {w_play:.0}");
    assert!((0.75..=1.33).contains(&(h_hold / h_play)), "height: paused {h_hold:.0} vs playing {h_play:.0}");
}

// ---- the hand's lag, per stroke --------------------------------------------------------------

/// The sprite fixture's centre (source pixels) at video time t (as in tests/sketch.rs).
fn sprite(t: f64) -> [f64; 2] {
    [
        (950.0 + 500.0 * (0.9 * t).sin() + 60.0 * (5.3 * t).sin()).floor() + 10.5,
        (530.0 + 300.0 * (1.3 * t + 0.7).sin() + 40.0 * (4.1 * t).sin()).floor() + 10.5,
    ]
}

/// Record the sprite at `rate` from frame `from` for `wall` seconds of playback:
/// the hand follows the frame that was on screen 0.25 s (real time) earlier,
/// with tremor. `new` starts a new sketch. Returns the sketch.
fn follow_sprite(d: &mut Driver, rate: f64, from: i64, wall: f64, new: bool) -> bevy_ecs::entity::Entity {
    use std::cell::RefCell;
    use std::rc::Rc;
    d.core.world.resource_mut::<Transport>().seek(from);
    d.core.world.resource_mut::<Transport>().rate = rate;
    // (wall time, playhead) after every app frame: what was on screen when.
    let shown = Rc::new(RefCell::new(vec![(d.now - 1.0, from as f64), (d.now, from as f64)]));
    let hand = {
        let shown = shown.clone();
        move |t: f64| {
            let s = shown.borrow();
            let w = t - 0.25;
            let i = s.partition_point(|(t, _)| *t <= w).clamp(1, s.len() - 1);
            let ((t0, p0), (t1, p1)) = (s[i - 1], s[i]);
            let playhead = if w >= t1 { p1 } else { p0 + (p1 - p0) * ((w - t0) / (t1 - t0)).clamp(0.0, 1.0) };
            let p = sprite(playhead.floor() / 60.0);
            let tremor = |ph: f64| 1.5 * ((9.0 * std::f64::consts::TAU * t + ph).sin() * 0.6 + (12.3 * std::f64::consts::TAU * t + 2.0 * ph).sin() * 0.4);
            [p[0] + tremor(0.0), p[1] + tremor(1.3)]
        }
    };
    let step = |d: &mut Driver, input: Input| {
        d.frame(&hand, input);
        shown.borrow_mut().push((d.now, d.transport().playhead));
    };
    step(d, Input { shift: new, ..PRESS });
    step(d, Input { action: Some(Action::TogglePlay), ..HOLD });
    for _ in 0..(wall * UI_HZ) as usize {
        step(d, HOLD);
    }
    step(d, Input { action: Some(Action::TogglePlay), ..HOLD });
    for _ in 0..60 {
        step(d, HOLD); // a beat past the end: the hand catches up
    }
    step(d, UP);
    d.core.world.resource::<Selection>().primary().expect("the sketch is selected")
}

/// Median point error against the sprite over `frames` (those with a value).
fn sprite_error(d: &Driver, s: bevy_ecs::entity::Entity, frames: std::ops::Range<i64>) -> (f64, usize) {
    let mut e: Vec<f64> = frames
        .filter_map(|f| {
            let v = d.value(s, f)?;
            let t = sprite(f as f64 / 60.0);
            Some((v[0] as f64 - t[0]).hypot(v[1] as f64 - t[1]))
        })
        .collect();
    e.sort_by(f64::total_cmp);
    (e.get(e.len() / 2).copied().unwrap_or(f64::NAN), e.len())
}

/// A sketch's strokes (capture entities), in layer order.
fn strokes_of(d: &Driver, s: bevy_ecs::entity::Entity) -> Vec<bevy_ecs::entity::Entity> {
    d.core.world.get::<tt_core::op::Inputs>(s).unwrap().0.iter().filter(|(slot, _)| slot == "stroke").map(|(_, e)| *e).collect()
}

fn set_lag(d: &mut Driver, capture: bevy_ecs::entity::Entity, lag: f32) {
    history::edit(&mut d.core.world, "Edit Stroke", |tx| tx.modify::<Stroke>(capture, |s| s.lag = lag));
    d.frames(2, still(0.0, 0.0), UP);
}

#[test]
fn the_lag_is_real_time_at_any_playback_speed() {
    for rate in [2.0, 0.5] {
        let mut d = Driver::new();
        let s = follow_sprite(&mut d, rate, 50, 4.0, true);
        let (median, n) = sprite_error(&d, s, 0..600);
        // The same stroke if its lag were counted in video time (0.25 s of video).
        let stroke = strokes_of(&d, s)[0];
        assert_eq!(d.core.world.get::<Stroke>(stroke).unwrap().lag, 0.25, "the stroke took the sketch's lag");
        set_lag(&mut d, stroke, (0.25 / rate) as f32);
        let (video_lag, _) = sprite_error(&d, s, 0..600);
        println!("at {rate}×: {n} frames, median point error {median:.2} px with 0.25 s of real time; {video_lag:.2} px with 0.25 s of video");
        assert!(n as f64 > 4.0 * rate * 60.0 * 0.9, "{n} frames");
        assert!(median < 3.0, "at {rate}×: {median:.2} px (measured 2.33 at 2×, 1.51 at ½× when set)");
        assert!(video_lag > 3.0 * median, "at {rate}×, a video-time lag is far off: {video_lag:.2} vs {median:.2} px");
    }
}

#[test]
fn changing_one_strokes_lag_moves_only_its_frames() {
    let mut d = Driver::new();
    d.core.world.resource_mut::<Transport>().rate = 1.0;
    let s = follow_sprite(&mut d, 1.0, 50, 1.5, true);
    let s2 = follow_sprite(&mut d, 1.0, 300, 1.5, false);
    assert_eq!(s, s2, "the second stroke went onto the selected sketch");
    let [first, second] = strokes_of(&d, s)[..] else { panic!("two strokes") };
    let before: Vec<Option<[f32; 6]>> = (0..600).map(|f| d.value(s, f)).collect();
    set_lag(&mut d, second, 0.4);
    let after: Vec<Option<[f32; 6]>> = (0..600).map(|f| d.value(s, f)).collect();
    assert_eq!(before[..250], after[..250], "the first stroke's frames stay exactly");
    let moved = (300..390).filter(|f| before[*f as usize].is_some() && before[*f as usize] != after[*f as usize]).count();
    assert!(moved > 80, "the second stroke's frames moved: {moved} of 90");
    assert_eq!(d.core.world.get::<Stroke>(first).unwrap().lag, 0.25);
    history::undo(&mut d.core.world);
    d.frames(2, still(0.0, 0.0), UP);
    assert_eq!((0..600).map(|f| d.value(s, f)).collect::<Vec<_>>(), before, "undo restores the path");
}

#[test]
fn a_new_stroke_takes_its_sketchs_lag() {
    use tt_core::capture::SketchDefaults;
    use tt_core::sketch::SketchParams;
    let mut d = Driver::new();
    d.core.world.resource_mut::<SketchDefaults>().params.lag = 0.3;
    hold_at(&mut d, 50, 300.0, 300.0, 60, HOLD);
    let s = d.sketches()[0];
    assert_eq!(d.core.world.get::<SketchParams>(s).unwrap().lag, 0.3, "a new sketch takes the default lag");
    history::edit(&mut d.core.world, "Edit SketchParams", |tx| tx.modify::<SketchParams>(s, |p| p.lag = 0.15));
    hold_at(&mut d, 70, 300.0, 300.0, 60, HOLD);
    let lags: Vec<f32> = strokes_of(&d, s).iter().map(|c| d.core.world.get::<Stroke>(*c).unwrap().lag).collect();
    assert_eq!(lags, vec![0.3, 0.15], "each stroke keeps the lag its sketch had when it was drawn");
}

#[test]
fn a_stroke_saved_before_its_lag_existed_loads_with_the_default() {
    let mut d = Driver::new();
    let s: Stroke = load_component(&mut d.core.world, "(falloff: 0.5, influence: 1.0, size: 1.0, scale: 2.0)");
    assert_eq!((s.falloff, s.scale, s.lag), (0.5, 2.0, 0.25));
}
