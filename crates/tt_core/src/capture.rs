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
use crate::sketch::{BOX_CHANNELS, Boxes, Capture, ClockMap, STREAM_CHANNELS, SketchParams, Stroke, Through, compose, layer_over, sketch_of, stroke_frames, union_at, union_reach};
use crate::view::{ActiveView, SpaceMap, home_of, map_at};
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
    /// The target's path before the motion union, when the stroke began.
    base: std::collections::BTreeMap<FrameIndex, [f64; 6]>,
    /// The view the stroke is drawn in (None = the source) and its mapping
    /// for every frame shown during the stroke.
    pub drawn_in: Option<Entity>,
    pub through: std::collections::BTreeMap<FrameIndex, SpaceMap>,
    /// The sketch's home view (for the motion union): the target's, or `drawn_in` for a new sketch.
    home: Option<Entity>,
    /// The home view's framing per frame, looked up once.
    home_maps: std::collections::HashMap<FrameIndex, SpaceMap>,
    pub stroke: Stroke,
    /// The stroke's own frames (the frames it visited).
    pub boxes: Option<Boxes>,
    /// The sketch with the stroke laid over it (motion union included), on the frames that change.
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
        let fps = world.resource::<Transport>().fps.as_f64();
        let base = target.map(|t| compose(world, t, &params, fps)).unwrap_or_default();
        let drawn_in = world.resource::<ActiveView>().0;
        let home = match target {
            Some(t) => home_of(world, t),
            None => drawn_in,
        };
        let live = Live {
            start,
            samples: Vec::new(),
            clock: ClockMap::default(),
            rate: world.resource::<Transport>().rate,
            target,
            params,
            base,
            drawn_in,
            through: Default::default(),
            home,
            home_maps: Default::default(),
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
        live.base.clear();
        live.home = live.drawn_in;
        live.home_maps.clear();
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

    // What was on screen (the transport is the user's; it is only recorded),
    // and, inside a view, how that view framed every frame shown since.
    let fps = {
        let t = world.resource::<Transport>().clone();
        let prev = live.clock.frame.last().map_or(t.frame(), |f| f.floor() as FrameIndex);
        let was_playing = live.clock.playing.last().copied().unwrap_or(false);
        live.clock.push(t_now, t.playhead, t.playing);
        if live.drawn_in.is_some() {
            // Playback shows every frame in between; a jump (seek, step, scrub) only its landing.
            let continuous = was_playing && (0..=64).contains(&(t.frame() - prev));
            let from = if continuous { prev } else { t.frame() };
            for f in from..=t.frame() {
                if !live.through.contains_key(&f) || f == t.frame() {
                    let m = map_at(world, live.drawn_in, f);
                    live.through.insert(f, m);
                }
            }
        }
        t.fps.as_f64()
    };
    live.boxes = stroke_frames(&live.samples, &live.clock, &live.params, fps).map(|(first, mut frames)| {
        if live.drawn_in.is_some() {
            for (i, v) in frames.iter_mut().enumerate() {
                *v = v.zip(live.through.get(&(first + i as FrameIndex))).map(|(b, m)| m.box_to_source(b));
            }
        }
        (first, frames)
    });
    live.preview = live.boxes.as_ref().map(|(first, boxes)| {
        let (lo, layered) = layer_over(|f| live.base.get(&f).copied(), *first, boxes, live.stroke.falloff as f64 * fps, &live.stroke);
        // The motion union over the frames the change can reach.
        let get = |f: FrameIndex| layered.get(usize::try_from(f - lo).ok()?).copied().flatten().or_else(|| live.base.get(&f).copied());
        let (before, after) = union_reach(&live.params, fps);
        let (from, to) = (lo - after, lo + layered.len() as FrameIndex + before);
        // The home view doesn't change during a stroke: its framing is looked up once per frame.
        if live.home.is_some() {
            for f in from - before..to + after {
                live.home_maps.entry(f).or_insert_with(|| map_at(world, live.home, f));
            }
        }
        let maps = |f: FrameIndex| live.home_maps.get(&f).copied();
        let union = |f: FrameIndex| match maps(f) {
            None => union_at(get, f, before, after),
            Some(at_f) => union_at(
                |g| {
                    let v = get(g)?;
                    Some(match maps(g) {
                        Some(at_g) if g != f => at_f.box_to_source(at_g.box_from_source(v)),
                        _ => v,
                    })
                },
                f,
                before,
                after,
            ),
        };
        (from, (from..to).map(union).collect())
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
        if live.drawn_in.is_some() {
            let through = tx.create_signal(3);
            let sig = tx.signal(through);
            for (f, m) in &live.through {
                sig.set(*f, &m.channels());
            }
            tx.insert(capture, Through(through));
        }
        match sketch {
            Some(s) => tx.modify::<Inputs>(s, |i| i.0.push(("stroke".into(), capture))),
            None => {
                let out = tx.create_signal(BOX_CHANNELS);
                let mut inputs = vec![("stroke".to_string(), capture)];
                inputs.extend(live.drawn_in.map(|v| ("space".to_string(), v)));
                sketch = Some(tx.spawn((
                    Name::new(format!("Sketch {}", sketches + 1)),
                    Operator { kind: "sketch".into() },
                    Inputs(inputs),
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
