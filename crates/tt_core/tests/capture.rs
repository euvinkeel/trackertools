//! The Sketch tool driven through the world, as the app drives it: one
//! PointerFrame per app frame at 175 Hz with 1 kHz samples.

use bevy_ecs::entity::Entity;
use tt_core::capture::LiveCapture;
use tt_core::history::{self, History};
use tt_core::input::{Action, KeysHeld, Mods, PendingActions};
use tt_core::op::{Inputs, Operator, Output};
use tt_core::selection::Selection;
use tt_core::signal::SignalStore;
use tt_core::sketch::{Capture, falloff_weight};
use tt_core::time::{Rational, WallClock};
use tt_core::tool::{ActiveTool, PointerFrame, Tool};
use tt_core::transport::Transport;
use tt_core::{AppBuilder, Core, CoreModules};

const UI_HZ: f64 = 175.0;

struct Driver {
    core: Core,
    now: f64,
}

#[derive(Default, Clone, Copy)]
struct Input {
    press: bool,
    down: bool,
    shift: bool,
    ctrl: bool,
    wheel: f32,
    action: Option<Action>,
}

const HOLD: Input = Input { press: false, down: true, shift: false, ctrl: false, wheel: 0.0, action: None };
const PRESS: Input = Input { press: true, ..HOLD };
const UP: Input = Input { down: false, ..HOLD };

impl Driver {
    fn new() -> Self {
        let mut app = AppBuilder::new();
        app.add_module(CoreModules);
        let mut core = app.build();
        *core.world.resource_mut::<Transport>() =
            Transport { fps: Rational::new(60, 1), frame_count: 600, rate: 0.5, ..Transport::default() };
        core.world.resource_mut::<ActiveTool>().0 = Tool::Sketch;
        Self { core, now: 1.0 }
    }

    /// One app frame: the pointer follows `path(t)` (1 kHz samples since the last frame).
    fn frame(&mut self, path: impl Fn(f64) -> [f64; 2], input: Input) {
        let prev = self.now;
        self.now += 1.0 / UI_HZ;
        let w = &mut self.core.world;
        w.resource_mut::<WallClock>().tick(self.now);
        let mut samples = Vec::new();
        let mut t = (prev * 1000.0).floor() / 1000.0 + 0.001;
        while t <= self.now {
            let p = path(t);
            samples.push([t, p[0], p[1]]);
            t += 0.001;
        }
        *w.resource_mut::<PointerFrame>() = PointerFrame {
            samples,
            hover: Some(path(self.now)),
            pressed: input.press.then_some(prev + 0.002),
            down: input.down,
            released: (!input.down).then_some(self.now - 0.001),
            wheel: input.wheel,
            scale: 1.0,
            ..PointerFrame::default()
        };
        *w.resource_mut::<KeysHeld>() = KeysHeld { keys: Vec::new(), mods: Mods { shift: input.shift, ctrl: input.ctrl, ..Mods::NONE } };
        if let Some(a) = input.action {
            w.resource_mut::<PendingActions>().push(a);
        }
        self.core.run_pre_ui();
        self.core.run_post_ui();
    }

    fn frames(&mut self, n: usize, path: impl Fn(f64) -> [f64; 2] + Copy, input: Input) {
        for _ in 0..n {
            self.frame(path, input);
        }
    }

    fn sketches(&mut self) -> Vec<Entity> {
        let w = &mut self.core.world;
        let mut q = w.query::<(Entity, &Operator)>();
        let mut v: Vec<Entity> = q.iter(w).filter(|(_, o)| o.kind == "sketch").map(|(e, _)| e).collect();
        v.sort();
        v
    }

    fn strokes(&self, sketch: Entity) -> usize {
        self.core.world.get::<Inputs>(sketch).map_or(0, |i| i.0.len())
    }

    fn value(&self, sketch: Entity, f: i64) -> Option<[f32; 6]> {
        let w = &self.core.world;
        let out = w.get::<Output>(sketch)?.0;
        w.resource::<SignalStore>().get(out)?.get_valid(f).map(|v| v.try_into().unwrap())
    }

    fn transport(&self) -> Transport {
        self.core.world.resource::<Transport>().clone()
    }
}

fn still(x: f64, y: f64) -> impl Fn(f64) -> [f64; 2] + Copy {
    move |t| [x + 0.5 * (40.0 * t).sin(), y + 0.5 * (37.0 * t).cos()]
}

fn circle(t: f64) -> [f64; 2] {
    [500.0 + 100.0 * t.cos(), 300.0 + 100.0 * t.sin()]
}

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
fn the_wheel_sets_the_falloff_and_esc_cancels() {
    let mut d = Driver::new();
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
