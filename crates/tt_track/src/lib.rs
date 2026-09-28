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
use tt_core::history::edit;
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
    out.sort_by_key(|e| e.index_u32());
    out
}

/// Add a tracker following `guide` (any box producer), seeded at `frame`
/// (moved into the guide's frames if outside them), tracking in `space`
/// (a view; None = the source). One undo step; the tracker is selected.
pub fn add_tracker(world: &mut World, guide: Entity, frame: FrameIndex, space: Option<Entity>) -> Option<Entity> {
    let sig = world.resource::<SignalStore>().get(world.get::<Output>(guide)?.0)?;
    if sig.channels() < 6 {
        return None;
    }
    let (lo, hi) = sig.present_hull()?;
    let anchor = frame.clamp(lo, hi);
    let guide_name = world.get::<Name>(guide).map_or("box".to_string(), |n| n.to_string());
    let n = {
        let mut q = world.query::<&Operator>();
        q.iter(world).filter(|o| o.kind == "track").count() + 1
    };
    let mut made = None;
    edit(world, &format!("Track {guide_name}"), |tx| {
        let out = tx.create_signal(TRACK_CHANNELS);
        let mut inputs = vec![("guide".to_string(), guide)];
        inputs.extend(space.map(|v| ("space".to_string(), v)));
        made = Some(tx.spawn((Name::new(format!("Tracker {n}")), Operator { kind: "track".into() }, Inputs(inputs), Output(out), Tracker::at(anchor), runner::TrackBook::default())));
    });
    let tracker = made?;
    world.resource_mut::<Selection>().select_only(tracker);
    Some(tracker)
}

/// `T`: track each selected sketch from the playhead, in the view being
/// looked at; on a selected tracker, re-seed it at the playhead.
fn apply_track_actions(world: &mut World) {
    if world.resource_mut::<PendingActions>().take(|a| a == Action::Track).is_empty() {
        return;
    }
    let frame = world.resource::<Transport>().frame();
    let space = world.resource::<ActiveView>().0;
    let selected = world.resource::<Selection>().entities.clone();
    let mut guides: Vec<Entity> = Vec::new();
    for e in selected {
        if is_tracker(world, e) {
            if world.get::<Tracker>(e).is_some_and(|t| t.anchor != frame) {
                edit(world, "Re-seed tracker", |tx| tx.modify::<Tracker>(e, |t| t.anchor = frame));
            }
        } else if let Some(s) = tt_core::sketch::sketch_of(world, e)
            && !guides.contains(&s)
        {
            guides.push(s);
        }
    }
    let mut made = Vec::new();
    for g in guides {
        made.extend(add_tracker(world, g, frame, space));
    }
    if made.len() > 1 {
        let mut sel = world.resource_mut::<Selection>();
        sel.entities = made;
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
