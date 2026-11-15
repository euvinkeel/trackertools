//! Subjects (DESIGN §7, "Target"): what several trackers, sketches, or any
//! other points follow together. A subject is an operator whose inputs
//! (`member`) are those points' producers.
//!
//! **The pushed position** (from v1). A subject is not the mean of its
//! members' points: that jumps whenever one comes or goes. It starts at their
//! mean on its anchor frame; from there, both ways, it moves frame by frame
//! with the members that have a good point on both neighbouring frames: by
//! their mean shift and, with `turn` and two or more of them, by their turn,
//! which carries it about their centre (a least-squares rigid fit). Members
//! coming or going, lost or outside (a tracker's flags, channel 7), change
//! only how many are averaged; a frame where none moves holds it still.
//! While the same members carry it, its steps add up to exactly their own
//! motion (the sum telescopes), so it doesn't drift; where they change, a
//! step's error can stay, and an offset key puts it right.
//!
//! **Its own transform.** Offset keys (position and angle; linear between
//! keys, held beyond them) say where on the moving thing the subject is, in
//! its anchor frame's pixels, so an offset turns with the thing. Dragging the
//! selected subject in the Select tool keys it on the shown frame.
//!
//! **Output** ([`SUBJECT_CHANNELS`]): `[x, y, left, top, right, bottom,
//! angle, flags, pushed x, pushed y, pushed angle]` in source pixels, y down;
//! angles in radians, clockwise on screen (y down), 0 on the anchor frame;
//! the box is `half` around the point; flags 0. The pushed values are the
//! members' motion alone, without the offset: what a drag keys from.

use std::collections::{BTreeMap, HashMap};

use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::name::Name;
use bevy_ecs::prelude::*;
use bevy_ecs::world::EntityRef;
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

/// `[x, y, left, top, right, bottom, angle, flags, pushed x, pushed y, pushed angle]`.
pub const SUBJECT_CHANNELS: usize = 11;
/// The input slot of a subject's members.
pub const MEMBER: &str = "member";

/// A subject's parameters (its operator's params).
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component)]
pub struct Subject {
    /// It starts here, at its members' mean, and their motion carries it both ways.
    pub anchor: FrameIndex,
    /// Turn with the members (two or more on a step): their turn carries it about their centre.
    pub turn: bool,
    /// Its box: half width and half height around the point (px).
    pub half: [f32; 2],
    /// Its own transform over time: where on the moving thing it is, keyed.
    pub offsets: Vec<OffsetKey>,
    /// Seconds (video time) of smoothing on its members' motion, over the
    /// whole path (0: none). Its keys go on top, so a key stays exactly
    /// where it was put.
    #[reflect(default)]
    pub smooth: f32,
    /// How its offset goes from one key to the next.
    #[reflect(default)]
    pub ease: Ease,
}

impl Default for Subject {
    fn default() -> Self {
        Self { anchor: 0, turn: true, half: [48.0, 48.0], offsets: Vec::new(), smooth: 0.0, ease: Ease::Smooth }
    }
}

/// How a subject's offset goes from one key to the next.
#[derive(Reflect, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Ease {
    /// In a straight line (subjects made before pins on the path existed).
    #[default]
    Linear,
    /// Easing out of one key and into the next: no corner at a key, never
    /// past one, and each stretch depends on its two keys alone. New subjects.
    Smooth,
}

/// A key of a subject's own transform: on `frame`, its point is (`x`, `y`)
/// from where its members' motion puts it, in its anchor frame's pixels
/// (turning with the thing), and it is turned `angle` degrees more (clockwise
/// on screen).
#[derive(Reflect, Clone, Copy, Debug, PartialEq, Default)]
pub struct OffsetKey {
    pub frame: FrameIndex,
    pub x: f32,
    pub y: f32,
    pub angle: f32,
}

pub fn is_subject(world: &World, e: Entity) -> bool {
    world.get::<Operator>(e).is_some_and(|o| o.kind == "subject")
}

/// A subject's members (deleted ones left out).
pub fn members_of(world: &World, subject: Entity) -> Vec<Entity> {
    world
        .get::<Inputs>(subject)
        .map(|i| i.0.iter().filter(|(slot, _)| slot == MEMBER).map(|(_, e)| *e).filter(|e| world.get::<Disabled>(*e).is_none()).collect())
        .unwrap_or_default()
}

/// The subjects `member` belongs to.
pub fn subjects_of(world: &mut World, member: Entity) -> Vec<Entity> {
    let mut q = world.query_filtered::<(Entity, &Operator, &Inputs), Without<Disabled>>();
    q.iter(world).filter(|(_, o, i)| o.kind == "subject" && i.0.iter().any(|(s, e)| s == MEMBER && *e == member)).map(|(e, ..)| e).collect()
}

/// Whether `e` can be a member: anything with a point a frame (a tracker, a
/// sketch, another subject), not a view (a cycle is the graph's to refuse).
pub fn can_be_member(world: &World, e: Entity) -> bool {
    world.get::<Output>(e).is_some() && world.get::<Operator>(e).is_none_or(|o| o.kind != "frame")
}

/// A producer's point on each frame it has a good one: inside its span, not
/// flagged (a tracker's lost or outside, channel 7), finite. Source px, y down.
pub fn points_of(world: &World, e: Entity) -> Vec<(FrameIndex, [f64; 2])> {
    let Some(sig) = crate::span::output(world, e) else { return Vec::new() };
    let Some((lo, hi)) = sig.present_hull() else { return Vec::new() };
    (lo..=hi)
        .filter_map(|f| {
            let v = sig.get(f)?;
            let good = v.get(7).is_none_or(|flags| *flags == 0.0) && v[0].is_finite() && v[1].is_finite();
            good.then(|| (f, [v[0] as f64, v[1] as f64]))
        })
        .collect()
}

/// The pushed path (module doc): `[x, y, angle]` on every frame from the
/// members' first point to their last. `anchor` moves to the nearest frame
/// with a point if it has none.
pub fn push(members: &[Vec<(FrameIndex, [f64; 2])>], anchor: FrameIndex, turn: bool) -> BTreeMap<FrameIndex, [f64; 3]> {
    let maps: Vec<HashMap<FrameIndex, [f64; 2]>> = members.iter().map(|m| m.iter().copied().collect()).collect();
    let mut path = BTreeMap::new();
    let (Some(lo), Some(hi)) = (maps.iter().flat_map(|m| m.keys()).min().copied(), maps.iter().flat_map(|m| m.keys()).max().copied()) else { return path };
    let has = |f: FrameIndex| maps.iter().any(|m| m.contains_key(&f));
    let near = anchor.clamp(lo, hi);
    let Some(anchor) = (0..=hi - lo).flat_map(|d| [near - d, near + d]).find(|f| (lo..=hi).contains(f) && has(*f)) else { return path };
    let start: Vec<[f64; 2]> = maps.iter().filter_map(|m| m.get(&anchor).copied()).collect();
    let n = start.len() as f64;
    let first = start.iter().fold([0.0; 2], |s, p| [s[0] + p[0] / n, s[1] + p[1] / n]);
    // A step from frame a to b: the members good on both, their centres there and their turn.
    let step = |a: FrameIndex, b: FrameIndex| -> Option<([f64; 2], [f64; 2], f64)> {
        let pairs: Vec<([f64; 2], [f64; 2])> = maps.iter().filter_map(|m| Some((*m.get(&a)?, *m.get(&b)?))).collect();
        if pairs.is_empty() {
            return None;
        }
        let k = pairs.len() as f64;
        let (mut ca, mut cb) = ([0.0; 2], [0.0; 2]);
        for (p, q) in &pairs {
            ca = [ca[0] + p[0] / k, ca[1] + p[1] / k];
            cb = [cb[0] + q[0] / k, cb[1] + q[1] / k];
        }
        let mut turned = 0.0;
        if turn && pairs.len() >= 2 {
            let (mut sin, mut cos) = (0.0, 0.0);
            for (p, q) in &pairs {
                let (u, v) = ([p[0] - ca[0], p[1] - ca[1]], [q[0] - cb[0], q[1] - cb[1]]);
                sin += u[0] * v[1] - u[1] * v[0];
                cos += u[0] * v[0] + u[1] * v[1];
            }
            turned = sin.atan2(cos);
        }
        Some((ca, cb, turned))
    };
    // Carried from a centre `from` to `to`, turning `by`: R(by)(p − from) + to.
    let carry = |s: [f64; 3], from: [f64; 2], to: [f64; 2], by: f64| {
        let (sn, cs) = by.sin_cos();
        let d = [s[0] - from[0], s[1] - from[1]];
        [cs * d[0] - sn * d[1] + to[0], sn * d[0] + cs * d[1] + to[1], s[2] + by]
    };
    let origin = [first[0], first[1], 0.0];
    path.insert(anchor, origin);
    let mut s = origin;
    for f in anchor + 1..=hi {
        if let Some((a, b, by)) = step(f - 1, f) {
            s = carry(s, a, b, by);
        }
        path.insert(f, s);
    }
    s = origin;
    for f in (lo..anchor).rev() {
        if let Some((a, b, by)) = step(f, f + 1) {
            s = carry(s, b, a, -by);
        }
        path.insert(f, s);
    }
    path
}

/// The subject's path on every frame its members cover: `(frame, pushed,
/// placed)`, each `[x, y, angle]`. Pushed: its members' motion ([`push`]),
/// smoothed over `subject.smooth` seconds; placed: that with its own offset
/// keys on top ([`place`]).
pub fn path(members: &[Vec<(FrameIndex, [f64; 2])>], subject: &Subject, fps: f64) -> Vec<(FrameIndex, [f64; 3], [f64; 3])> {
    let pushed = push(members, subject.anchor, subject.turn);
    // (Every frame from the first to the last is in it: one series per channel.)
    let sigma = subject.smooth.max(0.0) as f64 * fps;
    let smooth = |c: usize| crate::sketch::gauss_trend(&pushed.values().map(|v| v[c]).collect::<Vec<_>>(), sigma);
    let [x, y, a] = [smooth(0), smooth(1), smooth(2)];
    pushed
        .keys()
        .enumerate()
        .map(|(i, f)| {
            let p = [x[i], y[i], a[i]];
            (*f, p, place(p, offset_at(&subject.offsets, *f, subject.ease)))
        })
        .collect()
}

/// The keys after a drag sets the offset on `key.frame` to `key`, bending
/// the path only within `reach` frames of it (None: as far as the keys on
/// either side): `keys` (as they were when the drag began) with `key` in
/// them and, on each side with no other key within the reach, a key at its
/// edge holding the offset that was there.
pub fn pinned(keys: &[OffsetKey], ease: Ease, key: OffsetKey, reach: Option<FrameIndex>) -> Vec<OffsetKey> {
    let mut out = keys.to_vec();
    let mut put = |k: OffsetKey| {
        let i = out.partition_point(|o| o.frame < k.frame);
        match out.get_mut(i).filter(|o| o.frame == k.frame) {
            Some(o) => *o = k,
            None => out.insert(i, k),
        }
    };
    let f = key.frame;
    for edge in reach.map(|r| [f - r.max(1), f + r.max(1)]).into_iter().flatten() {
        if !keys.iter().any(|k| k.frame != f && (edge.min(f)..=edge.max(f)).contains(&k.frame)) {
            let [x, y, a] = offset_at(keys, edge, ease);
            put(OffsetKey { frame: edge, x: x as f32, y: y as f32, angle: a.to_degrees() as f32 });
        }
    }
    put(key);
    out
}

/// The offset on frame `f`: `[x, y, angle (radians)]`, from key to key as
/// `ease` says, held beyond them, 0 without any. `keys` are in frame order.
pub fn offset_at(keys: &[OffsetKey], f: FrameIndex, ease: Ease) -> [f64; 3] {
    let v = |k: &OffsetKey| [k.x as f64, k.y as f64, (k.angle as f64).to_radians()];
    let (Some(first), Some(last)) = (keys.first(), keys.last()) else { return [0.0; 3] };
    if f <= first.frame {
        return v(first);
    }
    if f >= last.frame {
        return v(last);
    }
    let i = keys.partition_point(|k| k.frame <= f);
    let (a, b) = (&keys[i - 1], &keys[i]);
    let u = (f - a.frame) as f64 / (b.frame - a.frame) as f64;
    let u = if ease == Ease::Smooth { u * u * (3.0 - 2.0 * u) } else { u };
    let (va, vb) = (v(a), v(b));
    std::array::from_fn(|c| va[c] + (vb[c] - va[c]) * u)
}

/// Where the subject is with `offset` on its pushed state `pushed` (`[x, y, angle]`).
pub fn place(pushed: [f64; 3], offset: [f64; 3]) -> [f64; 3] {
    let (s, c) = pushed[2].sin_cos();
    [pushed[0] + c * offset[0] - s * offset[1], pushed[1] + s * offset[0] + c * offset[1], pushed[2] + offset[2]]
}

/// The offset position that puts the subject at `at` on its pushed state `pushed`.
pub fn offset_to(pushed: [f64; 3], at: [f64; 2]) -> [f64; 2] {
    let (s, c) = (-pushed[2]).sin_cos();
    let d = [at[0] - pushed[0], at[1] - pushed[1]];
    [c * d[0] - s * d[1], s * d[0] + c * d[1]]
}

pub struct SubjectKind;

impl OperatorKind for SubjectKind {
    fn name(&self) -> &'static str {
        "subject"
    }

    fn channels(&self) -> usize {
        SUBJECT_CHANNELS
    }

    fn footprint(&self, _: EntityRef<'_>) -> Footprint {
        Footprint::Global
    }

    fn evaluate(&self, ctx: &EvalCtx<'_>, range: std::ops::Range<FrameIndex>, out: &mut Signal) -> anyhow::Result<()> {
        out.clear(range.clone());
        let Some(subject) = ctx.params::<Subject>() else { return Ok(()) };
        let members: Vec<_> = members_of(ctx.world, ctx.entity).into_iter().map(|m| points_of(ctx.world, m)).collect();
        let [hw, hh] = subject.half.map(|v| v.max(1.0) as f64);
        let fps = ctx.world.get_resource::<Transport>().map_or(60.0, |t| t.fps.as_f64());
        for (f, pushed, [x, y, a]) in path(&members, subject, fps).into_iter().filter(|(f, ..)| range.contains(f)) {
            out.set(f, &[x, y, x - hw, y - hh, x + hw, y + hh, a, 0.0, pushed[0], pushed[1], pushed[2]].map(|v| v as f32));
        }
        Ok(())
    }
}

/// A subject's value on `frame`, if it has one there.
pub fn value_at(world: &World, subject: Entity, frame: FrameIndex) -> Option<[f64; SUBJECT_CHANNELS]> {
    let v = world.resource::<SignalStore>().get(world.get::<Output>(subject)?.0)?.get(frame)?;
    (v.len() >= SUBJECT_CHANNELS).then(|| std::array::from_fn(|c| v[c] as f64))
}

/// The subject whose point on `frame` is nearest `pos` (source px), within `radius`.
pub fn pick_subject(world: &mut World, frame: FrameIndex, pos: [f64; 2], radius: f64) -> Option<Entity> {
    let mut q = world.query_filtered::<(Entity, &Operator), Without<Disabled>>();
    let subjects: Vec<Entity> = q.iter(world).filter(|(_, o)| o.kind == "subject").map(|(e, _)| e).collect();
    subjects
        .into_iter()
        .filter_map(|e| {
            let v = value_at(world, e, frame)?;
            let d = (v[0] - pos[0]).hypot(v[1] - pos[1]);
            (d <= radius).then_some((e, d))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(e, _)| e)
}

/// Make a subject of `members`, starting from `anchor`. One undo step; the subject is selected.
pub fn make_subject(world: &mut World, members: &[Entity], anchor: FrameIndex) -> Option<Entity> {
    let members: Vec<Entity> = members.iter().copied().filter(|e| can_be_member(world, *e)).collect();
    if members.is_empty() {
        return None;
    }
    let count = {
        let mut q = world.query::<&Operator>();
        q.iter(world).filter(|o| o.kind == "subject").count()
    };
    let name = format!("Subject {}", count + 1);
    // Its box: the size of the members' boxes on the anchor frame (a point's: the default).
    let store = world.resource::<SignalStore>();
    let half = members
        .iter()
        .filter_map(|m| store.get(world.get::<Output>(*m)?.0)?.get(anchor).filter(|v| v.len() >= 6).map(|v| [(v[4] - v[2]) / 2.0, (v[5] - v[3]) / 2.0]))
        .fold(None, |acc: Option<[f32; 2]>, h| Some(acc.map_or(h, |a| [a[0].max(h[0]), a[1].max(h[1])])))
        .filter(|h| h[0] >= 4.0 && h[1] >= 4.0)
        .unwrap_or(Subject::default().half);
    let mut made = None;
    edit(world, &format!("Make {name}"), |tx| {
        let out = tx.create_signal(SUBJECT_CHANNELS);
        let inputs = members.iter().map(|m| (MEMBER.to_string(), *m)).collect();
        made = Some(tx.spawn((Name::new(name.clone()), Operator { kind: "subject".into() }, Inputs(inputs), Output(out), Subject { anchor, half, ..Subject::default() })));
    });
    let s = made?;
    world.resource_mut::<Selection>().select_only(s);
    Some(s)
}

/// Whether `e` can join `subject`: it can be a member, isn't one yet, and
/// doesn't read the subject itself (a tracker in a view following it, a
/// subject it is in): that would be a cycle.
pub fn can_join(world: &World, subject: Entity, e: Entity) -> bool {
    can_be_member(world, e) && !members_of(world, subject).contains(&e) && !crate::op::reads(world, e, subject)
}

/// Add `members` to `subject` (one undo step); returns how many were new.
pub fn add_members(world: &mut World, subject: Entity, members: &[Entity]) -> usize {
    let mut new: Vec<Entity> = Vec::new();
    for e in members.iter().copied().filter(|e| can_join(world, subject, *e)) {
        if !new.contains(&e) {
            new.push(e);
        }
    }
    if new.is_empty() {
        return 0;
    }
    let name = world.get::<Name>(subject).map_or("subject".to_string(), |n| n.to_string());
    edit(world, &format!("Add to {name}"), |tx| tx.modify::<Inputs>(subject, |i| i.0.extend(new.iter().map(|m| (MEMBER.to_string(), *m)))));
    new.len()
}

/// Take `members` out of `subject` (one undo step); returns how many were in it.
pub fn remove_members(world: &mut World, subject: Entity, members: &[Entity]) -> usize {
    let gone = world.get::<Inputs>(subject).map_or(0, |i| i.0.iter().filter(|(s, e)| s == MEMBER && members.contains(e)).count());
    if gone > 0 {
        let name = world.get::<Name>(subject).map_or("subject".to_string(), |n| n.to_string());
        edit(world, &format!("Remove from {name}"), |tx| tx.modify::<Inputs>(subject, |i| i.0.retain(|(s, e)| !(s == MEMBER && members.contains(e)))));
    }
    gone
}

/// Key the subject's own transform on `frame` (replacing a key there). One
/// undo step, or part of the open gesture.
pub fn set_offset_key(world: &mut World, subject: Entity, key: OffsetKey) {
    let name = world.get::<Name>(subject).map_or("subject".to_string(), |n| n.to_string());
    edit(world, &format!("Key {name}"), |tx| {
        tx.modify::<Subject>(subject, |s| {
            let i = s.offsets.partition_point(|k| k.frame < key.frame);
            match s.offsets.get_mut(i).filter(|k| k.frame == key.frame) {
                Some(k) => *k = key,
                None => s.offsets.insert(i, key),
            }
        })
    });
}

/// Remove the subject's key on `frame`, if any (one undo step).
pub fn remove_offset_key(world: &mut World, subject: Entity, frame: FrameIndex) {
    if world.get::<Subject>(subject).is_some_and(|s| s.offsets.iter().any(|k| k.frame == frame)) {
        let name = world.get::<Name>(subject).map_or("subject".to_string(), |n| n.to_string());
        edit(world, &format!("Unkey {name}"), |tx| tx.modify::<Subject>(subject, |s| s.offsets.retain(|k| k.frame != frame)));
    }
}

/// Frames either side of the playhead the selected subject's path is drawn
/// over, and can be taken hold of.
pub const PATH_FRAMES: FrameIndex = 90;

/// How far a drag on a subject's path bends it, in seconds of video, from
/// least to most; past the last, the whole path (as far as its other keys).
pub const REACHES: [f32; 9] = [0.1, 0.2, 0.35, 0.5, 0.75, 1.0, 1.5, 2.0, 3.0];

/// The reach a drag on a subject's path has: wheel notches into
/// [`REACHES`] (the wheel while dragging turns it), remembered for the next drag.
#[derive(Resource, Debug, Clone, Copy)]
pub struct SubjectReach(pub f32);

impl Default for SubjectReach {
    fn default() -> Self {
        Self(3.0)
    }
}

impl SubjectReach {
    /// In seconds; None: the whole path.
    pub fn seconds(self) -> Option<f32> {
        REACHES.get(self.0.round().max(0.0) as usize).copied()
    }
}

/// The frame of `subject`'s drawn path whose point is nearest `at` (the
/// shown space's pixels, `view`'s), within `radius` of them: each frame's
/// point through the view's framing on that frame, as the path is drawn,
/// over [`PATH_FRAMES`] either side of `frame`. Ties go to the frame nearer `frame`.
pub fn path_frame(world: &World, subject: Entity, view: Option<Entity>, frame: FrameIndex, at: [f64; 2], radius: f64) -> Option<FrameIndex> {
    (frame - PATH_FRAMES..=frame + PATH_FRAMES)
        .filter_map(|f| {
            let v = value_at(world, subject, f)?;
            let p = map_at(world, view, f).from_source([v[0], v[1]]);
            let d = (p[0] - at[0]).hypot(p[1] - at[1]);
            (d <= radius).then_some((f, d))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1).then((a.0 - frame).abs().cmp(&(b.0 - frame).abs())))
        .map(|(f, _)| f)
}

/// The Select tool's drag of a subject, from the press on.
#[derive(Clone, Debug)]
struct Grab {
    subject: Entity,
    /// The frame taken hold of, and the shown space's framing on it (the path is drawn through it).
    frame: FrameIndex,
    map: crate::view::SpaceMap,
    /// The press (shown px), and from it to the subject's point on that frame (source px).
    from: [f64; 2],
    hold: [f64; 2],
    /// Its members' motion on that frame and its keys, as they were at the press.
    pushed: [f64; 3],
    keys: Vec<OffsetKey>,
    moved: bool,
}

#[derive(Resource, Debug, Default)]
pub struct SubjectDrag(Option<Grab>);

impl SubjectDrag {
    /// The subject being dragged and the frame of it taken hold of.
    pub fn grabbed(&self) -> Option<(Entity, FrameIndex)> {
        self.0.as_ref().filter(|g| g.moved).map(|g| (g.subject, g.frame))
    }
}

/// `Set::Tools`: in the Select tool, a press on a subject's point, or on
/// the selected subject's path, and a drag bends its path there: a key on
/// that frame where the pointer is, and keys at the reach's edges holding
/// what was there ([`pinned`]); the wheel changes the reach as it goes. One
/// undo step per drag. A press that doesn't move is a click (tool.rs).
fn drag_subject(world: &mut World) {
    if world.resource::<ActiveTool>().0 != Tool::Select {
        if world.resource_mut::<SubjectDrag>().0.take().is_some_and(|g| g.moved) {
            world.resource_mut::<History>().end();
        }
        return;
    }
    let p = world.resource::<PointerFrame>().clone();
    let frame = world.resource::<Transport>().frame();
    let fps = world.resource::<Transport>().fps.as_f64();
    let view = world.resource::<ActiveView>().0;
    let scale = if p.scale > 0.0 { p.scale } else { 1.0 };
    if let Some(t) = p.pressed
        && let Some(at) = p.samples.iter().find(|s| s[0] >= t).map(|s| [s[1], s[2]]).or(p.hover)
    {
        let map = map_at(world, view, frame);
        let src = map.to_source(at);
        // The subject whose point is under the press, else the selected one's path there.
        // (A layer's picture over the point takes the press, unless this subject is selected.)
        let grabbed = match crate::layer::press_on_layer(world, frame, src, map.a / scale) {
            Some(_) => None,
            None => pick_subject(world, frame, src, 12.0 / scale * map.a).map(|e| (e, frame)).or_else(|| {
                let s = world.resource::<Selection>().primary().filter(|e| is_subject(world, *e))?;
                Some((s, path_frame(world, s, view, frame, at, 12.0 / scale)?))
            }),
        };
        if let Some((e, f)) = grabbed
            && let Some(v) = value_at(world, e, f)
        {
            let map = map_at(world, view, f);
            let src = map.to_source(at);
            let keys = world.get::<Subject>(e).map(|s| s.offsets.clone()).unwrap_or_default();
            world.resource_mut::<SubjectDrag>().0 = Some(Grab { subject: e, frame: f, map, from: at, hold: [v[0] - src[0], v[1] - src[1]], pushed: [v[8], v[9], v[10]], keys, moved: false });
        }
    }
    let Some(g) = world.resource::<SubjectDrag>().0.clone() else { return };
    // While it's held, the views hold still (view::HoldViews): one following the
    // subject would otherwise move under the pointer, and the drag run away.
    let start = g.map.to_source(g.from);
    crate::view::HoldViews::ask(world, Some([start[0] + g.hold[0], start[1] + g.hold[1]]));
    if p.wheel != 0.0 {
        let mut reach = world.resource_mut::<SubjectReach>();
        reach.0 = (reach.0 + p.wheel).clamp(0.0, REACHES.len() as f32);
        world.resource_mut::<PointerFrame>().wheel_taken = true;
    }
    if let Some(now) = p.samples.last().map(|s| [s[1], s[2]]).or(p.hover)
        && (g.moved || (now[0] - g.from[0]).hypot(now[1] - g.from[1]) * scale >= CLICK_MOVE)
    {
        let e = g.subject;
        if !g.moved {
            let name = world.get::<Name>(e).map_or("subject".to_string(), |n| n.to_string());
            world.resource_mut::<History>().begin(format!("Move {name}"));
            world.resource_mut::<Selection>().select_only(e);
            if let Some(d) = world.resource_mut::<SubjectDrag>().0.as_mut() {
                d.moved = true;
            }
        }
        let src = g.map.to_source(now);
        let [x, y] = offset_to(g.pushed, [src[0] + g.hold[0], src[1] + g.hold[1]]);
        let ease = world.get::<Subject>(e).map_or(Ease::Smooth, |s| s.ease);
        let angle = offset_at(&g.keys, g.frame, ease)[2].to_degrees() as f32;
        let reach = world.resource::<SubjectReach>().seconds().map(|s| (s as f64 * fps).round() as FrameIndex);
        let keys = pinned(&g.keys, ease, OffsetKey { frame: g.frame, x: x as f32, y: y as f32, angle }, reach);
        if world.get::<Subject>(e).is_some_and(|s| s.offsets != keys) {
            edit(world, "Move subject", |tx| tx.modify::<Subject>(e, |s| s.offsets = keys));
        }
    }
    if (p.released.is_some() || !p.down) && world.resource_mut::<SubjectDrag>().0.take().is_some_and(|g| g.moved) {
        world.resource_mut::<History>().end();
    }
}

pub struct SubjectModule;

impl Module for SubjectModule {
    fn build(&self, app: &mut AppBuilder) {
        app.operator(SubjectKind)
            .operator_params::<Subject>()
            .register_type::<OffsetKey>()
            .register_type::<Ease>()
            .declare::<SubjectDrag>(Class::Derived)
            .init_resource::<SubjectDrag>()
            .declare::<SubjectReach>(Class::Session)
            .init_resource::<SubjectReach>()
            .add_systems(drag_subject.in_set(Set::Tools));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rigid thing at `centre` turned `turn` (radians, y down), its points at `local`.
    fn on(centre: [f64; 2], turn: f64, local: [f64; 2]) -> [f64; 2] {
        let (s, c) = turn.sin_cos();
        [centre[0] + c * local[0] - s * local[1], centre[1] + s * local[0] + c * local[1]]
    }

    #[test]
    fn members_coming_and_going_never_make_it_jump() {
        // Three trackers on something moving right; they disagree about where it is
        // (one sits 30 px off), and come and go: an average of positions would jump.
        let at = |f: i64| [100.0 + 2.0 * f as f64, 50.0];
        let a: Vec<_> = (0..100).map(|f| (f, at(f))).collect();
        let b: Vec<_> = (20..70).map(|f| (f, [at(f)[0] + 30.0, at(f)[1]])).collect();
        let c: Vec<_> = (0..100).filter(|f| !(40..60).contains(f)).map(|f| (f, [at(f)[0], at(f)[1] - 10.0])).collect();
        let path = push(&[a, b, c], 0, true);
        assert_eq!(path.len(), 100);
        let x0 = path[&0][0];
        for f in 0..100 {
            let p = path[&f];
            assert!((p[0] - (x0 + 2.0 * f as f64)).abs() < 1e-9 && p[2].abs() < 1e-12, "frame {f}: {p:?}");
        }
        // It started at their mean on the anchor frame (the two there).
        assert!((x0 - 100.0).abs() < 1e-9 && (path[&0][1] - 45.0).abs() < 1e-9);
    }

    #[test]
    fn it_turns_with_its_members_and_its_offset_turns_too() {
        // Two points of a thing turning 1° a frame about (300, 200), drifting down.
        let centre = |f: i64| [300.0, 200.0 + f as f64];
        let turn = |f: i64| (f as f64).to_radians();
        let members: Vec<Vec<(i64, [f64; 2])>> = [[-40.0, 0.0], [40.0, 0.0]].iter().map(|l| (-20..40).map(|f| (f, on(centre(f), turn(f), *l))).collect()).collect();
        let path = push(&members, 0, true);
        // An offset of 50 px along the thing's own x axis, keyed on the anchor frame.
        let keys = [OffsetKey { frame: 0, x: 50.0, y: 0.0, angle: 10.0 }];
        for f in -20..40 {
            let p = place(path[&f], offset_at(&keys, f, Ease::Linear));
            let want = on(centre(f), turn(f), [50.0, 0.0]);
            assert!((p[0] - want[0]).hypot(p[1] - want[1]) < 1e-6, "frame {f}: {p:?} vs {want:?}");
            assert!((p[2] - turn(f) - 10f64.to_radians()).abs() < 1e-9, "frame {f}: angle {}", p[2]);
        }
        // Without `turn` it only shifts.
        assert!(push(&members, 0, false).values().all(|p| p[2] == 0.0));
    }

    #[test]
    fn offset_keys_are_linear_between_and_held_beyond() {
        let keys = [OffsetKey { frame: 10, x: 0.0, y: 0.0, angle: 0.0 }, OffsetKey { frame: 20, x: 10.0, y: -4.0, angle: 30.0 }];
        assert_eq!(offset_at(&keys, 0, Ease::Linear), [0.0, 0.0, 0.0]);
        let mid = offset_at(&keys, 15, Ease::Linear);
        assert!((mid[0] - 5.0).abs() < 1e-9 && (mid[1] + 2.0).abs() < 1e-9 && (mid[2] - 15f64.to_radians()).abs() < 1e-9);
        assert_eq!(offset_at(&keys, 99, Ease::Linear)[0], 10.0);
        assert_eq!(offset_at(&[], 5, Ease::Linear), [0.0; 3]);
        // `offset_to` is `place`'s inverse.
        let pushed = [100.0, 50.0, 0.3];
        let o = offset_to(pushed, [130.0, 70.0]);
        let back = place(pushed, [o[0], o[1], 0.0]);
        assert!((back[0] - 130.0).abs() < 1e-9 && (back[1] - 70.0).abs() < 1e-9);
    }

    #[test]
    fn a_frame_nobody_moves_holds_it_and_the_anchor_finds_a_point() {
        let a: Vec<_> = (0..10).chain(20..30).map(|f| (f, [f as f64, 0.0])).collect();
        let path = push(&[a], 15, true);
        // The anchor (15) has no point: the nearest frame with one is 20 (9 is a frame farther).
        assert_eq!(path.len(), 30);
        // Frames 10–19 hold: no step there has a member on both frames.
        assert_eq!(path[&12], path[&9]);
        assert!((path[&25][0] - path[&20][0] - 5.0).abs() < 1e-9);
    }
}

#[cfg(test)]
mod path_tests {
    use super::*;

    fn key(frame: FrameIndex, x: f32) -> OffsetKey {
        OffsetKey { frame, x, y: 0.0, angle: 0.0 }
    }

    #[test]
    fn a_pin_holds_what_was_there_at_its_reach_unless_a_key_is_nearer() {
        let frames = |k: &[OffsetKey]| k.iter().map(|k| (k.frame, k.x)).collect::<Vec<_>>();
        // No keys: guards on both sides, holding 0.
        assert_eq!(frames(&pinned(&[], Ease::Smooth, key(50, 9.0), Some(10))), vec![(40, 0.0), (50, 9.0), (60, 0.0)]);
        // A key within the reach on the left bounds it there; on the right a guard holds the key's value (held beyond it).
        assert_eq!(frames(&pinned(&[key(45, 4.0)], Ease::Smooth, key(50, 9.0), Some(10))), vec![(45, 4.0), (50, 9.0), (60, 4.0)]);
        // A key on the frame itself is replaced; no reach: no guards.
        assert_eq!(frames(&pinned(&[key(50, 1.0), key(90, 2.0)], Ease::Linear, key(50, 9.0), None)), vec![(50, 9.0), (90, 2.0)]);
        // Between two keys (linear), the guard takes the line's value there, so outside the reach nothing changes.
        let keys = [key(0, 0.0), key(100, 100.0)];
        let after = pinned(&keys, Ease::Linear, key(50, 80.0), Some(10));
        for f in (0..40).chain(61..100) {
            assert!((offset_at(&after, f, Ease::Linear)[0] - offset_at(&keys, f, Ease::Linear)[0]).abs() < 1e-4, "frame {f}");
        }
    }

    #[test]
    fn smooth_keys_have_no_corner_and_never_overshoot() {
        let keys = [key(0, 0.0), key(10, 10.0), key(20, 0.0)];
        let x = |f: FrameIndex| offset_at(&keys, f, Ease::Smooth)[0];
        assert!((0..=20).all(|f| (0.0..=10.0).contains(&x(f))));
        // Flat into and out of each key: the steps beside a key are small, the middle ones big.
        assert!(x(1) - x(0) < 0.3 && x(10) - x(9) < 0.3 && x(6) - x(5) > 1.4);
        assert_eq!((x(10), x(30), x(-5)), (10.0, 0.0, 0.0));
    }

    #[test]
    fn smoothing_the_path_smooths_the_members_motion_and_keeps_keys_exact() {
        // A point moving 2 px a frame with ±3 px of jitter.
        let jitter = |f: i64| if f % 2 == 0 { 3.0 } else { -3.0 };
        let members = vec![(0..300).map(|f| (f, [100.0 + 2.0 * f as f64, 50.0 + jitter(f)])).collect::<Vec<_>>()];
        let rough = Subject { anchor: 0, ..Subject::default() };
        let same: Vec<_> = path(&members, &rough, 60.0);
        let pushed = push(&members, 0, true);
        assert!(same.iter().all(|(f, p, q)| *p == pushed[f] && q == p), "smooth 0: the members' motion as it is");
        let smooth = Subject { smooth: 0.25, offsets: vec![OffsetKey { frame: 150, x: 0.0, y: 10.0, angle: 0.0 }], ..rough };
        let out = path(&members, &smooth, 60.0);
        let shake = |p: &[(FrameIndex, [f64; 3], [f64; 3])]| p.windows(3).map(|w| (w[0].1[1] - 2.0 * w[1].1[1] + w[2].1[1]).abs()).fold(0.0, f64::max);
        assert!(shake(&out) < 0.05 * shake(&same), "{} vs {}", shake(&out), shake(&same));
        // The ends aren't pulled (the motion is a line there), and the key's offset sits exactly on the smoothed motion.
        assert!((out[0].1[0] - 100.0).abs() < 0.5 && (out[299].1[0] - 698.0).abs() < 0.5, "{:?} {:?}", out[0].1, out[299].1);
        assert_eq!(out[150].2[1], out[150].1[1] + 10.0);
    }
}
