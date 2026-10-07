//! Layers (on request: "attach an image/gif/video to that tracking data …
//! like after effects"): a picture, GIF or video clip attached to anything
//! with a position (a sketch, a tracker, a subject), moving with it.
//!
//! A layer is an operator (`layer`) whose input `target` is what it is
//! attached to and whose output is where it sits on every frame the target
//! has ([`LAYER_CHANNELS`]): `[x, y, angle, scale x, scale y, opacity, clip
//! time, anchor x, anchor y, flags]`. The anchor (a point of the picture,
//! as fractions of its width and height) lands on `(x, y)` (source pixels),
//! the picture turned by `angle` (radians, clockwise on screen) and scaled
//! from its own pixels; the clip time (seconds) says which of a clip's
//! frames shows. A frame with no output is a frame it doesn't show (before
//! its target, or after a clip that plays once and goes).
//!
//! Its parameters ([`LayerParams`]):
//! - what follows the target: its position always; its angle when it has
//!   one (a subject) and `follow_rotation` is on; its box's size
//!   ([`SizeMode`]) relative to the box on a reference frame;
//! - its own transform on top, each value fixed or keyed ([`Animated`]:
//!   linear between keys, held beyond): offset (in the target's own turned
//!   frame when it follows rotation), scale, rotation, opacity, anchor;
//! - a clip's timing: where in the clip it starts, its speed, and what
//!   happens at its end ([`EndMode`]).
//!
//! Lost frames of a tracker (flags) are bridged, as a view does. The Select
//! tool drags a layer by its picture: its offset changes on the shown frame,
//! keyed when that offset already has keys or [`AutoKey`] is on, else its
//! fixed value (one undo step per drag).

use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::name::Name;
use bevy_ecs::prelude::*;
use bevy_reflect::Reflect;

use crate::app::{AppBuilder, Module, Set};
use crate::history::{History, edit};
use crate::meta::Class;
use crate::op::{EvalCtx, Footprint, Inputs, Operator, OperatorKind, Output};
use crate::selection::Selection;
use crate::signal::{Signal, SignalStore};
use crate::time::FrameIndex;
use crate::tool::{ActiveTool, CLICK_MOVE, PointerFrame, Tool};
use crate::transport::Transport;
use crate::view::{ActiveView, map_at};

/// `[x, y, angle, scale x, scale y, opacity, clip time, anchor x, anchor y, flags]`.
pub const LAYER_CHANNELS: usize = 10;
/// The input slot of what a layer is attached to.
pub const TARGET: &str = "target";

/// A value of a layer, fixed or keyed.
#[derive(Reflect, Clone, Debug, PartialEq, Default)]
pub struct Animated {
    /// The value when there are no keys.
    pub value: f32,
    /// Keys in frame order: linear between them, held beyond.
    pub keys: Vec<Key>,
}

/// A key of an [`Animated`] value.
#[derive(Reflect, Clone, Copy, Debug, PartialEq, Default)]
pub struct Key {
    pub frame: FrameIndex,
    pub value: f32,
}

impl Animated {
    pub fn fixed(value: f32) -> Self {
        Self { value, keys: Vec::new() }
    }

    /// The value on frame `f`.
    pub fn at(&self, f: FrameIndex) -> f32 {
        let k = &self.keys;
        match k.len() {
            0 => self.value,
            _ if f <= k[0].frame => k[0].value,
            n if f >= k[n - 1].frame => k[n - 1].value,
            _ => {
                let i = k.partition_point(|x| x.frame <= f);
                let (a, b) = (k[i - 1], k[i]);
                let u = (f - a.frame) as f32 / (b.frame - a.frame).max(1) as f32;
                a.value + (b.value - a.value) * u
            }
        }
    }

    /// Set the value on frame `f`: a key there when it has keys or `key`
    /// (auto-key), else its fixed value.
    pub fn set(&mut self, f: FrameIndex, value: f32, key: bool) {
        if self.keys.is_empty() && !key {
            self.value = value;
            return;
        }
        let i = self.keys.partition_point(|k| k.frame < f);
        match self.keys.get_mut(i).filter(|k| k.frame == f) {
            Some(k) => k.value = value,
            None => self.keys.insert(i, Key { frame: f, value }),
        }
    }

    /// A key on frame `f` (the value there now), or none: toggled.
    pub fn toggle_key(&mut self, f: FrameIndex) {
        match self.keys.iter().position(|k| k.frame == f) {
            Some(i) => {
                let v = self.at(f);
                self.keys.remove(i);
                if self.keys.is_empty() {
                    self.value = v;
                }
            }
            None => {
                let v = self.at(f);
                self.set(f, v, true);
            }
        }
    }

    pub fn has_key(&self, f: FrameIndex) -> bool {
        self.keys.iter().any(|k| k.frame == f)
    }
}

/// How a layer's size follows its target's box.
#[derive(Reflect, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SizeMode {
    /// Its own scale only.
    #[default]
    Fixed,
    /// With the box's area (the mean of its width and height ratios, uniformly).
    Box,
    /// With the box's width.
    Width,
    /// With the box's height.
    Height,
}

/// What a clip does at its end.
#[derive(Reflect, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum EndMode {
    /// From its start again.
    #[default]
    Loop,
    /// Its last frame stays.
    Hold,
    /// The layer is gone after it.
    Hide,
    /// Backward to its start, then forward again.
    PingPong,
}

/// A layer's parameters (its operator's params).
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component)]
pub struct LayerParams {
    /// The media file (a picture, GIF or video clip).
    pub media: String,
    /// Its size in its own pixels.
    pub media_size: [f32; 2],
    /// A clip's (or GIF's) length in seconds; 0: a still picture.
    pub clip_duration: f32,
    /// A clip's frames per second (0: a still picture).
    pub clip_fps: f32,
    /// Turn with the target, when it has an angle (a subject).
    pub follow_rotation: bool,
    pub size: SizeMode,
    /// The frame whose box size is the layer's own size (None: its first frame).
    pub size_frame: Option<FrameIndex>,
    /// Where in the clip it starts (seconds), and how fast it plays.
    pub clip_in: f32,
    pub speed: f32,
    pub end: EndMode,
    /// Offset from the target's point (source px; in its turned frame when following rotation).
    pub offset_x: Animated,
    pub offset_y: Animated,
    /// A multiplier on the picture's own pixels.
    pub scale: Animated,
    /// Degrees, clockwise on screen, on top of the target's.
    pub rotation: Animated,
    /// 0 to 1.
    pub opacity: Animated,
    /// The point of the picture on the target's point: fractions of its width and height.
    pub anchor_x: Animated,
    pub anchor_y: Animated,
}

impl Default for LayerParams {
    fn default() -> Self {
        Self {
            media: String::new(),
            media_size: [100.0, 100.0],
            clip_duration: 0.0,
            clip_fps: 0.0,
            follow_rotation: true,
            size: SizeMode::Fixed,
            size_frame: None,
            clip_in: 0.0,
            speed: 1.0,
            end: EndMode::Loop,
            offset_x: Animated::fixed(0.0),
            offset_y: Animated::fixed(0.0),
            scale: Animated::fixed(1.0),
            rotation: Animated::fixed(0.0),
            opacity: Animated::fixed(1.0),
            anchor_x: Animated::fixed(0.5),
            anchor_y: Animated::fixed(0.5),
        }
    }
}

impl LayerParams {
    /// The animated values, with their names (the Inspector, the timeline's keys).
    pub fn values(&self) -> [(&'static str, &Animated); 7] {
        [
            ("Offset X", &self.offset_x),
            ("Offset Y", &self.offset_y),
            ("Scale", &self.scale),
            ("Rotation", &self.rotation),
            ("Opacity", &self.opacity),
            ("Anchor X", &self.anchor_x),
            ("Anchor Y", &self.anchor_y),
        ]
    }

    pub fn values_mut(&mut self) -> [(&'static str, &mut Animated); 7] {
        [
            ("Offset X", &mut self.offset_x),
            ("Offset Y", &mut self.offset_y),
            ("Scale", &mut self.scale),
            ("Rotation", &mut self.rotation),
            ("Opacity", &mut self.opacity),
            ("Anchor X", &mut self.anchor_x),
            ("Anchor Y", &mut self.anchor_y),
        ]
    }

    /// Every frame any value has a key on.
    pub fn key_frames(&self) -> Vec<FrameIndex> {
        let mut v: Vec<FrameIndex> = self.values().iter().flat_map(|(_, a)| a.keys.iter().map(|k| k.frame)).collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// The clip time (seconds) `secs` after the layer starts, after its end
    /// mode; None: not shown then (a clip that played once and went).
    pub fn clip_time(&self, secs: f64) -> Option<f64> {
        let d = self.clip_duration as f64;
        if d <= 0.0 {
            return Some(0.0);
        }
        let t = (self.clip_in as f64 + secs * self.speed as f64).max(0.0);
        // The last frame starts one frame before the end.
        let last = (d - 1.0 / (self.clip_fps.max(1.0) as f64)).max(0.0);
        match self.end {
            EndMode::Loop => Some(t.rem_euclid(d)),
            EndMode::Hold => Some(t.min(last)),
            EndMode::Hide => (t < d).then_some(t),
            EndMode::PingPong => {
                let c = t.rem_euclid(2.0 * d);
                Some(if c < d { c } else { (2.0 * d - c).min(last) })
            }
        }
    }
}

/// Whether to key a value the user changes on the shown frame when it has no keys yet.
#[derive(Resource, Debug, Default, Clone, Copy)]
pub struct AutoKey(pub bool);

/// What layer `e` is attached to.
pub fn target_of(world: &World, e: Entity) -> Option<Entity> {
    world.get::<Inputs>(e)?.0.iter().find(|(s, _)| s == TARGET).map(|(_, t)| *t)
}

/// Whether `e` is a layer.
pub fn is_layer(world: &World, e: Entity) -> bool {
    world.get::<Operator>(e).is_some_and(|o| o.kind == "layer")
}

/// Whether `e` has an angle a layer can turn with (a subject's).
pub fn has_angle(world: &World, e: Entity) -> bool {
    world.get::<Operator>(e).is_some_and(|o| o.kind == "subject")
}

/// A layer placed on a frame: its output's value there.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placed {
    pub at: [f64; 2],
    pub angle: f64,
    pub scale: [f64; 2],
    pub opacity: f64,
    pub clip_time: f64,
    pub anchor: [f64; 2],
}

impl Placed {
    pub fn of(v: &[f32]) -> Option<Self> {
        (v.len() >= LAYER_CHANNELS).then(|| {
            let c = |i: usize| v[i] as f64;
            Self { at: [c(0), c(1)], angle: c(2), scale: [c(3), c(4)], opacity: c(5), clip_time: c(6), anchor: [c(7), c(8)] }
        })
    }

    /// Where a point of the picture (its own pixels) lands (source px).
    pub fn to_source(&self, size: [f32; 2], p: [f64; 2]) -> [f64; 2] {
        let (w, h) = (size[0] as f64, size[1] as f64);
        let (dx, dy) = ((p[0] - self.anchor[0] * w) * self.scale[0], (p[1] - self.anchor[1] * h) * self.scale[1]);
        let (s, c) = self.angle.sin_cos();
        [self.at[0] + c * dx - s * dy, self.at[1] + s * dx + c * dy]
    }

    /// Where a point in source px is on the picture (its own pixels).
    pub fn from_source(&self, size: [f32; 2], q: [f64; 2]) -> [f64; 2] {
        let (w, h) = (size[0] as f64, size[1] as f64);
        let (s, c) = self.angle.sin_cos();
        let (x, y) = (q[0] - self.at[0], q[1] - self.at[1]);
        let (dx, dy) = (c * x + s * y, -s * x + c * y);
        [dx / self.scale[0].max(1e-9) + self.anchor[0] * w, dy / self.scale[1].max(1e-9) + self.anchor[1] * h]
    }

    /// The picture's corners (source px): top left, top right, bottom right, bottom left.
    pub fn corners(&self, size: [f32; 2]) -> [[f64; 2]; 4] {
        let (w, h) = (size[0] as f64, size[1] as f64);
        [[0.0, 0.0], [w, 0.0], [w, h], [0.0, h]].map(|p| self.to_source(size, p))
    }

    pub fn contains(&self, size: [f32; 2], q: [f64; 2]) -> bool {
        let p = self.from_source(size, q);
        (0.0..=size[0] as f64).contains(&p[0]) && (0.0..=size[1] as f64).contains(&p[1])
    }
}

/// Layer `e` on frame `f`, if it shows there.
pub fn placed_at(world: &World, e: Entity, f: FrameIndex) -> Option<Placed> {
    if !crate::span::span_of(world, e).contains(f) {
        return None;
    }
    let v = world.resource::<SignalStore>().get(world.get::<Output>(e)?.0)?.get(f)?;
    Placed::of(v)
}

/// The live layers, bottom to top (the order they were made).
pub fn layers(world: &mut World) -> Vec<Entity> {
    let mut q = world.query_filtered::<(Entity, &Operator), Without<Disabled>>();
    let mut v: Vec<Entity> = q.iter(world).filter(|(_, o)| o.kind == "layer").map(|(e, _)| e).collect();
    crate::meta::creation_order(world, &mut v);
    v
}

/// The top layer whose picture is under `pos` (source px) on frame `f`.
pub fn pick_layer(world: &mut World, f: FrameIndex, pos: [f64; 2]) -> Option<Entity> {
    layers(world).into_iter().rev().find(|e| {
        let Some(size) = world.get::<LayerParams>(*e).map(|p| p.media_size) else { return false };
        placed_at(world, *e, f).is_some_and(|pl| pl.opacity > 0.0 && pl.contains(size, pos))
    })
}

/// Which layer a Select-tool press at `src` (source px) on frame `f` drags,
/// if any: the selected layer when it is under it; none when the selected
/// subject's point is (within `grab` px: the subject's drag takes it); else
/// the top layer under it.
pub fn press_on_layer(world: &mut World, f: FrameIndex, src: [f64; 2], grab: f64) -> Option<Entity> {
    if let Some(p) = world.resource::<Selection>().primary() {
        if is_layer(world, p) {
            let size = world.get::<LayerParams>(p).map(|q| q.media_size);
            if let (Some(size), Some(pl)) = (size, placed_at(world, p, f))
                && pl.contains(size, src)
            {
                return Some(p);
            }
        } else if crate::subject::pick_subject(world, f, src, grab) == Some(p) {
            return None;
        }
    }
    pick_layer(world, f, src)
}

/// Attach a new layer showing `params.media` to `target`, as one undo step;
/// it is selected. Its scale starts so the picture is about as tall as the
/// target's box on `frame` (or its own size when the box isn't known).
pub fn attach(world: &mut World, target: Entity, mut params: LayerParams, frame: FrameIndex) -> Option<Entity> {
    let store = world.resource::<SignalStore>();
    let box_h = world.get::<Output>(target).and_then(|o| store.get(o.0)).and_then(|s| s.get(frame).or_else(|| s.present_hull().and_then(|(a, _)| s.get(a)))).map(|v| (v[5] - v[3]) as f64);
    if let Some(h) = box_h.filter(|h| *h > 1.0) {
        params.scale = Animated::fixed((h / params.media_size[1].max(1.0) as f64) as f32);
    }
    params.follow_rotation = has_angle(world, target);
    let name = std::path::Path::new(&params.media).file_stem().map_or("Layer".to_string(), |s| s.to_string_lossy().into_owned());
    let mut made = None;
    edit(world, &format!("Attach {name}"), |tx| {
        let out = tx.create_signal(LAYER_CHANNELS);
        made = Some(tx.spawn((Name::new(name.clone()), Operator { kind: "layer".into() }, Inputs(vec![(TARGET.to_string(), target)]), Output(out), params)));
    });
    if let Some(l) = made {
        world.resource_mut::<Selection>().select_only(l);
    }
    made
}

/// Change a layer's parameters, as one undo step (or part of an open gesture).
pub fn set_params(world: &mut World, e: Entity, label: &str, f: impl FnOnce(&mut LayerParams)) {
    edit(world, label, |tx| tx.modify::<LayerParams>(e, f));
}

/// Re-attach layer `e` to `target` (one undo step).
pub fn reattach(world: &mut World, e: Entity, target: Entity) {
    edit(world, "Attach to", |tx| {
        tx.modify::<Inputs>(e, |i| {
            i.0.retain(|(s, _)| s != TARGET);
            i.0.push((TARGET.to_string(), target));
        })
    });
}

// ---- the operator ---------------------------------------------------------------------------

/// `layer`: where a layer sits on every frame of its target (module docs).
pub struct LayerKind;

impl OperatorKind for LayerKind {
    fn name(&self) -> &'static str {
        "layer"
    }

    fn channels(&self) -> usize {
        LAYER_CHANNELS
    }

    fn footprint(&self, _: EntityRef<'_>) -> Footprint {
        Footprint::Global
    }

    fn evaluate(&self, ctx: &EvalCtx<'_>, range: std::ops::Range<FrameIndex>, out: &mut Signal) -> anyhow::Result<()> {
        out.clear(range.clone());
        let p = ctx.params::<LayerParams>().cloned().unwrap_or_default();
        let fps = ctx.world.get_resource::<Transport>().map_or(60.0, |t| t.fps.as_f64());
        let Some(sig) = ctx.input(TARGET) else { return Ok(()) };
        let angled = target_of(ctx.world, ctx.entity).is_some_and(|t| has_angle(ctx.world, t));
        let start = crate::span::span_of(ctx.world, ctx.entity).first;
        let Some((first, frames)) = place(sig, &p, angled, start, fps) else { return Ok(()) };
        for (i, v) in frames.iter().enumerate() {
            let f = first + i as FrameIndex;
            if range.contains(&f)
                && let Some(v) = v
            {
                out.set(f, &v.map(|x| x as f32));
            }
        }
        Ok(())
    }
}

/// The placement on every frame from the target's first to its last
/// (`None`: not shown that frame). `angled`: the target's channel 6 is its
/// angle. `start`: the layer's first frame when its span is trimmed.
pub fn place(target: &Signal, p: &LayerParams, angled: bool, start: Option<FrameIndex>, fps: f64) -> Option<(FrameIndex, Vec<Option<[f64; LAYER_CHANNELS]>>)> {
    let (lo, hi) = target.present_hull()?;
    let n = (hi - lo + 1) as usize;
    // `[x, y, width, height, angle]` per frame; a tracker's flagged frames (channel 7) bridged.
    let mut known: Vec<Option<[f64; 5]>> = (lo..=hi)
        .map(|f| {
            target.get(f).filter(|v| v.len() >= 6 && (angled || v.get(7).is_none_or(|fl| *fl == 0.0))).map(|v| {
                let c = |i: usize| v[i] as f64;
                [c(0), c(1), (c(4) - c(2)).abs(), (c(5) - c(3)).abs(), if angled { c(6) } else { 0.0 }]
            })
        })
        .collect();
    known.iter().any(Option::is_some).then_some(())?;
    fill(&mut known);
    let at = |f: FrameIndex| known[(f.clamp(lo, hi) - lo) as usize].expect("filled");
    let first = start.unwrap_or(lo).clamp(lo, hi);
    let r = at(p.size_frame.unwrap_or(first));
    let (w0, h0) = (r[2].max(1e-6), r[3].max(1e-6));
    let out = (lo..=hi)
        .map(|f| {
            let [x, y, w, h, a] = at(f);
            let turn = if p.follow_rotation && angled { a } else { 0.0 };
            let (s, c) = turn.sin_cos();
            let (ox, oy) = (p.offset_x.at(f) as f64, p.offset_y.at(f) as f64);
            let follow = match p.size {
                SizeMode::Fixed => 1.0,
                SizeMode::Box => (w / w0 + h / h0) / 2.0,
                SizeMode::Width => w / w0,
                SizeMode::Height => h / h0,
            };
            let scale = p.scale.at(f) as f64 * follow;
            let t = p.clip_time((f - first) as f64 / fps)?;
            Some([
                x + c * ox - s * oy,
                y + s * ox + c * oy,
                turn + (p.rotation.at(f) as f64).to_radians(),
                scale,
                scale,
                (p.opacity.at(f) as f64).clamp(0.0, 1.0),
                t,
                p.anchor_x.at(f) as f64,
                p.anchor_y.at(f) as f64,
                0.0,
            ])
        })
        .collect::<Vec<_>>();
    debug_assert_eq!(out.len(), n);
    Some((lo, out))
}

/// Linear across `None` runs; the ends hold the nearest known value.
fn fill(v: &mut [Option<[f64; 5]>]) {
    let known: Vec<usize> = (0..v.len()).filter(|i| v[*i].is_some()).collect();
    let (Some(&a), Some(&b)) = (known.first(), known.last()) else { return };
    let (head, tail) = (v[a], v[b]);
    v[..a].fill(head);
    v[b + 1..].fill(tail);
    for w in known.windows(2) {
        let (i, j) = (w[0], w[1]);
        let (p, q) = (v[i].unwrap(), v[j].unwrap());
        for (k, slot) in v[i + 1..j].iter_mut().enumerate() {
            let u = (k + 1) as f64 / (j - i) as f64;
            *slot = Some(std::array::from_fn(|c| p[c] + (q[c] - p[c]) * u));
        }
    }
}

// ---- dragging -------------------------------------------------------------------------------

/// The Select tool's drag of a layer.
#[derive(Clone, Copy, Debug)]
struct Grab {
    layer: Entity,
    /// The press (source px), the frame, its offset then, the target's turn then.
    from: [f64; 2],
    frame: FrameIndex,
    offset: [f32; 2],
    turn: f64,
    moved: bool,
}

#[derive(Resource, Debug, Default)]
struct LayerDrag(Option<Grab>);

/// `Set::Tools`: in the Select tool, a press on a layer's picture and a
/// drag moves it (module docs). A press that doesn't move is a click, which
/// selects it (tool.rs).
fn drag_layer(world: &mut World) {
    if world.resource::<ActiveTool>().0 != Tool::Select {
        if world.resource_mut::<LayerDrag>().0.take().is_some_and(|d| d.moved) {
            world.resource_mut::<History>().end();
        }
        return;
    }
    let p = world.resource::<PointerFrame>().clone();
    let frame = world.resource::<Transport>().frame();
    let map = map_at(world, world.resource::<ActiveView>().0, frame);
    let scale = if p.scale > 0.0 { p.scale } else { 1.0 };
    if let Some(t) = p.pressed
        && let Some(at) = p.samples.iter().find(|s| s[0] >= t).map(|s| [s[1], s[2]]).or(p.hover)
    {
        let src = map.to_source(at);
        let grab = (12.0 / scale) * map.a;
        if let Some(e) = press_on_layer(world, frame, src, grab)
            && let (Some(params), Some(pl)) = (world.get::<LayerParams>(e), placed_at(world, e, frame))
        {
            let turn = pl.angle - (params.rotation.at(frame) as f64).to_radians();
            let off = [params.offset_x.at(frame), params.offset_y.at(frame)];
            world.resource_mut::<LayerDrag>().0 = Some(Grab { layer: e, from: src, frame, offset: off, turn, moved: false });
        }
    }
    let Some(g) = world.resource::<LayerDrag>().0 else { return };
    let Grab { layer: e, from, frame: f, offset: off, turn, moved } = g;
    let ended = p.released.is_some() || !p.down;
    if let Some(now) = p.samples.last().map(|s| [s[1], s[2]]).or(p.hover) {
        let src = map.to_source(now);
        let (dx, dy) = (src[0] - from[0], src[1] - from[1]);
        let far = dx.hypot(dy) * scale / map.a.max(1e-9) >= CLICK_MOVE;
        if moved || far {
            if !moved {
                let name = world.get::<Name>(e).map_or("layer".to_string(), |n| n.to_string());
                world.resource_mut::<History>().begin(format!("Move {name}"));
                world.resource_mut::<Selection>().select_only(e);
                world.resource_mut::<LayerDrag>().0 = Some(Grab { moved: true, ..g });
            }
            // The move in the target's own turned frame (the offset's).
            let (s, c) = turn.sin_cos();
            let (lx, ly) = (c * dx + s * dy, -s * dx + c * dy);
            let key = world.resource::<AutoKey>().0;
            set_params(world, e, "Move", |p| {
                p.offset_x.set(f, off[0] + lx as f32, key);
                p.offset_y.set(f, off[1] + ly as f32, key);
            });
        }
    }
    if ended && world.resource_mut::<LayerDrag>().0.take().is_some_and(|d| d.moved) {
        world.resource_mut::<History>().end();
    }
}

pub struct LayerModule;

impl Module for LayerModule {
    fn build(&self, app: &mut AppBuilder) {
        app.operator(LayerKind)
            .operator_params::<LayerParams>()
            .declare::<AutoKey>(Class::Session)
            .declare::<LayerDrag>(Class::Derived)
            .init_resource::<AutoKey>()
            .init_resource::<LayerDrag>()
            .add_systems(drag_layer.in_set(Set::Tools));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_linear_between_and_held_beyond() {
        let mut a = Animated::fixed(2.0);
        assert_eq!(a.at(50), 2.0);
        a.set(10, 0.0, false);
        assert_eq!((a.value, a.keys.len()), (0.0, 0), "no keys and no auto-key: the fixed value");
        a.set(10, 0.0, true);
        a.set(20, 10.0, false);
        assert_eq!(a.keys.len(), 2, "once keyed, changes key");
        assert_eq!([a.at(0), a.at(15), a.at(30)], [0.0, 5.0, 10.0]);
        a.toggle_key(20);
        assert_eq!(a.keys.len(), 1);
    }

    #[test]
    fn clips_loop_hold_hide_and_ping_pong() {
        let p = |end| LayerParams { clip_duration: 2.0, clip_fps: 10.0, end, ..LayerParams::default() };
        assert_eq!(p(EndMode::Loop).clip_time(2.5), Some(0.5));
        assert!((p(EndMode::Hold).clip_time(5.0).unwrap() - 1.9).abs() < 1e-9, "the last frame");
        assert_eq!(p(EndMode::Hide).clip_time(2.5), None);
        assert_eq!(p(EndMode::PingPong).clip_time(2.5), Some(1.5));
        let fast = LayerParams { speed: 2.0, clip_in: 0.5, ..p(EndMode::Loop) };
        assert_eq!(fast.clip_time(0.5), Some(1.5));
        assert_eq!(LayerParams::default().clip_time(9.0), Some(0.0), "a picture is always on");
    }

    #[test]
    fn a_layer_follows_its_target_and_its_box() {
        // A box moving right 2 px a frame and doubling in height from frame 0 to 10.
        let mut s = Signal::new(8);
        for f in 0..=10 {
            let (x, h) = (100.0 + 2.0 * f as f32, 20.0 + 2.0 * f as f32);
            s.set(f, &[x, 50.0, x - 10.0, 50.0 - h / 2.0, x + 10.0, 50.0 + h / 2.0, 1.0, 0.0]);
        }
        let mut p = LayerParams { offset_x: Animated::fixed(5.0), size: SizeMode::Height, ..LayerParams::default() };
        let (first, v) = place(&s, &p, false, None, 60.0).unwrap();
        assert_eq!(first, 0);
        assert_eq!([v[10].unwrap()[0], v[10].unwrap()[1]], [125.0, 50.0], "the point plus the offset");
        assert!((v[10].unwrap()[3] - 2.0).abs() < 1e-9, "twice as tall as on its first frame: twice the scale");
        p.size = SizeMode::Fixed;
        assert_eq!(place(&s, &p, false, None, 60.0).unwrap().1[10].unwrap()[3], 1.0);
        // A placed picture maps its corners and back.
        let pl = Placed { at: [100.0, 50.0], angle: 0.3, scale: [2.0, 2.0], opacity: 1.0, clip_time: 0.0, anchor: [0.5, 0.5] };
        let q = pl.to_source([40.0, 20.0], [5.0, 7.0]);
        let back = pl.from_source([40.0, 20.0], q);
        assert!((back[0] - 5.0).abs() < 1e-9 && (back[1] - 7.0).abs() < 1e-9);
        assert_eq!(pl.to_source([40.0, 20.0], [20.0, 10.0]), [100.0, 50.0], "the anchor lands on the point");
    }
}
