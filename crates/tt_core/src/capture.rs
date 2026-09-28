//! The Sketch tool (DESIGN §8.1, §8.3): record the pointer into a path, like
//! auto-keying while the video plays.
//!
//! - Press and hold on the viewport: the pointer is recorded against whatever
//!   frame is on screen. The transport stays independent (Space plays and
//!   pauses as usual, even while holding):
//!   - paused, the hold edits that instant (hold-to-simulate: the point is
//!     where the hand settles, the box is sized by its jiggle);
//!   - playing (or stepping, scrubbing), it records across frames.
//! - Each press → release is a *stroke* laid over the selected sketch: it
//!   replaces the frames it visited and pulls neighbouring frames along with a
//!   falloff (`sketch::layer_over`); the mouse wheel sets the falloff while
//!   holding. With no sketch selected, or with Shift held at the press, the
//!   stroke starts a new sketch.
//! - `Ctrl` at the press: move only (the stroke keeps the region's size).
//! - A quick click doesn't record: it selects the sketch under it (tool.rs).
//! - Esc abandons the stroke.
//!
//! While the button is held, the stroke lives in [`LiveCapture`]; its preview
//! is the same pipeline and layering over the sketch as it stands. Release
//! commits it as one transaction (one undo step): a [`Capture`] entity (raw
//! stream + clock map + [`Stroke`]) appended to the sketch's inputs, or a new
//! `sketch` operator reading it. The sketch becomes the selection.

use bevy_ecs::name::Name;
use bevy_ecs::prelude::*;

use crate::app::{AppBuilder, Module};
use crate::history::edit;
use crate::input::{Action, KeysHeld, PendingActions};
use crate::meta::Class;
use crate::op::{Inputs, Operator, Output};
use crate::selection::Selection;
use crate::signal::SignalStore;
use crate::sketch::{BOX_CHANNELS, Boxes, Capture, ClockMap, STREAM_CHANNELS, SketchParams, Stroke, layer_over, sketch_boxes, sketch_of};
use crate::time::{FrameIndex, WallClock};
use crate::tool::{ActiveTool, PointerFrame, Tool};
use crate::transport::Transport;

/// A stroke in progress.
#[derive(Debug, Clone)]
pub struct Live {
    /// Wall time of the press; sample and clock times are relative to it.
    pub start: f64,
    /// `[t, x, y]`, t ascending.
    pub samples: Vec<[f64; 3]>,
    pub clock: ClockMap,
    /// The transport rate at the press.
    pub rate: f64,
    /// The sketch this stroke edits; `None` = a new sketch on release.
    pub target: Option<Entity>,
    /// The target's parameters (or the defaults for a new sketch).
    pub params: SketchParams,
    pub stroke: Stroke,
    /// The stroke's own boxes (frames it visited).
    pub boxes: Option<Boxes>,
    /// The sketch with the stroke laid over it, on the frames that change.
    pub preview: Option<Boxes>,
}

impl Live {
    /// The previewed `[x, y, left, top, right, bottom]` at `f`, if the stroke changes `f`.
    pub fn preview_at(&self, f: FrameIndex) -> Option<[f64; 6]> {
        at(self.preview.as_ref()?, f)
    }

    /// Whether the stroke itself visited `f`.
    pub fn visits(&self, f: FrameIndex) -> bool {
        self.boxes.as_ref().is_some_and(|b| at(b, f).is_some())
    }
}

fn at((first, values): &Boxes, f: FrameIndex) -> Option<[f64; 6]> {
    values.get(usize::try_from(f - first).ok()?).copied().flatten()
}

#[derive(Resource, Debug, Default)]
pub struct LiveCapture(pub Option<Live>);

/// What new sketches and strokes start with.
#[derive(Resource, Debug, Default, Clone)]
pub struct SketchDefaults {
    pub params: SketchParams,
    /// The falloff last chosen with the wheel.
    pub stroke: Stroke,
}

/// Raw input only reports motion: a still pointer is extended to "now" once
/// it has been still this long.
const STILL: f64 = 0.02;
/// Falloff change per wheel notch, and its limits (seconds of video).
const WHEEL_STEP: f32 = 1.25;
const FALLOFF_MAX: f32 = 5.0;

/// Start, extend and commit strokes (`Set::Tools`, after the transport moved).
pub fn sketch_tool(world: &mut World) {
    let pointer = world.resource::<PointerFrame>().clone();
    let now = world.resource::<WallClock>().now;

    if world.resource::<LiveCapture>().0.is_none() {
        let armed = world.resource::<ActiveTool>().0 == Tool::Sketch && world.resource::<Transport>().has_media();
        let Some(start) = pointer.pressed.filter(|_| armed) else { return };
        let mods = world.resource::<KeysHeld>().mods;
        let selected = world.resource::<Selection>().primary();
        let target = selected.filter(|_| !mods.shift).and_then(|e| sketch_of(world, e));
        let defaults = world.resource::<SketchDefaults>().clone();
        let params = target.and_then(|t| world.get::<SketchParams>(t).cloned()).unwrap_or(defaults.params);
        // Ctrl: move only, keeping the region's size.
        let stroke = Stroke { size: if mods.ctrl { 0.0 } else { 1.0 }, ..defaults.stroke };
        let live = Live {
            start,
            samples: Vec::new(),
            clock: ClockMap::default(),
            rate: world.resource::<Transport>().rate,
            target,
            params,
            stroke,
            boxes: None,
            preview: None,
        };
        world.resource_mut::<LiveCapture>().0 = Some(live);
    }
    let mut live = world.resource_mut::<LiveCapture>().0.take().expect("live capture");
    world.resource_mut::<PointerFrame>().wheel_taken = true;
    if !world.resource_mut::<PendingActions>().take(|a| a == Action::Cancel).is_empty() {
        tracing::info!("stroke cancelled");
        return;
    }
    // The sketch being edited was deleted meanwhile (e.g. undone): the stroke starts a new one.
    if let Some(t) = live.target.filter(|t| !is_live_sketch(world, *t)) {
        tracing::info!("the sketch {t} went away during the stroke; it will start a new sketch");
        live.target = None;
    }
    if pointer.wheel != 0.0 {
        let f = (live.stroke.falloff.max(0.01) * WHEEL_STEP.powf(pointer.wheel)).min(FALLOFF_MAX);
        live.stroke.falloff = if f < 0.015 { 0.0 } else { f };
    }
    let end = pointer.released.or((!pointer.down).then_some(now));

    // Samples up to the release, kept in time order.
    let until = end.unwrap_or(f64::INFINITY);
    for s in &pointer.samples {
        let t = s[0] - live.start;
        if t >= 0.0 && s[0] <= until && live.samples.last().is_none_or(|l| t > l[0]) {
            live.samples.push([t, s[1], s[2]]);
        }
    }
    let t_now = (end.unwrap_or(now) - live.start).max(0.0);
    match live.samples.last().copied() {
        Some(l) if t_now - l[0] > STILL => live.samples.push([t_now, l[1], l[2]]),
        None => {
            if let Some(h) = pointer.hover {
                live.samples.push([t_now, h[0], h[1]]);
            }
        }
        _ => {}
    }

    // What was on screen (the transport is the user's; it is only recorded).
    let fps = {
        let t = world.resource::<Transport>();
        live.clock.push(t_now, t.playhead, t.playing);
        t.fps.as_f64()
    };
    live.boxes = sketch_boxes(&live.samples, &live.clock, &live.params, fps);
    live.preview = live.boxes.as_ref().map(|(first, boxes)| {
        let base = live.target.and_then(|t| world.get::<Output>(t)).and_then(|o| world.resource::<SignalStore>().get(o.0));
        let base = |f: FrameIndex| base.and_then(|s| s.get(f)).map(|v| std::array::from_fn(|c| v[c] as f64));
        layer_over(base, *first, boxes, live.stroke.falloff as f64 * fps, &live.stroke)
    });
    if end.is_some() {
        if pointer.click.is_some() {
            return; // a click selects (tool.rs); it doesn't record
        }
        commit(world, live);
    } else {
        world.resource_mut::<LiveCapture>().0 = Some(live);
    }
}

/// One undo step: the stroke's capture entity, laid over its sketch (or a new sketch).
fn commit(world: &mut World, live: Live) {
    if live.boxes.as_ref().is_none_or(|(_, b)| b.iter().all(Option::is_none)) {
        return; // nothing visited
    }
    let (sketches, strokes) = {
        let mut ops = world.query::<&Operator>();
        let sketches = ops.iter(world).filter(|o| o.kind == "sketch").count();
        let mut captures = world.query::<&Capture>();
        (sketches, captures.iter(world).count())
    };
    let target = live.target.filter(|t| is_live_sketch(world, *t));
    let label = match target.and_then(|t| world.get::<Name>(t)) {
        Some(name) => format!("Stroke on {name}"),
        None => "New sketch".to_string(),
    };
    let mut sketch = target;
    edit(world, &label, |tx| {
        let stream = tx.create_signal(STREAM_CHANNELS);
        let flat: Vec<f32> = live.samples.iter().flat_map(|s| s.map(|v| v as f32)).collect();
        tx.signal(stream).write(0, &flat);
        let capture = tx.spawn((
            Name::new(format!("Stroke {}", strokes + 1)),
            Capture { rate: live.rate, samples: live.samples.len() as u32 },
            live.clock.clone(),
            live.stroke.clone(),
            Output(stream),
        ));
        match sketch {
            Some(s) => tx.modify::<Inputs>(s, |i| i.0.push(("stroke".into(), capture))),
            None => {
                let out = tx.create_signal(BOX_CHANNELS);
                sketch = Some(tx.spawn((
                    Name::new(format!("Sketch {}", sketches + 1)),
                    Operator { kind: "sketch".into() },
                    Inputs(vec![("stroke".into(), capture)]),
                    Output(out),
                    live.params.clone(),
                )));
            }
        }
    });
    let sketch = sketch.expect("a sketch");
    world.resource_mut::<Selection>().select_only(sketch);
    world.resource_mut::<SketchDefaults>().stroke.falloff = live.stroke.falloff;
    let visited = live.boxes.as_ref().map_or(0, |(_, b)| b.iter().flatten().count());
    tracing::info!("{label}: {} samples over {:.2} s, {visited} frames visited", live.samples.len(), live.samples.last().map_or(0.0, |s| s[0]));
}

/// An enabled sketch (not deleted, not undone).
fn is_live_sketch(world: &World, e: Entity) -> bool {
    world.get_entity(e).is_ok_and(|r| !r.contains::<bevy_ecs::entity_disabling::Disabled>()) && crate::sketch::is_sketch(world, e)
}

pub struct CaptureModule;

impl Module for CaptureModule {
    fn build(&self, app: &mut AppBuilder) {
        app.declare::<LiveCapture>(Class::Session)
            .declare::<SketchDefaults>(Class::Session)
            .init_resource::<LiveCapture>()
            .init_resource::<SketchDefaults>();
    }
}
