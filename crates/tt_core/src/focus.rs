//! SpringFocus (on request: "if at the start of the clip i already zoomed
//! in and centered onto something but then i later want to do a tracked
//! mouse shot … still being able to move the camera smoothly over to it";
//! "make something called a SpringFocus … set whatever its tracking at any
//! time, spring settings in inspector"): one point that follows a sequence
//! of things, moving smoothly from one to the next.
//!
//! A SpringFocus is an operator (`focus`) with keys ([`FocusKey`]): from a
//! frame on, it focuses on a target (a sketch, a tracker, a subject: anything
//! with a box; its inputs `target` are those). Its output is a box,
//! `[x, y, left, top, right, bottom, angle, flags]` (a tracker's layout, so
//! a view follows it with Tab, a layer attaches to it, an export renders it,
//! a subject can take it as a member).
//!
//! **Moving over.** At a key it doesn't chase the new target (a spring on
//! the position would trail behind a moving mouse); it *blends* from where it
//! was going to where the new target is, by a spring's step response over
//! the move time: `pos(f) = lerp(before(f), target(f), w(f − start))`, with
//! `before` what it showed without this key (itself blending from the keys
//! before it). Both ends keep moving with what they follow during the move,
//! and once the spring has settled it sits exactly on the new target: no
//! lag. A key during a move starts from wherever that move was.
//!
//! **The spring** ([`FocusParams`]): the move time (seconds to settle), the
//! bounce (0: none, critically damped; up to 0.9: it overshoots and settles
//! back), and the lead (seconds it starts before the key, so it arrives
//! sooner). The box's size moves the same way (a view that zooms with what
//! it follows zooms over too). Frames a target doesn't have (outside its
//! frames, or a tracker's lost ones) hold or bridge its nearest.

use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::name::Name;
use bevy_ecs::prelude::*;
use bevy_reflect::Reflect;

use crate::app::{AppBuilder, Module};
use crate::history::edit;
use crate::op::{EvalCtx, Footprint, Inputs, Operator, OperatorKind, Output};
use crate::selection::Selection;
use crate::signal::{Signal, SignalStore};
use crate::time::FrameIndex;
use crate::transport::Transport;

/// `[x, y, left, top, right, bottom, angle, flags]` (a tracker's layout).
pub const FOCUS_CHANNELS: usize = 8;
/// The input slot of what it can focus on.
pub const TARGET: &str = "target";

/// From `frame` on, focus on `target`.
#[derive(Reflect, Clone, Copy, Debug, PartialEq)]
pub struct FocusKey {
    pub frame: FrameIndex,
    pub target: Entity,
}

/// A SpringFocus's keys and spring (its operator's params).
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component)]
pub struct FocusParams {
    /// In frame order.
    pub keys: Vec<FocusKey>,
    /// Seconds a move takes to settle.
    pub move_time: f32,
    /// 0: no overshoot; up to 0.9: it goes past and settles back.
    pub bounce: f32,
    /// Seconds a move starts before its key.
    pub lead: f32,
}

impl Default for FocusParams {
    fn default() -> Self {
        Self { keys: Vec::new(), move_time: 1.0, bounce: 0.0, lead: 0.0 }
    }
}

impl FocusParams {
    /// The key on frame `f`, if any.
    pub fn key_at(&self, f: FrameIndex) -> Option<&FocusKey> {
        self.keys.iter().find(|k| k.frame == f)
    }

    /// Focus on `target` from frame `f` (replacing a key there).
    pub fn set(&mut self, f: FrameIndex, target: Entity) {
        let i = self.keys.partition_point(|k| k.frame < f);
        match self.keys.get_mut(i).filter(|k| k.frame == f) {
            Some(k) => k.target = target,
            None => self.keys.insert(i, FocusKey { frame: f, target }),
        }
    }

    /// What it focuses on at frame `f` (the key at or before it; before the first, the first's).
    pub fn target_at(&self, f: FrameIndex) -> Option<Entity> {
        let i = self.keys.partition_point(|k| k.frame <= f);
        self.keys.get(i.saturating_sub(1)).map(|k| k.target)
    }
}

/// How far a move has gone `t` seconds after it starts: a spring's step
/// response (damping ratio 1 − `bounce`: overshoot when above 0), tuned to
/// be within 0.2 % at `time`, and made to land exactly there (the last bit
/// added smoothly over the move), so from then on it sits on the target.
pub fn spring(t: f64, time: f64, bounce: f64) -> f64 {
    if t <= 0.0 {
        return 0.0;
    }
    if time <= 0.0 || t >= time {
        return 1.0;
    }
    let z = (1.0 - bounce.clamp(0.0, 0.9)).max(0.1);
    // Settling to 0.2 %: about 8.4 / ω critically damped, 6.2 / (ζ ω) below that.
    let w = if z >= 1.0 { 8.4 / time } else { 6.2 / (z * time) };
    let raw = |t: f64| {
        if z >= 1.0 {
            1.0 - (1.0 + w * t) * (-w * t).exp()
        } else {
            let wd = w * (1.0 - z * z).sqrt();
            1.0 - (-z * w * t).exp() * ((wd * t).cos() + z * w / wd * (wd * t).sin())
        }
    };
    let u = t / time;
    raw(t) + (1.0 - raw(time)) * u * u * (3.0 - 2.0 * u)
}

/// `[x, y, half width, half height, angle]` of each target on every frame
/// from `lo` to `hi`: its own (lost or missing frames bridged, its ends held).
fn track(sig: &Signal, angled: bool, lo: FrameIndex, hi: FrameIndex) -> Option<Vec<[f64; 5]>> {
    let mut v: Vec<Option<[f64; 5]>> = (lo..=hi)
        .map(|f| {
            sig.get(f).filter(|v| v.len() >= 6 && (angled || v.get(7).is_none_or(|fl| *fl == 0.0))).map(|v| {
                let c = |i: usize| v[i] as f64;
                [c(0), c(1), (c(4) - c(2)).abs() / 2.0, (c(5) - c(3)).abs() / 2.0, if angled { c(6) } else { 0.0 }]
            })
        })
        .collect();
    let known: Vec<usize> = (0..v.len()).filter(|i| v[*i].is_some()).collect();
    let (&a, &b) = (known.first()?, known.last()?);
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
    Some(v.into_iter().map(|x| x.expect("filled")).collect())
}

/// Its box on every frame from `lo` to `hi` (module docs): `targets(k)` is
/// key k's target's `[x, y, half w, half h, angle]` over those frames.
pub fn focus_path(p: &FocusParams, targets: &[Option<Vec<[f64; 5]>>], lo: FrameIndex, hi: FrameIndex, fps: f64) -> Vec<Option<[f64; 5]>> {
    let n = (hi - lo + 1).max(0) as usize;
    let mut out: Vec<Option<[f64; 5]>> = vec![None; n];
    let lead = (p.lead.max(0.0) as f64 * fps).round() as FrameIndex;
    for (k, key) in p.keys.iter().enumerate() {
        let Some(t) = targets.get(k).and_then(Option::as_ref) else { continue };
        // The first key (or one with nothing before it): its target from the start.
        let start = key.frame - lead;
        for (i, slot) in out.iter_mut().enumerate() {
            let f = lo + i as FrameIndex;
            let w = if slot.is_none() { 1.0 } else { spring((f - start) as f64 / fps, p.move_time as f64, p.bounce as f64) };
            let to = t[i];
            *slot = Some(match *slot {
                Some(from) if w < 1.0 => std::array::from_fn(|c| from[c] + (to[c] - from[c]) * w),
                _ => to,
            });
        }
    }
    out
}

/// `focus`: a SpringFocus's box on every frame its targets cover (module docs).
pub struct FocusKind;

impl OperatorKind for FocusKind {
    fn name(&self) -> &'static str {
        "focus"
    }

    fn channels(&self) -> usize {
        FOCUS_CHANNELS
    }

    fn footprint(&self, _: EntityRef<'_>) -> Footprint {
        Footprint::Global
    }

    fn evaluate(&self, ctx: &EvalCtx<'_>, range: std::ops::Range<FrameIndex>, out: &mut Signal) -> anyhow::Result<()> {
        out.clear(range.clone());
        let p = ctx.params::<FocusParams>().cloned().unwrap_or_default();
        let fps = ctx.world.get_resource::<Transport>().map_or(60.0, |t| t.fps.as_f64());
        let store = ctx.world.resource::<SignalStore>();
        let signal = |e: Entity| ctx.world.get::<Output>(e).and_then(|o| store.get(o.0)).filter(|_| ctx.world.get::<Disabled>(e).is_none());
        // The frames any of its targets covers.
        let hulls: Vec<(FrameIndex, FrameIndex)> = p.keys.iter().filter_map(|k| signal(k.target)?.present_hull()).collect();
        let (Some(lo), Some(hi)) = (hulls.iter().map(|h| h.0).min(), hulls.iter().map(|h| h.1).max()) else { return Ok(()) };
        let targets: Vec<Option<Vec<[f64; 5]>>> = p.keys.iter().map(|k| track(signal(k.target)?, ctx.world.get::<Operator>(k.target).is_some_and(|o| o.kind == "subject"), lo, hi)).collect();
        for (i, v) in focus_path(&p, &targets, lo, hi, fps).into_iter().enumerate() {
            let f = lo + i as FrameIndex;
            if range.contains(&f)
                && let Some([x, y, hw, hh, a]) = v
            {
                out.set(f, &[x, y, x - hw, y - hh, x + hw, y + hh, a, 0.0].map(|c| c as f32));
            }
        }
        Ok(())
    }
}

/// Whether `e` is a SpringFocus.
pub fn is_focus(world: &World, e: Entity) -> bool {
    world.get::<Operator>(e).is_some_and(|o| o.kind == "focus")
}

/// Its inputs: each target its keys name, once.
fn inputs_for(p: &FocusParams) -> Vec<(String, Entity)> {
    let mut v: Vec<(String, Entity)> = Vec::new();
    for k in &p.keys {
        if !v.iter().any(|(_, e)| *e == k.target) {
            v.push((TARGET.to_string(), k.target));
        }
    }
    v
}

/// A new SpringFocus focusing on `target` from frame `frame` (one undo step; it is selected).
pub fn make_focus(world: &mut World, target: Entity, frame: FrameIndex) -> Entity {
    let n = {
        let mut q = world.query::<&Operator>();
        q.iter(world).filter(|o| o.kind == "focus").count() + 1
    };
    let mut p = FocusParams::default();
    p.set(frame, target);
    let name = format!("SpringFocus {n}");
    let mut made = None;
    edit(world, &format!("Make {name}"), |tx| {
        let out = tx.create_signal(FOCUS_CHANNELS);
        made = Some(tx.spawn((Name::new(name.clone()), Operator { kind: "focus".into() }, Inputs(inputs_for(&p)), Output(out), p)));
    });
    let e = made.expect("made");
    world.resource_mut::<Selection>().select_only(e);
    e
}

/// Change a SpringFocus's keys or spring, as one undo step (or part of an
/// open gesture); its inputs follow its keys.
pub fn set_focus(world: &mut World, e: Entity, label: &str, f: impl FnOnce(&mut FocusParams)) {
    let Some(mut p) = world.get::<FocusParams>(e).cloned() else { return };
    f(&mut p);
    p.keys.sort_by_key(|k| k.frame);
    p.keys.dedup_by_key(|k| k.frame);
    let inputs = inputs_for(&p);
    let changed_inputs = world.get::<Inputs>(e).is_none_or(|i| i.0 != inputs);
    edit(world, label, |tx| {
        tx.modify::<FocusParams>(e, |q| *q = p);
        if changed_inputs {
            tx.modify::<Inputs>(e, |i| i.0 = inputs);
        }
    });
}

/// Focus `e` on `target` from frame `f` (one undo step).
pub fn focus_on(world: &mut World, e: Entity, f: FrameIndex, target: Entity) {
    let name = world.get::<Name>(target).map_or("it".to_string(), |n| n.to_string());
    set_focus(world, e, &format!("Focus on {name}"), |p| p.set(f, target));
}

/// The SpringFocus whose point on `frame` is nearest `pos` (source px), within `radius`.
pub fn pick_focus(world: &mut World, frame: FrameIndex, pos: [f64; 2], radius: f64) -> Option<Entity> {
    let list = focuses(world);
    let store = world.resource::<SignalStore>();
    list.into_iter()
        .filter_map(|e| {
            let v = store.get(world.get::<Output>(e)?.0)?.get(frame)?;
            let d = (v[0] as f64 - pos[0]).hypot(v[1] as f64 - pos[1]);
            (d <= radius).then_some((e, d))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(e, _)| e)
}

/// The live SpringFocuses, in the order they were made.
pub fn focuses(world: &mut World) -> Vec<Entity> {
    let mut q = world.query_filtered::<(Entity, &Operator), Without<Disabled>>();
    let mut v: Vec<Entity> = q.iter(world).filter(|(_, o)| o.kind == "focus").map(|(e, _)| e).collect();
    crate::meta::creation_order(world, &mut v);
    v
}

pub struct FocusModule;

impl Module for FocusModule {
    fn build(&self, app: &mut AppBuilder) {
        app.operator(FocusKind).operator_params::<FocusParams>();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spring_settles_in_its_time_and_bounces_when_asked() {
        assert_eq!(spring(0.0, 1.0, 0.0), 0.0);
        let half = spring(0.5, 1.0, 0.0);
        assert!(half > 0.5 && half < 1.0, "half way through: well on its way, {half}");
        assert_eq!(spring(1.0, 1.0, 0.0), 1.0, "exactly there at its time");
        assert!((spring(0.999, 1.0, 0.0) - 1.0).abs() < 0.003, "and no jump onto it");
        let mut prev = 0.0;
        for i in 0..=100 {
            let v = spring(i as f64 / 100.0, 1.0, 0.0);
            assert!(v >= prev - 1e-12, "no bounce: never goes back");
            prev = v;
        }
        let peak = (0..=150).map(|i| spring(i as f64 / 100.0, 1.0, 0.5)).fold(0.0, f64::max);
        assert!(peak > 1.05, "a bounce goes past: {peak}");
        assert!((spring(0.999, 1.0, 0.5) - 1.0).abs() < 0.01 && spring(1.0, 1.0, 0.5) == 1.0);
        assert_eq!(spring(0.1, 0.0, 0.0), 1.0, "no move time: a cut");
    }

    #[test]
    fn it_moves_over_without_lag_and_starts_a_move_from_where_one_was() {
        // A still at x = 0 and B moving right 1 px a frame from x = 1000; keys: A from 0, B from 100.
        let a: Vec<[f64; 5]> = (0..=400).map(|_| [0.0, 0.0, 10.0, 10.0, 0.0]).collect();
        let b: Vec<[f64; 5]> = (0..=400).map(|f| [1000.0 + f as f64, 0.0, 20.0, 20.0, 0.0]).collect();
        let e = Entity::PLACEHOLDER;
        let mut p = FocusParams { move_time: 1.0, ..FocusParams::default() };
        p.set(0, e);
        p.set(100, e);
        let x = |p: &FocusParams, f: usize| focus_path(p, &[Some(a.clone()), Some(b.clone())], 0, 400, 60.0)[f].unwrap()[0];
        assert_eq!(x(&p, 99), 0.0, "on A before the key");
        assert_eq!(x(&p, 100), 0.0, "the move starts on the key");
        assert!(x(&p, 130) > 0.0 && x(&p, 130) < 1130.0, "half way: between them");
        assert_eq!(x(&p, 160), 1160.0, "exactly on B at its time");
        assert_eq!(x(&p, 300), 1300.0, "then exactly on B, moving with it: no lag");
        // The box's size moves over too.
        assert_eq!(focus_path(&p, &[Some(a.clone()), Some(b.clone())], 0, 400, 60.0)[300].unwrap()[2], 20.0);
        // A key back to A half way through: it starts from where the move was, no jump.
        let mut q = p.clone();
        q.set(130, e);
        let path = focus_path(&q, &[Some(a.clone()), Some(b.clone()), Some(a.clone())], 0, 400, 60.0);
        assert!((path[130].unwrap()[0] - path[129].unwrap()[0]).abs() < 60.0, "no jump at the second key");
        assert_eq!(path[300].unwrap()[0], 0.0, "back on A");
        // A lead starts it sooner.
        let early = FocusParams { lead: 0.5, ..p.clone() };
        assert!(x(&early, 90) > 0.0, "moving before its key");
    }
}
