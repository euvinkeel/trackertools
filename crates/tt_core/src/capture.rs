//! The Sketch tool (DESIGN §8.1, §8.3): press and hold on the viewport and
//! follow something with the pointer.
//!
//! - The video plays at the transport rate, which is the capture speed, set
//!   with `[` / `]` beforehand. It pauses at the last frame instead of looping.
//! - Hold the simulate key (Space) to freeze on the current frame: samples keep
//!   streaming and the box sizes to the jiggle (hold-to-simulate). Steps and
//!   scrubs while holding are recorded too, so a hard passage can be sculpted
//!   frame by frame. With the Sketch tool active, a *tap* of the key still
//!   plays/pauses, decided when the key comes up.
//! - Esc abandons the capture.
//!
//! While the button is held, the capture lives in [`LiveCapture`] and its
//! preview is the same pipeline over the samples so far. Release commits it
//! as one transaction (one undo step): a [`Capture`] entity (raw stream + clock
//! map) and a `sketch` operator reading it, which becomes the selection.

use bevy_ecs::name::Name;
use bevy_ecs::prelude::*;

use crate::app::{AppBuilder, Module, Set};
use crate::history::edit;
use crate::input::{Action, Keymap, KeysHeld, PendingActions};
use crate::meta::Class;
use crate::op::{Inputs, Operator, Output};
use crate::selection::Selection;
use crate::sketch::{BOX_CHANNELS, Capture, ClockMap, STREAM_CHANNELS, SketchParams, sketch_boxes};
use crate::time::{FrameIndex, WallClock};
use crate::tool::{ActiveTool, PointerFrame, Tool};
use crate::transport::Transport;

/// A capture in progress.
#[derive(Debug, Clone)]
pub struct Live {
    /// Wall time of the press; sample and clock times are relative to it.
    pub start: f64,
    /// `[t, x, y]`, t ascending.
    pub samples: Vec<[f64; 3]>,
    pub clock: ClockMap,
    /// Capture speed: the transport rate at the press.
    pub rate: f64,
    pub params: SketchParams,
    /// The pipeline over the samples so far: `(first frame, boxes)`.
    pub preview: Option<(FrameIndex, Vec<Option<[f64; 6]>>)>,
    looping: bool,
}

impl Live {
    /// The preview's `[x, y, left, top, right, bottom]` at frame `f`.
    pub fn preview_at(&self, f: FrameIndex) -> Option<[f64; 6]> {
        let (first, boxes) = self.preview.as_ref()?;
        let i = usize::try_from(f - first).ok()?;
        boxes.get(i).copied().flatten()
    }
}

#[derive(Resource, Debug, Default)]
pub struct LiveCapture(pub Option<Live>);

/// Parameters new sketches start with.
#[derive(Resource, Debug, Default, Clone)]
pub struct SketchDefaults(pub SketchParams);

/// With the Sketch tool active the simulate key is a hold during captures
/// and a tap-to-play otherwise; which one is known when the key comes up.
#[derive(Resource, Debug, Default)]
struct SimulateKey {
    held: bool,
    toggle_on_release: bool,
}

/// Raw input only reports motion: a still pointer is extended to "now"
/// once it has been still this long.
const STILL: f64 = 0.02;

fn simulate_key(
    tool: Res<ActiveTool>,
    keys: Res<KeysHeld>,
    keymap: Res<Keymap>,
    live: Res<LiveCapture>,
    mut actions: ResMut<PendingActions>,
    mut sim: ResMut<SimulateKey>,
) {
    let held = keys.contains(keymap.simulate);
    let capturing = live.0.is_some();
    if (tool.0 == Tool::Sketch && held) || capturing {
        // The key's own press queued TogglePlay: hold it until the key comes up.
        if !actions.take(|a| a == Action::TogglePlay).is_empty() && !capturing {
            sim.toggle_on_release = true;
        }
    }
    if capturing {
        sim.toggle_on_release = false; // it was a hold after all
    }
    if sim.held && !held && std::mem::take(&mut sim.toggle_on_release) {
        actions.push(Action::TogglePlay);
    }
    sim.held = held;
}

/// Start, extend and commit captures (`Set::Tools`, after the transport moved).
pub fn sketch_tool(world: &mut World) {
    let pointer = world.resource::<PointerFrame>().clone();
    let now = world.resource::<WallClock>().now;
    let frozen = world.resource::<KeysHeld>().contains(world.resource::<Keymap>().simulate);

    if world.resource::<LiveCapture>().0.is_none() {
        let armed = world.resource::<ActiveTool>().0 == Tool::Sketch && world.resource::<Transport>().has_media();
        let Some(start) = pointer.pressed.filter(|_| armed) else { return };
        let params = world.resource::<SketchDefaults>().0.clone();
        let mut t = world.resource_mut::<Transport>();
        let live = Live {
            start,
            samples: Vec::new(),
            clock: ClockMap::default(),
            rate: t.rate,
            params,
            preview: None,
            looping: t.looping,
        };
        t.looping = false;
        world.resource_mut::<LiveCapture>().0 = Some(live);
    }
    let mut live = world.resource_mut::<LiveCapture>().0.take().expect("live capture");
    let cancelled = !world.resource_mut::<PendingActions>().take(|a| a == Action::Cancel).is_empty();
    let end = pointer.released.or((!pointer.down).then_some(now));

    // Samples up to the release, kept in time order.
    let until = end.unwrap_or(f64::INFINITY);
    for s in &pointer.samples {
        let t = s[0] - live.start;
        if t >= 0.0 && s[0] <= until && live.samples.last().is_none_or(|l| t > l[0]) {
            live.samples.push([t, s[1], s[2]]);
        }
    }
    let t_now = end.unwrap_or(now) - live.start;
    match live.samples.last().copied() {
        Some(l) if t_now - l[0] > STILL => live.samples.push([t_now, l[1], l[2]]),
        None => {
            if let Some(h) = pointer.hover {
                live.samples.push([t_now.max(0.0), h[0], h[1]]);
            }
        }
        _ => {}
    }

    // The transport: playing unless frozen; the clock map records what was shown.
    let fps = {
        let mut t = world.resource_mut::<Transport>();
        if end.is_none() && !cancelled {
            t.playing = !frozen && t.frame() < t.last_frame();
        } else {
            t.playing = false;
            t.looping = live.looping;
        }
        live.clock.push(t_now, t.playhead, t.playing);
        t.fps.as_f64()
    };
    if cancelled {
        tracing::info!("sketch cancelled");
        return;
    }
    live.preview = sketch_boxes(&live.samples, &live.clock, &live.params, fps);
    if end.is_some() {
        commit(world, live);
    } else {
        world.resource_mut::<LiveCapture>().0 = Some(live);
    }
}

/// One undo step: the capture entity (stream + clock map) and a sketch operator on it.
fn commit(world: &mut World, live: Live) {
    if live.preview.as_ref().is_none_or(|(_, b)| b.iter().all(Option::is_none)) {
        return; // a click, not a gesture
    }
    let n = {
        let mut q = world.query::<&Operator>();
        q.iter(world).filter(|o| o.kind == "sketch").count() + 1
    };
    let mut made = None;
    edit(world, "Sketch", |tx| {
        let stream = tx.create_signal(STREAM_CHANNELS);
        let flat: Vec<f32> = live.samples.iter().flat_map(|s| s.map(|v| v as f32)).collect();
        tx.signal(stream).write(0, &flat);
        let capture = tx.spawn((
            Name::new(format!("Capture {n}")),
            Capture { rate: live.rate, samples: live.samples.len() as u32 },
            live.clock.clone(),
            Output(stream),
        ));
        let out = tx.create_signal(BOX_CHANNELS);
        made = Some(tx.spawn((
            Name::new(format!("Sketch {n}")),
            Operator { kind: "sketch".into() },
            Inputs(vec![("capture".into(), capture)]),
            Output(out),
            live.params.clone(),
        )));
    });
    if let Some(op) = made {
        world.resource_mut::<Selection>().select_only(op);
        tracing::info!(
            "sketch {n}: {} samples over {:.2} s, {} frames",
            live.samples.len(),
            live.samples.last().map_or(0.0, |s| s[0]),
            live.preview.as_ref().map_or(0, |(_, b)| b.iter().flatten().count())
        );
    }
}

pub struct CaptureModule;

impl Module for CaptureModule {
    fn build(&self, app: &mut AppBuilder) {
        app.declare::<LiveCapture>(Class::Session)
            .declare::<SketchDefaults>(Class::Session)
            .declare::<SimulateKey>(Class::Derived)
            .init_resource::<LiveCapture>()
            .init_resource::<SketchDefaults>()
            .init_resource::<SimulateKey>()
            .add_systems(simulate_key.in_set(Set::Input))
            .add_systems(sketch_tool.in_set(Set::Tools));
    }
}
