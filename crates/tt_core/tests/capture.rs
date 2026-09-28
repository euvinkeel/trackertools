//! The Sketch tool driven through the world, as the app drives it: one
//! PointerFrame per app frame at 175 Hz with 1 kHz samples.

use tt_core::capture::LiveCapture;
use tt_core::history;
use tt_core::input::{Action, Key, KeysHeld, PendingActions};
use tt_core::op::{Operator, Output};
use tt_core::selection::Selection;
use tt_core::signal::SignalStore;
use tt_core::sketch::Capture;
use tt_core::time::{Rational, WallClock};
use tt_core::tool::{ActiveTool, PointerFrame, Tool};
use tt_core::transport::Transport;
use tt_core::{AppBuilder, Core, CoreModules};

const UI_HZ: f64 = 175.0;

struct Driver {
    core: Core,
    now: f64,
}

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
    fn frame(&mut self, path: impl Fn(f64) -> [f64; 2], press: bool, down: bool, keys: &[Key], actions: &[Action]) {
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
        let hover = Some(path(self.now));
        *w.resource_mut::<PointerFrame>() = PointerFrame {
            samples,
            hover,
            pressed: press.then_some(prev + 0.002),
            down,
            released: (!down).then_some(self.now - 0.001),
        };
        w.resource_mut::<KeysHeld>().0 = keys.to_vec();
        w.resource_mut::<PendingActions>().0.extend_from_slice(actions);
        self.core.run_pre_ui();
        self.core.run_post_ui();
    }

    fn sketches(&mut self) -> Vec<bevy_ecs::entity::Entity> {
        let w = &mut self.core.world;
        let mut q = w.query::<(bevy_ecs::entity::Entity, &Operator)>();
        q.iter(w).filter(|(_, o)| o.kind == "sketch").map(|(e, _)| e).collect()
    }
}

fn circle(t: f64) -> [f64; 2] {
    [500.0 + 100.0 * t.cos(), 300.0 + 100.0 * t.sin()]
}

#[test]
fn press_follow_release_is_one_undoable_sketch() {
    let mut d = Driver::new();
    d.frame(circle, true, true, &[], &[]);
    assert!(d.core.world.resource::<LiveCapture>().0.is_some(), "the press starts a capture");
    for _ in 0..(UI_HZ as usize * 2) {
        d.frame(circle, false, true, &[], &[]);
    }
    assert!(d.core.world.resource::<Transport>().playing, "the video plays while the button is held");
    let live_preview = d.core.world.resource::<LiveCapture>().0.as_ref().unwrap().preview.clone().unwrap();
    d.frame(circle, false, false, &[], &[]);

    let w = &mut d.core.world;
    assert!(w.resource::<LiveCapture>().0.is_none());
    let t = w.resource::<Transport>();
    assert!(!t.playing, "release pauses where the capture ended");
    let end_frame = t.frame();
    // 2 s at half speed ≈ 60 frames.
    assert!((55..=65).contains(&end_frame), "end frame {end_frame}");
    let ops = d.sketches();
    assert_eq!(ops.len(), 1);
    let op = ops[0];
    let w = &mut d.core.world;
    assert_eq!(w.resource::<Selection>().primary(), Some(op), "the new sketch is selected");
    assert_eq!(w.resource::<tt_core::history::History>().undo_label(), Some("Sketch"));

    // The committed operator reproduces the live preview (same pipeline, f32 storage).
    let out = w.get::<Output>(op).unwrap().0;
    let sig = w.resource::<SignalStore>().get(out).unwrap();
    let (first, boxes) = &live_preview;
    let mut compared = 0;
    for (i, b) in boxes.iter().enumerate() {
        let (Some(b), Some(v)) = (b, sig.get_valid(first + i as i64)) else { continue };
        compared += 1;
        assert!((b[0] - v[0] as f64).abs() < 1.0 && (b[1] - v[1] as f64).abs() < 1.0, "frame {}: {b:?} vs {v:?}", first + i as i64);
    }
    assert!(compared > 40, "compared {compared} frames");

    // Undo removes the capture and the operator in one step; redo brings back the same ids.
    history::undo(w);
    assert!(d.sketches().is_empty());
    let w = &mut d.core.world;
    assert_eq!(w.query::<&Capture>().iter(w).count(), 0);
    history::redo(w);
    assert_eq!(d.sketches(), vec![op]);
}

#[test]
fn holding_the_simulate_key_freezes_the_frame() {
    let mut d = Driver::new();
    d.core.world.resource_mut::<Transport>().seek(100);
    // Space held from before the press: the press starts frozen.
    d.frame(circle, false, false, &[Key::Space], &[Action::TogglePlay]);
    d.frame(circle, true, true, &[Key::Space], &[]);
    for _ in 0..100 {
        d.frame(circle, false, true, &[Key::Space], &[]);
    }
    let t = d.core.world.resource::<Transport>();
    assert_eq!(t.frame(), 100, "frozen while the key is held");
    assert!(!t.playing);
    // Letting go of the key plays on; the capture continues.
    for _ in 0..50 {
        d.frame(circle, false, true, &[], &[]);
    }
    assert!(d.core.world.resource::<Transport>().frame() > 105);
    d.frame(circle, false, false, &[], &[]);
    assert!(!d.core.world.resource::<Transport>().playing, "the space press that began the hold did not toggle play afterwards");
    assert_eq!(d.sketches().len(), 1);
}

#[test]
fn a_tap_of_the_simulate_key_still_plays_and_esc_cancels() {
    let mut d = Driver::new();
    d.frame(circle, false, false, &[Key::Space], &[Action::TogglePlay]);
    assert!(!d.core.world.resource::<Transport>().playing, "decided on release");
    d.frame(circle, false, false, &[], &[]);
    d.frame(circle, false, false, &[], &[]);
    assert!(d.core.world.resource::<Transport>().playing, "a tap plays");
    d.frame(circle, false, false, &[], &[Action::TogglePlay]);

    d.frame(circle, true, true, &[], &[]);
    for _ in 0..50 {
        d.frame(circle, false, true, &[], &[]);
    }
    d.frame(circle, false, true, &[], &[Action::Cancel]);
    assert!(d.core.world.resource::<LiveCapture>().0.is_none());
    d.frame(circle, false, false, &[], &[]);
    assert!(d.sketches().is_empty(), "a cancelled capture leaves nothing");
    assert!(!d.core.world.resource::<tt_core::history::History>().can_undo());
    assert_eq!(d.core.world.resource::<ActiveTool>().0, Tool::Sketch, "Esc cancelled the capture, not the tool");
    d.frame(circle, false, false, &[], &[Action::Cancel]);
    assert_eq!(d.core.world.resource::<ActiveTool>().0, Tool::Select, "a second Esc leaves the tool");
}
