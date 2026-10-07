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
}

impl Default for Subject {
    fn default() -> Self {
        Self { anchor: 0, turn: true, half: [48.0, 48.0], offsets: Vec::new() }
    }
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

/// The offset on frame `f`: `[x, y, angle (radians)]`, linear between keys,
/// held beyond them, 0 without any. `keys` are in frame order.
pub fn offset_at(keys: &[OffsetKey], f: FrameIndex) -> [f64; 3] {
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
        for (f, pushed) in push(&members, subject.anchor, subject.turn).range(range) {
            let [x, y, a] = place(*pushed, offset_at(&subject.offsets, *f));
            out.set(*f, &[x, y, x - hw, y - hh, x + hw, y + hh, a, 0.0, pushed[0], pushed[1], pushed[2]].map(|v| v as f32));
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

/// Add `members` to `subject` (one undo step); returns how many were new.
pub fn add_members(world: &mut World, subject: Entity, members: &[Entity]) -> usize {
    let have = members_of(world, subject);
    let new: Vec<Entity> = members.iter().copied().filter(|e| *e != subject && !have.contains(e) && can_be_member(world, *e)).collect();
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

/// The Select tool's drag of a subject: `(subject, where the press took hold
/// of it relative to its point (source px), the frame, moved yet)`.
#[derive(Resource, Debug, Default)]
struct SubjectDrag(Option<(Entity, [f64; 2], FrameIndex, bool)>);

/// `Set::Tools`: in the Select tool, a press on a subject's point and a drag
/// moves it: its offset is keyed on the shown frame (one undo step per drag).
/// A press that doesn't move is a click, which selects it (tool.rs).
fn drag_subject(world: &mut World) {
    if world.resource::<ActiveTool>().0 != Tool::Select {
        if world.resource_mut::<SubjectDrag>().0.take().is_some_and(|d| d.3) {
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
        // (A layer's picture over the point takes the press, unless this subject is selected.)
        if crate::layer::press_on_layer(world, frame, src, map.a / scale).is_none()
            && let Some(e) = pick_subject(world, frame, src, grab)
            && let Some(v) = value_at(world, e, frame)
        {
            world.resource_mut::<SubjectDrag>().0 = Some((e, [v[0] - src[0], v[1] - src[1]], frame, false));
        }
    }
    let Some((e, hold, f, moved)) = world.resource::<SubjectDrag>().0 else { return };
    let ended = p.released.is_some() || !p.down;
    if let Some(now) = p.samples.last().map(|s| [s[1], s[2]]).or(p.hover) {
        let src = map.to_source(now);
        let at = [src[0] + hold[0], src[1] + hold[1]];
        let start = value_at(world, e, f);
        let far = start.is_some_and(|v| (v[0] - at[0]).hypot(v[1] - at[1]) * scale / map.a.max(1e-9) >= CLICK_MOVE);
        if (moved || far)
            && let Some(v) = start
        {
            if !moved {
                let name = world.get::<Name>(e).map_or("subject".to_string(), |n| n.to_string());
                world.resource_mut::<History>().begin(format!("Move {name}"));
                world.resource_mut::<Selection>().select_only(e);
                world.resource_mut::<SubjectDrag>().0 = Some((e, hold, f, true));
            }
            let pushed = [v[8], v[9], v[10]];
            let [x, y] = offset_to(pushed, at);
            let angle = world.get::<Subject>(e).map_or(0.0, |s| offset_at(&s.offsets, f)[2].to_degrees() as f32);
            set_offset_key(world, e, OffsetKey { frame: f, x: x as f32, y: y as f32, angle });
        }
    }
    if ended && world.resource_mut::<SubjectDrag>().0.take().is_some_and(|d| d.3) {
        world.resource_mut::<History>().end();
    }
}

pub struct SubjectModule;

impl Module for SubjectModule {
    fn build(&self, app: &mut AppBuilder) {
        app.operator(SubjectKind)
            .operator_params::<Subject>()
            .register_type::<OffsetKey>()
            .declare::<SubjectDrag>(Class::Derived)
            .init_resource::<SubjectDrag>()
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
            let p = place(path[&f], offset_at(&keys, f));
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
        assert_eq!(offset_at(&keys, 0), [0.0, 0.0, 0.0]);
        let mid = offset_at(&keys, 15);
        assert!((mid[0] - 5.0).abs() < 1e-9 && (mid[1] + 2.0).abs() < 1e-9 && (mid[2] - 15f64.to_radians()).abs() < 1e-9);
        assert_eq!(offset_at(&keys, 99)[0], 10.0);
        assert_eq!(offset_at(&[], 5), [0.0; 3]);
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
