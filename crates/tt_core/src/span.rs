//! Lifetimes on the timeline: when a timeline object (a tracker, a sketch, a
//! view) begins and ends.
//!
//! A [`Span`] trims an entity's output without touching it. Its data stays
//! as it is; whoever reads it through [`output`] (operators through
//! `EvalCtx::input`, views, overlays, the timeline, snapping, anticipatory
//! speed, trackers reading their guide) sees nothing outside the span, and
//! extending the span brings the frames back. A tracker's jobs stop at its
//! span's edges (tt_track's runner).
//!
//! - Each edge is optional: an untrimmed edge follows the data (a sketch that
//!   gets a stroke past its end grows there as before).
//! - Edits are ordinary document edits (one undo step each; a timeline drag
//!   is one gesture).
//! - A change re-derives what reads the entity (and, for a job operator, lets
//!   it plan again), like any change to its output.

use std::borrow::Cow;
use std::ops::Range;

use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::name::Name;
use bevy_ecs::prelude::*;
use bevy_reflect::Reflect;

use crate::app::{AppBuilder, Module, Set};
use crate::history::{History, edit};
use crate::meta::Class;
use crate::op::{Invalidations, OpRegistry, Operator, Output};
use crate::signal::{Signal, SignalStore};
use crate::time::FrameIndex;
use crate::transport::Transport;

/// Frames far beyond any clip: what an untrimmed edge stands for.
const OPEN: FrameIndex = 1 << 40;

/// The frames an entity is alive on: `first ..= last` (None: untrimmed on that side).
#[derive(Component, Reflect, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[reflect(Component)]
pub struct Span {
    pub first: Option<FrameIndex>,
    pub last: Option<FrameIndex>,
}

impl Span {
    pub fn new(first: FrameIndex, last: FrameIndex) -> Self {
        Self { first: Some(first), last: Some(last) }
    }

    /// The frames it keeps, half-open (untrimmed edges are far away).
    pub fn range(&self) -> Range<FrameIndex> {
        self.first.unwrap_or(-OPEN)..self.last.map_or(OPEN, |l| l + 1)
    }

    pub fn contains(&self, f: FrameIndex) -> bool {
        self.range().contains(&f)
    }

    /// `f` moved inside the span (an empty span keeps its first frame).
    pub fn clamp(&self, f: FrameIndex) -> FrameIndex {
        let r = self.range();
        f.min(r.end - 1).max(r.start)
    }

    pub fn is_trimmed(&self) -> bool {
        self.first.is_some() || self.last.is_some()
    }

    /// `first ..= last` of `hull` (the data's first and last frame) inside the span, if any is left.
    pub fn trim(&self, hull: (FrameIndex, FrameIndex)) -> Option<(FrameIndex, FrameIndex)> {
        let r = self.range();
        let (a, b) = (hull.0.max(r.start), hull.1.min(r.end - 1));
        (a <= b).then_some((a, b))
    }
}

/// The frames an entity could have data on, where that isn't the data it
/// has yet: a tracker's are its guide's frames in the directions it tracks
/// (derived; its runner keeps it). An edge dragged to its end untrims.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reach(pub FrameIndex, pub FrameIndex);

/// Which end of a span.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    First,
    Last,
}

/// An entity's span (untrimmed if it has none).
pub fn span_of(world: &World, e: Entity) -> Span {
    world.get::<Span>(e).copied().unwrap_or_default()
}

impl Signal {
    /// This signal without the frames outside `range` (a cheap copy: chunks
    /// are shared, and only the two at the edges are copied).
    pub fn clipped(&self, range: Range<FrameIndex>) -> Signal {
        let mut s = self.clone();
        if let Some((lo, hi)) = s.present_hull() {
            if lo < range.start {
                s.clear(lo..range.start.min(hi + 1));
            }
            if hi >= range.end {
                s.clear(range.end.max(lo)..hi + 1);
            }
        }
        s
    }
}

/// `sig` as seen through `span`: itself when untrimmed or already inside it.
pub fn clip(sig: &Signal, span: Span) -> Cow<'_, Signal> {
    let r = span.range();
    match sig.present_hull() {
        Some((lo, hi)) if span.is_trimmed() && (lo < r.start || hi >= r.end) => Cow::Owned(sig.clipped(r)),
        _ => Cow::Borrowed(sig),
    }
}

/// A producer's output as everything downstream sees it: nothing outside its span.
pub fn output(world: &World, e: Entity) -> Option<Cow<'_, Signal>> {
    let sig = world.resource::<SignalStore>().get(world.get::<Output>(e)?.0)?;
    Some(clip(sig, span_of(world, e)))
}

/// The first and last frame an entity's data covers, untrimmed (a stroke:
/// the frames it visited). What its span can be extended to without new data.
pub fn natural_span(world: &World, e: Entity) -> Option<(FrameIndex, FrameIndex)> {
    if world.get::<Disabled>(e).is_some() {
        return None;
    }
    if world.get::<crate::sketch::Capture>(e).is_some() {
        let (first, shown) = world.get::<crate::sketch::ClockMap>(e)?.frame_times()?;
        let a = shown.iter().position(Option::is_some)?;
        let b = shown.iter().rposition(Option::is_some)?;
        return Some((first + a as FrameIndex, first + b as FrameIndex));
    }
    let out = world.get::<Output>(e)?;
    world.resource::<SignalStore>().get(out.0)?.present_hull()
}

/// How far an entity's span can reach: its [`Reach`], else its data.
pub fn extent_of(world: &World, e: Entity) -> Option<(FrameIndex, FrameIndex)> {
    match world.get::<Reach>(e) {
        Some(r) if world.get::<Disabled>(e).is_none() => Some((r.0, r.1)),
        _ => natural_span(world, e),
    }
}

/// The first and last frame an entity is alive on: its data, trimmed by its span.
pub fn live_span(world: &World, e: Entity) -> Option<(FrameIndex, FrameIndex)> {
    span_of(world, e).trim(natural_span(world, e)?)
}

/// `span` with `edge` moved to `frame`, kept at least one frame long. Moving
/// an edge onto (or past) the end of `natural` (the entity's [`extent_of`])
/// untrims it, so it follows the data again.
pub fn moved(span: Span, natural: Option<(FrameIndex, FrameIndex)>, edge: Edge, frame: FrameIndex) -> Span {
    let r = span.range();
    match edge {
        Edge::First => {
            let f = frame.min(r.end - 1);
            Span { first: (natural.is_none_or(|(a, _)| f > a)).then_some(f), ..span }
        }
        Edge::Last => {
            let f = frame.max(r.start);
            Span { last: (natural.is_none_or(|(_, b)| f < b)).then_some(f), ..span }
        }
    }
}

/// Set `e`'s span (as part of the open gesture, or one undo step). An
/// untrimmed span removes the component.
pub fn set_span(world: &mut World, e: Entity, span: Span) -> bool {
    if span_of(world, e) == span {
        return false;
    }
    let name = world.get::<Name>(e).map_or_else(|| "item".to_string(), |n| n.to_string());
    edit(world, &format!("Trim {name}"), |tx| {
        if span.is_trimmed() {
            tx.insert(e, span);
        } else {
            tx.remove::<Span>(e);
        }
    })
}

/// Move one edge of `e`'s span to `frame` (see [`moved`]).
pub fn move_edge(world: &mut World, e: Entity, edge: Edge, frame: FrameIndex) -> bool {
    let span = moved(span_of(world, e), extent_of(world, e), edge, frame);
    set_span(world, e, span)
}

/// Start dragging an edge: the drag's edits are one undo step until [`end_drag`].
pub fn begin_drag(world: &mut World, e: Entity) {
    let name = world.get::<Name>(e).map_or_else(|| "item".to_string(), |n| n.to_string());
    world.resource_mut::<History>().begin(format!("Trim {name}"));
}

pub fn end_drag(world: &mut World) {
    world.resource_mut::<History>().end();
}

/// A span changed (or went): what reads the entity re-derives, and a job
/// operator (a tracker) plans again: its jobs stop at the new edges, or go
/// on into frames it didn't have.
fn span_changed(
    changed: Query<Entity, Changed<Span>>,
    mut removed: RemovedComponents<Span>,
    ops: Query<&Operator>,
    registry: Res<OpRegistry>,
    mut inv: ResMut<Invalidations>,
    t: Res<Transport>,
) {
    let frames = 0..t.frame_count;
    for e in changed.iter().chain(removed.read()) {
        inv.output_changed(e, frames.clone());
        if ops.get(e).ok().and_then(|o| registry.get(&o.kind)).is_some_and(|k| k.job()) {
            inv.recompute(e, frames.clone());
        }
    }
}

pub struct SpanModule;

impl Module for SpanModule {
    fn build(&self, app: &mut AppBuilder) {
        app.component::<Span>(Class::Document).declare::<Reach>(Class::Derived).add_systems(span_changed.in_set(Set::Invalidate).before(crate::op::propagate));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipping_keeps_only_the_span_and_shares_the_rest() {
        let mut s = Signal::new(1);
        s.write(0, &(0..2000).map(|i| i as f32).collect::<Vec<_>>());
        let c = s.clipped(300..1500);
        assert_eq!(c.present_hull(), Some((300, 1499)));
        assert_eq!(c.get(300), Some(&[300.0][..]));
        assert_eq!(c.get(299), None);
        assert_eq!(s.shared_chunks_with(&c), 3, "whole chunks inside the span are shared (512..1280)");
        assert_eq!(s.present_hull(), Some((0, 1999)), "the original keeps everything");
    }

    #[test]
    fn edges_follow_the_data_unless_trimmed() {
        let open = Span::default();
        assert!(open.contains(-5) && open.contains(1 << 30));
        let s = moved(open, Some((0, 999)), Edge::First, 200);
        assert_eq!(s, Span { first: Some(200), last: None });
        let s = moved(s, Some((0, 999)), Edge::Last, 100);
        assert_eq!(s, Span::new(200, 200), "an edge can't pass the other");
        let s = moved(s, Some((0, 999)), Edge::First, -10);
        assert_eq!(s, Span { first: None, last: Some(200) }, "dragged past the data's start: untrimmed there");
        assert_eq!(s.trim((0, 999)), Some((0, 200)));
        assert_eq!(Span::new(1200, 1300).trim((0, 999)), None);
    }
}
