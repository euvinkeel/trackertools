//! The human layer (DESIGN §6.4): where a person drew a tracker's point,
//! frame by frame, over what its algorithm found.
//!
//! A tracker keeps two layers:
//! - its **automatic results** ([`AutoOutput`]): what the runner writes, as
//!   the algorithm found them;
//! - its **human layer** ([`HumanLayer`]): `[x, y]` (source px) on each frame
//!   a person drew the point (the Draw tool, [`draw_tool`]).
//!
//! Its `Output`, what everything downstream reads (views, subjects, exports,
//! overlays), is the two composed ([`compose`]): on each frame the human
//! layer's point where there is one, else the automatic result (valid or
//! stale, as it is), else nothing. A drawn frame scores 1 and has no flags;
//! its box is the automatic one's size around the drawn point. Drawing never
//! changes what the algorithm tracks (a look or a reset point does that):
//! erase a drawn frame and the automatic result shows again.
//!
//! A **manual dot** is a tracker with only the human layer ([`Method::Manual`]):
//! no algorithm, no jobs. The Draw tool makes one when nothing is selected.
//!
//! The Draw tool (`M`): hold on the video and every frame shown while holding
//! takes the pointer's position (paused: the shown frame, as long as you
//! hold; Space plays and records across frames). With a tracker selected it
//! draws that tracker's human layer; otherwise (or with Shift) a new manual
//! dot. Alt+hold erases. A hold is one undo step; Esc cancels it.

use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::name::Name;
use bevy_ecs::prelude::*;
use bevy_reflect::Reflect;
use tt_core::history::{History, Tx, edit, undo};
use tt_core::input::{Action, KeysHeld, PendingActions};
use tt_core::op::{Inputs, Invalidations, Operator, Output};
use tt_core::ranges::RangeSet;
use tt_core::selection::Selection;
use tt_core::signal::{FrameState, Signal, SignalId, SignalStore};
use tt_core::time::FrameIndex;
use tt_core::tool::{ActiveTool, PointerFrame, Tool};
use tt_core::transport::Transport;
use tt_core::view::{ActiveView, map_at};

use crate::look::{Look, looks_of};
use crate::{Method, TRACK_CHANNELS, TrackRun, Tracker, is_tracker};

/// Channels of the human layer: `[x, y]`, source px.
pub const HUMAN_CHANNELS: usize = 2;
/// A drawn frame's box half-size (source px) where nothing gives one (a manual dot).
const DOT_HALF: f32 = 8.0;
/// Frames skipped between two of the pointer's (playing fast) are filled
/// in a line, up to this many; a jump farther (a seek) isn't.
const MAX_FILL: FrameIndex = 24;

/// A tracker's automatic results: what its runner writes. Its `Output` is
/// these with the human layer over them ([`compose`]).
#[derive(Component, Reflect, Clone, Copy, Debug, PartialEq)]
#[reflect(Component)]
pub struct AutoOutput(pub SignalId);

/// A tracker's human layer: `[x, y]` (source px) on each frame a person drew
/// its point. Made with the first drawing (one undo step with it).
#[derive(Component, Reflect, Clone, Copy, Debug, PartialEq)]
#[reflect(Component)]
pub struct HumanLayer(pub SignalId);

/// What a tracker's output was composed from the last time (derived):
/// snapshots of both layers (cheap: shared chunks), the output's version
/// after, and whether it was a manual dot.
#[derive(Component, Default)]
pub struct Composed {
    auto: Option<Signal>,
    human: Option<Signal>,
    output: u64,
    manual: bool,
}

/// Whether `tracker` is a manual dot (no algorithm: only what is drawn).
pub fn is_manual(world: &World, tracker: Entity) -> bool {
    world.get::<Tracker>(tracker).is_some_and(|t| t.method == Method::Manual)
}

/// The tracker's automatic results, made from its output if it has none
/// yet (a tracker saved before the human layer existed: its output was its
/// results). Not an undo step; marked for saving.
pub fn ensure_auto(world: &mut World, tracker: Entity) -> Option<SignalId> {
    if let Some(a) = world.get::<AutoOutput>(tracker) {
        return Some(a.0);
    }
    let out = world.get::<Output>(tracker)?.0;
    let copy = world.resource::<SignalStore>().get(out).cloned().unwrap_or_else(|| Signal::new(TRACK_CHANNELS));
    let id = {
        let mut store = world.resource_mut::<SignalStore>();
        let id = store.create(copy.channels());
        store.insert(id, copy);
        id
    };
    world.entity_mut(tracker).insert(AutoOutput(id));
    world.resource_mut::<History>().touch();
    Some(id)
}

/// The tracker's automatic results (None for a manual dot or a tracker without them yet).
pub fn auto_signal(world: &World, tracker: Entity) -> Option<&Signal> {
    world.resource::<SignalStore>().get(world.get::<AutoOutput>(tracker)?.0)
}

/// The tracker's human layer, if anything was ever drawn on it.
pub fn human_signal(world: &World, tracker: Entity) -> Option<&Signal> {
    world.resource::<SignalStore>().get(world.get::<HumanLayer>(tracker)?.0)
}

/// Where a person drew the tracker's point on frame `f` (source px).
pub fn drawn_at(world: &World, tracker: Entity, f: FrameIndex) -> Option<[f64; 2]> {
    human_signal(world, tracker)?.get(f).map(|v| [v[0] as f64, v[1] as f64])
}

/// How many frames have a drawn point, and the first and last.
pub fn drawn_frames(world: &World, tracker: Entity) -> (usize, Option<(FrameIndex, FrameIndex)>) {
    let Some(sig) = human_signal(world, tracker) else { return (0, None) };
    let Some((lo, hi)) = sig.present_hull() else { return (0, None) };
    (sig.runs(lo..hi + 1).iter().map(|(r, _)| (r.end - r.start) as usize).sum(), Some((lo, hi)))
}

/// A drawn frame's box half-size where the automatic result has none there:
/// the tracker's first look's, else a small dot's.
fn default_half(world: &World, tracker: Entity) -> [f32; 2] {
    looks_of(world, tracker).first().and_then(|l| world.get::<Look>(*l)).map_or([DOT_HALF; 2], |l| [l.half_w.max(2.0), l.half_h.max(2.0)])
}

/// One frame of the output from the layers: the drawn point (valid), else
/// the automatic result as it is, else nothing.
fn composed(f: FrameIndex, auto: Option<&Signal>, human: Option<&Signal>, half: [f32; 2]) -> Option<([f32; TRACK_CHANNELS], FrameState)> {
    let a = auto.and_then(|a| a.get(f)).filter(|v| v.len() == TRACK_CHANNELS);
    if let Some(p) = human.and_then(|h| h.get(f)) {
        let (x, y) = (p[0], p[1]);
        let [hx, hy] = a.map_or(half, |v| [((v[4] - v[2]) / 2.0).abs(), ((v[5] - v[3]) / 2.0).abs()]);
        return Some(([x, y, x - hx, y - hy, x + hx, y + hy, 1.0, 0.0], FrameState::Valid));
    }
    let v = a?;
    Some((std::array::from_fn(|c| v[c]), auto?.state(f)))
}

/// The frames where two snapshots of a layer differ (chunk-wise), or all of either's.
fn changed(now: Option<&Signal>, before: Option<&Signal>) -> RangeSet {
    let hull = |s: &Signal| s.present_hull().map(|(lo, hi)| lo..hi + 1);
    match (now, before) {
        (Some(a), Some(b)) => a.differing_chunks(b),
        (Some(s), None) | (None, Some(s)) => hull(s).map(RangeSet::from_range).unwrap_or_default(),
        (None, None) => RangeSet::new(),
    }
}

/// Bring `tracker`'s output up to date with its layers (see the module
/// docs), on the frames whose layers changed since the last time. Where
/// something else marked the output stale (an input changed), drawn frames
/// are made valid again: they depend on nothing.
pub fn compose(world: &mut World, tracker: Entity) {
    let (Some(out), Some(auto)) = (world.get::<Output>(tracker).map(|o| o.0), world.get::<AutoOutput>(tracker).map(|a| a.0)) else { return };
    let manual = is_manual(world, tracker);
    let half = default_half(world, tracker);
    let (a, h) = {
        let store = world.resource::<SignalStore>();
        (if manual { None } else { store.get(auto).cloned() }, world.get::<HumanLayer>(tracker).and_then(|id| store.get(id.0)).cloned())
    };
    let Some(version) = world.resource::<SignalStore>().get(out).map(Signal::version) else { return };
    let mut todo = RangeSet::new();
    let mut revalidate = false;
    match world.get::<Composed>(tracker) {
        Some(p) if p.manual == manual => {
            todo.union(&changed(a.as_ref(), p.auto.as_ref()));
            todo.union(&changed(h.as_ref(), p.human.as_ref()));
            revalidate = p.output != version;
        }
        // The first time (or a manual dot turned automatic, or back): everything.
        _ => {
            let store = world.resource::<SignalStore>();
            let hulls = [a.as_ref(), h.as_ref(), store.get(out)].into_iter().flatten().filter_map(Signal::present_hull);
            if let Some((lo, hi)) = hulls.reduce(|x, y| (x.0.min(y.0), x.1.max(y.1))) {
                todo.insert(lo..hi + 1);
            }
        }
    }
    // Drawn frames something else marked stale.
    if revalidate && let Some(h) = &h {
        let store = world.resource::<SignalStore>();
        let o = store.get(out).expect("checked");
        if let Some((lo, hi)) = h.present_hull() {
            for (r, _) in h.runs(lo..hi + 1) {
                for f in r.filter(|f| o.state(*f) != FrameState::Valid) {
                    todo.insert(f..f + 1);
                }
            }
        }
    }
    // Nothing changed: nothing to do (not even a new snapshot).
    if todo.is_empty() && world.get::<Composed>(tracker).is_some_and(|p| p.output == version && p.manual == manual) {
        return;
    }
    let mut written = RangeSet::new();
    if !todo.is_empty() {
        let mut store = world.resource_mut::<SignalStore>();
        let sig = store.get_mut(out).expect("checked");
        let mut clear: Vec<std::ops::Range<FrameIndex>> = Vec::new();
        for r in todo.ranges() {
            for f in r.clone() {
                match composed(f, a.as_ref(), h.as_ref(), half) {
                    Some((v, state)) => {
                        let same = sig.get(f).is_some_and(|w| w == v) && sig.state(f) == state;
                        if !same {
                            sig.set(f, &v);
                            if state == FrameState::Stale {
                                sig.mark_stale(f..f + 1);
                            }
                            written.insert(f..f + 1);
                        }
                    }
                    None if sig.get(f).is_some() => match clear.last_mut() {
                        Some(c) if c.end == f => c.end = f + 1,
                        _ => clear.push(f..f + 1),
                    },
                    None => {}
                }
            }
        }
        for c in clear {
            sig.clear(c.clone());
            written.insert(c);
        }
    }
    let version = world.resource::<SignalStore>().get(out).map_or(0, Signal::version);
    world.entity_mut(tracker).insert(Composed { auto: a, human: h, output: version, manual });
    if !written.is_empty() {
        let mut inv = world.resource_mut::<Invalidations>();
        for r in written.ranges() {
            inv.output_changed(tracker, r.clone());
        }
    }
}

/// `Set::Jobs`, after the runner: every tracker's output composed from its layers.
pub fn compose_trackers(world: &mut World) {
    let trackers: Vec<Entity> = {
        let mut q = world.query_filtered::<(Entity, &Operator), Without<Disabled>>();
        q.iter(world).filter(|(_, o)| o.kind == "track").map(|(e, _)| e).collect()
    };
    for t in trackers {
        if ensure_auto(world, t).is_some() {
            compose(world, t);
        }
    }
}

/// Write `points` (source px; None: erase) into `tracker`'s human layer, as
/// part of the edit `tx` (the layer is made on first use).
pub fn write_drawn(tx: &mut Tx<'_>, tracker: Entity, points: &[(FrameIndex, Option<[f64; 2]>)]) {
    let id = match tx.world().get::<HumanLayer>(tracker) {
        Some(h) => h.0,
        None if points.iter().all(|(_, p)| p.is_none()) => return,
        None => {
            let id = tx.create_signal(HUMAN_CHANNELS);
            tx.insert(tracker, HumanLayer(id));
            id
        }
    };
    let sig = tx.signal(id);
    for (f, p) in points {
        match p {
            Some([x, y]) => sig.set(*f, &[*x as f32, *y as f32]),
            None if sig.get(*f).is_some() => sig.clear(*f..*f + 1),
            None => {}
        }
    }
}

/// Erase what was drawn on `tracker` over `frames` (all of it: None). One undo step.
pub fn erase_drawn(world: &mut World, tracker: Entity, frames: Option<std::ops::Range<FrameIndex>>) -> bool {
    let Some(sig) = human_signal(world, tracker) else { return false };
    let Some((lo, hi)) = sig.present_hull() else { return false };
    let r = frames.unwrap_or(lo..hi + 1);
    let r = r.start.max(lo)..r.end.min(hi + 1);
    if r.is_empty() || sig.runs(r.clone()).is_empty() {
        return false;
    }
    let name = world.get::<Name>(tracker).map_or("the tracker".to_string(), |n| n.to_string());
    let id = world.get::<HumanLayer>(tracker).expect("has a layer").0;
    edit(world, &format!("Erase {name}'s drawing"), |tx| tx.signal(id).clear(r))
}

/// Merge the manual dots `dots` into `tracker`, as one undo step: what each
/// dot has drawn inside its lifetime goes into the tracker's human layer
/// (a later dot over an earlier one, both over what the tracker had drawn
/// there), so it overrides the tracker's automatic results on those frames.
/// Then the dots go, with their views; whatever used a dot (a subject's
/// member, a tracker's guide) uses the tracker instead. The tracker is
/// selected. Returns how many frames were written (None: nothing to merge).
pub fn merge_dots(world: &mut World, dots: &[Entity], tracker: Entity) -> Option<usize> {
    let live = |w: &World, e: Entity| w.get_entity(e).is_ok_and(|r| !r.contains::<Disabled>());
    if !live(world, tracker) || !is_tracker(world, tracker) {
        return None;
    }
    let dots: Vec<Entity> = dots.iter().copied().filter(|d| *d != tracker && live(world, *d) && is_manual(world, *d)).collect();
    let mut points: Vec<(FrameIndex, Option<[f64; 2]>)> = Vec::new();
    for &d in &dots {
        let span = tt_core::span::span_of(world, d);
        let Some(sig) = human_signal(world, d) else { continue };
        let Some((lo, hi)) = sig.present_hull() else { continue };
        for (r, _) in sig.runs(lo..hi + 1) {
            points.extend(r.filter(|f| span.contains(*f)).filter_map(|f| sig.get(f).map(|v| (f, Some([v[0] as f64, v[1] as f64])))));
        }
    }
    if dots.is_empty() || points.is_empty() {
        return None;
    }
    // Their views go with them; everything else that used a dot uses the tracker.
    let mut doomed = dots.clone();
    for &d in &dots {
        doomed.extend(tt_core::view::view_of(world, d));
    }
    let users: Vec<(Entity, Vec<(String, Entity)>)> = {
        let mut q = world.query_filtered::<(Entity, &Inputs), Without<Disabled>>();
        q.iter(world)
            .filter(|(e, i)| !doomed.contains(e) && i.0.iter().any(|(_, p)| dots.contains(p)))
            .map(|(e, i)| {
                let mut inputs: Vec<(String, Entity)> = Vec::new();
                for (slot, p) in &i.0 {
                    let p = if dots.contains(p) { tracker } else { *p };
                    // (A subject with both the dot and the tracker as members keeps the tracker once.)
                    if !(p == e || inputs.iter().any(|(s, q)| *s == *slot && *q == p)) {
                        inputs.push((slot.clone(), p));
                    }
                }
                (e, inputs)
            })
            .collect()
    };
    let name = |w: &World, e: Entity| w.get::<Name>(e).map_or("the tracker".to_string(), |n| n.to_string());
    let label = match dots.as_slice() {
        [one] => format!("Merge {} into {}", name(world, *one), name(world, tracker)),
        _ => format!("Merge {} dots into {}", dots.len(), name(world, tracker)),
    };
    let n = points.len();
    edit(world, &label, |tx| {
        write_drawn(tx, tracker, &points);
        for (e, inputs) in users {
            tx.modify::<Inputs>(e, |i| i.0 = inputs);
        }
        for e in doomed {
            tx.delete(e);
        }
    });
    world.resource_mut::<Selection>().select_only(tracker);
    Some(n)
}

fn manual_count(world: &mut World) -> usize {
    let mut q = world.query::<&Tracker>();
    q.iter(world).filter(|t| t.method == Method::Manual).count()
}

/// A new manual dot (no algorithm, only its human layer), anchored on
/// `frame`, as part of the edit `tx`. Its name is `name`.
pub fn spawn_manual_dot(tx: &mut Tx<'_>, name: String, frame: FrameIndex) -> Entity {
    let out = tx.create_signal(TRACK_CHANNELS);
    let auto = tx.create_signal(TRACK_CHANNELS);
    let human = tx.create_signal(HUMAN_CHANNELS);
    let tracker = Tracker { method: Method::Manual, ..Tracker::at(frame) };
    tx.spawn((
        Name::new(name),
        Operator { kind: "track".into() },
        Inputs(Vec::new()),
        Output(out),
        AutoOutput(auto),
        HumanLayer(human),
        tracker,
        crate::runner::TrackBook::default(),
        TrackRun::Paused,
    ))
}

/// The Draw tool's state: the hold in progress.
#[derive(Resource, Debug, Clone, Default)]
pub struct DrawTool {
    pub stroke: Option<Stroke>,
    /// Why the last press drew nothing (for the HUD), cleared by the next.
    pub refused: Option<String>,
}

/// A hold of the Draw tool: the tracker it draws on, whether it erases, and
/// the last frame and point it wrote (source px).
#[derive(Debug, Clone, Copy)]
pub struct Stroke {
    pub tracker: Entity,
    pub erase: bool,
    pub last: Option<(FrameIndex, [f64; 2])>,
}

/// The frames from `from` (exclusive) to `to` (inclusive), each with its
/// point on the line between `p` and `q` (a frame skipped while playing
/// fast); a jump farther than [`MAX_FILL`] (a seek) only writes `to`.
fn line(from: Option<(FrameIndex, [f64; 2])>, to: FrameIndex, q: [f64; 2]) -> Vec<(FrameIndex, [f64; 2])> {
    match from {
        Some((f, p)) if f != to && (to - f).abs() <= MAX_FILL => {
            let n = (to - f).abs();
            let dir = (to - f).signum();
            (1..=n)
                .map(|k| {
                    let u = k as f64 / n as f64;
                    (f + dir * k, [p[0] + (q[0] - p[0]) * u, p[1] + (q[1] - p[1]) * u])
                })
                .collect()
        }
        _ => vec![(to, q)],
    }
}

/// While a Draw stroke is held, every view waits to be recomputed (`op::Held`)
/// and catches up when it ends: a view following the tracker being drawn
/// would otherwise move with each frame drawn, under the pointer, so the
/// next point would land somewhere else and the drawing would run away.
pub fn hold_views_while_drawing(world: &mut World) {
    let drawing = world.resource::<DrawTool>().stroke.is_some();
    let held = if drawing {
        let mut q = world.query_filtered::<(Entity, &Operator), Without<Disabled>>();
        q.iter(world).filter(|(_, o)| o.kind == "frame").map(|(e, _)| e).collect()
    } else {
        std::collections::HashSet::new()
    };
    let mut h = world.resource_mut::<tt_core::op::Held>();
    if h.0 != held {
        h.0 = held;
    }
}

/// `Set::Tools`: the Draw tool (module docs).
pub fn draw_tool(world: &mut World) {
    if world.resource::<ActiveTool>().0 != Tool::Draw {
        if world.resource_mut::<DrawTool>().stroke.take().is_some() {
            world.resource_mut::<History>().end();
        }
        return;
    }
    let p = world.resource::<PointerFrame>().clone();
    let frame = world.resource::<Transport>().frame();
    let mods = world.resource::<KeysHeld>().mods;
    let mut tool = world.resource::<DrawTool>().clone();
    // Esc while holding: the hold never happened.
    if tool.stroke.is_some() && !world.resource_mut::<PendingActions>().take(|a| a == Action::Cancel).is_empty() {
        tool.stroke = None;
        if world.resource_mut::<History>().end() {
            undo(world);
        }
        *world.resource_mut::<DrawTool>() = tool;
        return;
    }
    // The pointer is in the shown space's pixels; the human layer is in source pixels.
    let to_source = |world: &World, at: [f64; 2]| map_at(world, world.resource::<ActiveView>().0, frame).to_source(at);
    if let Some(t) = p.pressed
        && tool.stroke.is_none()
        && let Some(at) = p.samples.iter().find(|s| s[0] >= t).map(|s| [s[1], s[2]]).or(p.hover)
    {
        tool.refused = None;
        let erase = mods.alt;
        let selected = world.resource::<Selection>().primary().filter(|e| is_tracker(world, *e));
        let target = selected.filter(|_| !mods.shift);
        match target {
            Some(t) => {
                let name = world.get::<Name>(t).map_or("the tracker".to_string(), |n| n.to_string());
                world.resource_mut::<History>().begin(if erase { format!("Erase {name}'s drawing") } else { format!("Draw {name}") });
                tool.stroke = Some(Stroke { tracker: t, erase, last: None });
            }
            None if erase => tool.refused = Some("Select a tracker to erase what was drawn on it (Alt+hold erases)".into()),
            None => {
                let name = format!("Manual dot {}", manual_count(world) + 1);
                world.resource_mut::<History>().begin(format!("Draw {name}"));
                let mut made = None;
                edit(world, &name, |tx| made = Some(spawn_manual_dot(tx, name.clone(), frame)));
                if let Some(t) = made {
                    world.resource_mut::<Selection>().select_only(t);
                    tool.stroke = Some(Stroke { tracker: t, erase: false, last: None });
                }
            }
        }
        if let Some(s) = tool.stroke.as_mut() {
            let src = to_source(world, at);
            let points: Vec<(FrameIndex, Option<[f64; 2]>)> = vec![(frame, (!s.erase).then_some(src))];
            let t = s.tracker;
            edit(world, "Draw", |tx| write_drawn(tx, t, &points));
            s.last = Some((frame, src));
        }
    }
    let ended = p.released.is_some() || !p.down;
    if let Some(s) = tool.stroke.as_mut() {
        let latest = p.samples.last().map(|s| [s[1], s[2]]).or(p.hover);
        if let Some(at) = latest {
            let src = to_source(world, at);
            // Only what moved: the same point on the same frame again writes nothing.
            if s.last.is_none_or(|(f, q)| f != frame || (q[0] - src[0]).hypot(q[1] - src[1]) > 1e-3) {
                let points: Vec<(FrameIndex, Option<[f64; 2]>)> = line(s.last, frame, src).into_iter().map(|(f, q)| (f, (!s.erase).then_some(q))).collect();
                let t = s.tracker;
                edit(world, "Draw", |tx| write_drawn(tx, t, &points));
                s.last = Some((frame, src));
            }
        }
        if ended {
            tool.stroke = None;
            world.resource_mut::<History>().end();
        }
    }
    *world.resource_mut::<DrawTool>() = tool;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_skipped_while_playing_are_filled_in_a_line_and_a_seek_is_not() {
        assert_eq!(line(None, 10, [5.0, 5.0]), vec![(10, [5.0, 5.0])]);
        assert_eq!(line(Some((10, [0.0, 0.0])), 10, [5.0, 5.0]), vec![(10, [5.0, 5.0])], "the same frame again");
        assert_eq!(line(Some((10, [0.0, 0.0])), 13, [3.0, 6.0]), vec![(11, [1.0, 2.0]), (12, [2.0, 4.0]), (13, [3.0, 6.0])]);
        assert_eq!(line(Some((13, [3.0, 6.0])), 11, [1.0, 2.0]), vec![(12, [2.0, 4.0]), (11, [1.0, 2.0])], "playing backward");
        assert_eq!(line(Some((0, [0.0, 0.0])), 500, [1.0, 1.0]), vec![(500, [1.0, 1.0])], "a jump");
    }

    #[test]
    fn a_drawn_frame_is_the_output_and_the_automatic_result_shows_elsewhere() {
        let mut auto = Signal::new(TRACK_CHANNELS);
        auto.set(5, &[10.0, 20.0, 6.0, 16.0, 14.0, 24.0, 0.8, 0.0]);
        auto.set(6, &[11.0, 21.0, 7.0, 17.0, 15.0, 25.0, 0.3, 1.0]);
        auto.mark_stale(6..7);
        let mut human = Signal::new(HUMAN_CHANNELS);
        human.set(6, &[50.0, 60.0]);
        human.set(9, &[70.0, 80.0]);
        let half = [DOT_HALF; 2];
        assert_eq!(composed(5, Some(&auto), Some(&human), half), Some(([10.0, 20.0, 6.0, 16.0, 14.0, 24.0, 0.8, 0.0], FrameState::Valid)));
        // Drawn over a lost, stale result: the drawn point, its box's size, trusted.
        assert_eq!(composed(6, Some(&auto), Some(&human), half), Some(([50.0, 60.0, 46.0, 56.0, 54.0, 64.0, 1.0, 0.0], FrameState::Valid)));
        // Drawn where nothing was tracked: a dot's box.
        assert_eq!(composed(9, Some(&auto), Some(&human), half), Some(([70.0, 80.0, 62.0, 72.0, 78.0, 88.0, 1.0, 0.0], FrameState::Valid)));
        assert_eq!(composed(7, Some(&auto), Some(&human), half), None);
        // A manual dot: its drawing alone.
        assert_eq!(composed(5, None, Some(&human), half), None);
    }
}
