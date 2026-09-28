//! Automatic trackers (DESIGN §6, M6): operators that look at pixels.
//!
//! A tracker is a `track` operator entity:
//! - input `guide`: a box producer, normally a sketch. The rough pass does
//!   the hard part: it says roughly where the subject is on every frame, so
//!   the tracker only searches the guide's box and predicts from the guide's
//!   motion. Where the tracker loses the subject, it follows the guide.
//! - input `space` (optional): the view it tracks in (stabilized, magnified).
//!   Its patches are resampled through that view and its results lifted back
//!   to source pixels, so a tracker in a tight view still outputs source px.
//! - output: `[x, y, left, top, right, bottom, score]` in source px; the
//!   first six channels are a box, so anything that takes a sketch (a view's
//!   framing, overlays) takes a tracker too.
//!
//! Trackers are *job* operators: evaluation leaves their dirty frames to
//! [`runner`], which runs them in background threads forward and backward
//! from the anchor, keeps old results on screen as stale until new ones
//! arrive, and can hold them to the playhead (catch-up mode).

pub mod image;
pub mod job;
pub mod ncc;
pub mod runner;
pub mod template;

use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::name::Name;
use bevy_ecs::prelude::*;
use bevy_reflect::Reflect;
use tt_core::history::{Tx, edit};
use tt_core::input::{Action, PendingActions};
use tt_core::op::{EvalCtx, Footprint, Inputs, Operator, OperatorKind, Output};
use tt_core::selection::Selection;
use tt_core::signal::{Signal, SignalStore};
use tt_core::time::FrameIndex;
use tt_core::transport::Transport;
use tt_core::view::ActiveView;
use tt_core::{AppBuilder, Class, Module, Set};

pub use runner::{Footage, TrackStatus};

/// Channels of a tracker's output: `[x, y, left, top, right, bottom, score]`.
pub const TRACK_CHANNELS: usize = 7;

/// Which way a tracker runs from its anchor.
#[derive(Reflect, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Direction {
    #[default]
    Both,
    Forward,
    Backward,
}

/// Which copy of the video a tracker reads.
#[derive(Reflect, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Rendition {
    /// The proxy when it has at least one pixel per tracked pixel, else the original.
    #[default]
    Auto,
    Original,
    Proxy,
}

/// A tracker's settings (re-tunable: every change re-tracks).
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component)]
pub struct Tracker {
    /// The frame it was seeded on (the guide's point there); it runs forward and backward from here.
    pub anchor: FrameIndex,
    pub direction: Direction,
    /// Track only as far as the playhead, catching up as it moves, instead of the whole guide in the background.
    pub follow_playhead: bool,
    /// Size of the followed feature, as a fraction of the guide box's half-size at the anchor.
    pub feature: f32,
    /// Searched region, as a multiple of the guide's box.
    pub search: f32,
    /// Appearance: 0 = always the anchor's look, 1 = the previous frame's.
    pub adapt: f32,
    /// Match score (0–1) below which a frame counts as lost; lost frames follow the guide.
    pub min_score: f32,
    pub rendition: Rendition,
    /// Shift the finished path onto the guide's average position (the median
    /// offset), instead of wherever the guide was at the anchor.
    pub center_on_guide: bool,
}

impl Tracker {
    pub fn at(anchor: FrameIndex) -> Self {
        Self { anchor, direction: Direction::Both, follow_playhead: false, feature: 0.4, search: 1.0, adapt: 0.25, min_score: 0.5, rendition: Rendition::Auto, center_on_guide: true }
    }
}

/// `track`: see the module docs. Evaluated by jobs, never inline.
pub struct TrackKind;

impl OperatorKind for TrackKind {
    fn name(&self) -> &'static str {
        "track"
    }

    fn channels(&self) -> usize {
        TRACK_CHANNELS
    }

    fn footprint(&self, op: EntityRef<'_>) -> Footprint {
        Footprint::Radiating(op.get::<Tracker>().map_or(0, |t| t.anchor))
    }

    fn evaluate(&self, _: &EvalCtx<'_>, _: std::ops::Range<FrameIndex>, _: &mut Signal) -> anyhow::Result<()> {
        Ok(())
    }

    fn job(&self) -> bool {
        true
    }
}

pub fn is_tracker(world: &World, e: Entity) -> bool {
    world.get::<Operator>(e).is_some_and(|o| o.kind == "track")
}

/// The box a tracker follows (its `guide` input).
pub fn guide_of(world: &World, tracker: Entity) -> Option<Entity> {
    world.get::<Inputs>(tracker)?.0.iter().find(|(s, _)| s == "guide").map(|(_, e)| *e)
}

/// Live trackers guided by `guide`.
pub fn trackers_of(world: &mut World, guide: Entity) -> Vec<Entity> {
    let mut q = world.query_filtered::<(Entity, &Operator, &Inputs), Without<Disabled>>();
    let mut out: Vec<Entity> =
        q.iter(world).filter(|(_, o, i)| o.kind == "track" && i.0.iter().any(|(s, p)| s == "guide" && *p == guide)).map(|(e, _, _)| e).collect();
    tt_core::meta::creation_order(world, &mut out);
    out
}

/// Where a tracker following `guide` seeded at `frame` starts: `frame` moved
/// into the guide's frames. None if the guide isn't a box with frames.
fn anchor_in(world: &World, guide: Entity, frame: FrameIndex) -> Option<FrameIndex> {
    let sig = world.resource::<SignalStore>().get(world.get::<Output>(guide)?.0)?;
    if sig.channels() < 6 {
        return None;
    }
    let (lo, hi) = sig.present_hull()?;
    Some(frame.clamp(lo, hi))
}

/// A tracker entity following `guide` from `anchor` in `space`, as part of an edit.
fn spawn_tracker(tx: &mut Tx<'_>, name: String, guide: Entity, anchor: FrameIndex, space: Option<Entity>) -> Entity {
    let out = tx.create_signal(TRACK_CHANNELS);
    let mut inputs = vec![("guide".to_string(), guide)];
    inputs.extend(space.map(|v| ("space".to_string(), v)));
    tx.spawn((Name::new(name), Operator { kind: "track".into() }, Inputs(inputs), Output(out), Tracker::at(anchor), runner::TrackBook::default()))
}

fn tracker_count(world: &mut World) -> usize {
    let mut q = world.query::<&Operator>();
    q.iter(world).filter(|o| o.kind == "track").count()
}

/// Add a tracker following `guide` (any box producer), seeded at `frame`
/// (moved into the guide's frames if outside them), tracking in `space`
/// (a view; None = the source). One undo step; the tracker is selected.
pub fn add_tracker(world: &mut World, guide: Entity, frame: FrameIndex, space: Option<Entity>) -> Option<Entity> {
    let anchor = anchor_in(world, guide, frame)?;
    let guide_name = world.get::<Name>(guide).map_or("box".to_string(), |n| n.to_string());
    let name = format!("Tracker {}", tracker_count(world) + 1);
    let mut made = None;
    edit(world, &format!("Track {guide_name}"), |tx| made = Some(spawn_tracker(tx, name, guide, anchor, space)));
    let tracker = made?;
    world.resource_mut::<Selection>().select_only(tracker);
    Some(tracker)
}

/// `T`: track each selected sketch from the playhead, in the view being
/// looked at; on a selected tracker, re-seed it at the playhead (moved into
/// its guide's frames). One undo step for all of it.
fn apply_track_actions(world: &mut World) {
    if world.resource_mut::<PendingActions>().take(|a| a == Action::Track).is_empty() {
        return;
    }
    let frame = world.resource::<Transport>().frame();
    let space = world.resource::<ActiveView>().0;
    let selected = world.resource::<Selection>().entities.clone();
    // (tracker or guide, anchor)
    let mut reseed: Vec<(Entity, FrameIndex)> = Vec::new();
    let mut guides: Vec<(Entity, FrameIndex)> = Vec::new();
    for e in selected {
        if is_tracker(world, e) {
            let anchor = guide_of(world, e).and_then(|g| anchor_in(world, g, frame));
            if let Some(a) = anchor.filter(|a| world.get::<Tracker>(e).is_some_and(|t| t.anchor != *a)) {
                reseed.push((e, a));
            }
        } else if let Some(s) = tt_core::sketch::sketch_of(world, e)
            && !guides.iter().any(|(g, _)| *g == s)
            && let Some(a) = anchor_in(world, s, frame)
        {
            guides.push((s, a));
        }
    }
    let name_of = |world: &World, e: Entity| world.get::<Name>(e).map_or("box".to_string(), |n| n.to_string());
    let label = match (reseed.len(), guides.as_slice()) {
        (0, []) => return,
        (0, [(g, _)]) => format!("Track {}", name_of(world, *g)),
        (0, many) => format!("Track {} sketches", many.len()),
        (_, []) => "Re-seed tracker".to_string(),
        _ => "Track".to_string(),
    };
    let first = tracker_count(world) + 1;
    let mut made = Vec::new();
    edit(world, &label, |tx| {
        for (e, a) in reseed {
            tx.modify::<Tracker>(e, |t| t.anchor = a);
        }
        for (i, (g, a)) in guides.into_iter().enumerate() {
            made.push(spawn_tracker(tx, format!("Tracker {}", first + i), g, a, space));
        }
    });
    if !made.is_empty() {
        world.resource_mut::<Selection>().entities = made;
    }
}

pub struct TrackModule;

impl Module for TrackModule {
    fn build(&self, app: &mut AppBuilder) {
        app.operator(TrackKind)
            .operator_params::<Tracker>()
            .component::<runner::TrackBook>(Class::Document)
            .register_type::<Direction>()
            .register_type::<Rendition>()
            .declare::<TrackStatus>(Class::Derived)
            .declare::<runner::TrackJobs>(Class::Derived)
            .declare::<Footage>(Class::Derived)
            .init_resource::<runner::TrackJobs>()
            .add_systems(apply_track_actions.in_set(Set::Intents))
            .add_systems(runner::run_trackers.in_set(Set::Jobs));
    }
}
