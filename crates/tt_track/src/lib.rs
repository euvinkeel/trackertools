//! Automatic trackers (DESIGN §6, M6): operators that look at pixels.
//!
//! A tracker is a `track` operator entity:
//! - input `guide`: a box producer, normally a sketch. The rough pass does
//!   the hard part: it says roughly where the subject is on every frame, so
//!   the tracker only searches the guide's box and predicts from the guide's
//!   motion. Where the tracker loses the subject, it follows the guide.
//! - input `space` (optional): the view it tracks in (stabilized, magnified;
//!   by default the guide's own view). Its patches are resampled through
//!   that view and its results lifted back to source pixels, so a tracker in
//!   a tight view still outputs source px.
//! - inputs `look` ([`look::Look`] entities, DESIGN §6.3): what the subject
//!   looks like. The first is the seed: the tracker starts on its frame,
//!   exactly at its centre. Each is a template, masked where painted.
//! - output: `[x, y, left, top, right, bottom, score, flags]` in source px;
//!   the first six channels are a box, so anything that takes a sketch (a
//!   view's framing, overlays) takes a tracker too. `flags` marks frames not
//!   to trust ([`LOST`], [`OUTSIDE`]); their values stay (non-destructive).
//!
//! Trackers are *job* operators: evaluation leaves their dirty frames to
//! [`runner`], which runs them in background threads forward and backward
//! from the anchor, keeps old results on screen as stale until new ones
//! arrive, and can hold them to the playhead (catch-up mode). Which way
//! they track is the user's to ask ([`TrackRun`]): new trackers wait.

pub mod export;
pub mod image;
pub mod job;
pub mod look;
pub mod ncc;
pub mod runner;
pub mod template;
pub mod tool;

use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::name::Name;
use bevy_ecs::prelude::*;
use bevy_reflect::Reflect;
use look::Look;
use tt_core::history::{History, Tx, edit};
use tt_core::input::{Action, PendingActions};
use tt_core::op::{EvalCtx, Footprint, Inputs, Operator, OperatorKind, Output};
use tt_core::selection::Selection;
use tt_core::signal::{Signal, SignalStore};
use tt_core::time::FrameIndex;
use tt_core::transport::Transport;
use tt_core::view::ActiveView;
use tt_core::{AppBuilder, Class, Module, Set};

pub use runner::{Footage, TrackStatus};

/// Channels of a tracker's output: `[x, y, left, top, right, bottom, score, flags]`.
pub const TRACK_CHANNELS: usize = 8;
/// Flag: the match scored below `min_score`; the position is the guide's prediction.
pub const LOST: u32 = 1;
/// Flag: the point left the guide's box (the rough pass says the subject isn't there).
pub const OUTSIDE: u32 = 2;

/// A tracker frame's flags (0 = trustworthy); frames of older 7-channel outputs have none.
pub fn flags(v: &[f32]) -> u32 {
    v.get(7).map_or(0, |f| *f as u32)
}

/// Which way a tracker runs from its anchor.
#[derive(Reflect, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Direction {
    #[default]
    Both,
    Forward,
    Backward,
}

/// How a tracker follows its subject between its looks.
#[derive(Reflect, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Method {
    /// Its looks as templates, matched on every frame (built in).
    #[default]
    Template,
    /// CoTracker3 (Meta's learned point tracker, CC-BY-NC weights): seeded at
    /// the looks' points, run by a Python worker on the job's frames.
    CoTracker,
}

/// What a tracker is asked to do (its buttons): track forward from its
/// anchor, backward, both ways, or nothing for now. Not a setting: switching
/// keeps every result and only says which side's jobs may run (and keep
/// re-tracking after edits). New trackers start paused ([`NewTrackers`]);
/// a tracker saved before this existed tracks both ways, as it always did.
#[derive(Component, Reflect, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[reflect(Component)]
pub enum TrackRun {
    #[default]
    Paused,
    Forward,
    Backward,
    Both,
}

impl TrackRun {
    pub fn forward(self) -> bool {
        matches!(self, TrackRun::Forward | TrackRun::Both)
    }

    pub fn backward(self) -> bool {
        matches!(self, TrackRun::Backward | TrackRun::Both)
    }
}

/// A tracker's run state (one without any, from before: both ways).
pub fn run_of(world: &World, tracker: Entity) -> TrackRun {
    world.get::<TrackRun>(tracker).copied().unwrap_or(TrackRun::Both)
}

/// Ask `tracker` to track one way, both, or pause. Like its results, not an
/// undo step (undoing an edit shouldn't stop tracking), but saved.
pub fn set_run(world: &mut World, tracker: Entity, run: TrackRun) {
    if !is_tracker(world, tracker) || run_of(world, tracker) == run {
        return;
    }
    world.entity_mut(tracker).insert(run);
    world.resource_mut::<History>().touch();
}

/// What new trackers start with.
#[derive(Resource, Clone, Copy, Debug)]
pub struct NewTrackers {
    /// Templates or CoTracker (the Track tool's two buttons).
    pub method: Method,
    /// Paused: each starts when asked (scripted runs and tests start them at once).
    pub run: TrackRun,
}

impl Default for NewTrackers {
    fn default() -> Self {
        Self { method: Method::Template, run: TrackRun::Paused }
    }
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
    /// With no looks (an older tracker): the followed feature's size, as a
    /// fraction of the guide box's half-size at the anchor.
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
    /// Track each stretch between two looks from both ends and keep, frame
    /// by frame, the better pass (a look placed where it missed mends the
    /// frames before it too). Off: each side tracks one way.
    #[reflect(default = "yes")]
    pub fuse: bool,
    /// How alike a place must look to count as one of its looks.
    #[reflect(default)]
    pub matching: Matching,
    /// Templates (built in), or CoTracker3 (a Python worker; see `job::learned`).
    #[reflect(default)]
    pub method: Method,
}

/// How alike a place must look to count as one of a tracker's looks
/// (`ncc::Tolerance`). The defaults are how trackers matched before these
/// options existed.
#[derive(Reflect, Clone, Debug, PartialEq)]
pub struct Matching {
    /// Its contrast may differ from a look's by this factor either way for
    /// free (2: half or double); beyond, the score falls in proportion.
    /// Raise it where the whole picture dims or brightens (a menu's backdrop).
    pub contrast: f32,
    /// A painted look's brightness may differ by this many of its spreads
    /// for free; the score is gone two spreads further.
    pub brightness: f32,
    /// Compare the colour too, not only brightness: a white cursor and a
    /// yellow marker of the same shape are nearly twins in brightness.
    pub colour: bool,
    /// With `colour`: how far the colour may differ for free (chroma levels,
    /// 0–255); the score is gone at twice this.
    pub colour_slack: f32,
}

impl Default for Matching {
    fn default() -> Self {
        Self { contrast: 2.0, brightness: 1.0, colour: false, colour_slack: 20.0 }
    }
}

impl Matching {
    pub fn tolerance(&self) -> ncc::Tolerance {
        ncc::Tolerance { contrast: self.contrast.max(1.0), brightness: self.brightness.max(0.0), colour: self.colour.then_some(self.colour_slack.max(0.5)) }
    }
}

fn yes() -> bool {
    true
}

impl Tracker {
    pub fn at(anchor: FrameIndex) -> Self {
        Self { anchor, direction: Direction::Both, follow_playhead: false, feature: 0.4, search: 1.0, adapt: 0.25, min_score: 0.6, rendition: Rendition::Auto, center_on_guide: false, fuse: true, matching: Matching::default(), method: Method::Template }
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

/// A tracker entity following `guide` from `look` (its seed) in `space`, as part of an edit.
/// `placed`: the user put the look there (else it came from the guide's
/// point, so the finished path is re-centred on the guide).
fn spawn_tracker(tx: &mut Tx<'_>, name: String, guide: Entity, look: Look, space: Option<Entity>, placed: bool) -> Entity {
    let out = tx.create_signal(TRACK_CHANNELS);
    let anchor = look.frame;
    let look = tx.spawn((Name::new("Look 1"), look));
    let mut inputs = vec![("guide".to_string(), guide)];
    inputs.extend(space.map(|v| ("space".to_string(), v)));
    inputs.push(("look".to_string(), look));
    let new = tx.world().get_resource::<NewTrackers>().copied().unwrap_or_default();
    let tracker = Tracker { center_on_guide: !placed, method: new.method, ..Tracker::at(anchor) };
    tx.spawn((Name::new(name), Operator { kind: "track".into() }, Inputs(inputs), Output(out), tracker, runner::TrackBook::default(), new.run))
}

/// A look around the guide's point on `frame`: a square of `feature` × its
/// box (the quick way, when the user didn't draw one).
fn look_from_guide(world: &World, guide: Entity, frame: FrameIndex) -> Option<Look> {
    let v = world.resource::<SignalStore>().get(world.get::<Output>(guide)?.0)?.get(frame)?;
    let half = 0.4 * ((v[0] - v[2]).max(v[4] - v[0])).min((v[1] - v[3]).max(v[5] - v[1])) as f64;
    Some(Look::new(frame, [v[0] as f64, v[1] as f64], [half, half]))
}

/// The view a tracker following `guide` works in: the guide's own
/// (stabilized) view, made if it has none (inside the caller's gesture).
fn tracking_view(world: &mut World, guide: Entity) -> Option<Entity> {
    tt_core::sketch::is_sketch(world, guide).then(|| tt_core::view::ensure_view(world, guide))
}

/// Add a tracker following `guide` whose first look is `look` (its frame
/// must be one of the guide's), tracking in the guide's view. One undo step;
/// the tracker is selected.
pub fn add_tracker_with_look(world: &mut World, guide: Entity, look: Look) -> Option<Entity> {
    if anchor_in(world, guide, look.frame)? != look.frame {
        return None;
    }
    let guide_name = world.get::<Name>(guide).map_or("box".to_string(), |n| n.to_string());
    let name = format!("Tracker {}", tracker_count(world) + 1);
    world.resource_mut::<History>().begin(format!("Track {guide_name}"));
    let space = tracking_view(world, guide);
    let mut made = None;
    edit(world, "Track", |tx| made = Some(spawn_tracker(tx, name, guide, look, space, true)));
    world.resource_mut::<History>().end();
    let tracker = made?;
    world.resource_mut::<Selection>().select_only(tracker);
    Some(tracker)
}

/// Start `tracker` again from `look` (one undo step): the look goes first
/// (it seeds) and its frame becomes the anchor.
pub fn reseed_with_look(world: &mut World, tracker: Entity, look: Look) -> Option<Entity> {
    let n = look::looks_of(world, tracker).len() + 1;
    let frame = look.frame;
    let mut made = None;
    edit(world, "Re-seed tracker", |tx| {
        let e = tx.spawn((Name::new(format!("Look {n}")), look));
        tx.modify::<Inputs>(tracker, |i| {
            let at = i.0.iter().position(|(s, _)| s == "look").unwrap_or(i.0.len());
            i.0.insert(at, ("look".to_string(), e));
        });
        tx.modify::<Tracker>(tracker, |t| t.anchor = frame);
        made = Some(e);
    });
    made
}

/// Another look for `tracker` (one undo step); it re-tracks with all of
/// them, pinned on the look's frame.
pub fn add_look(world: &mut World, tracker: Entity, look: Look) -> Option<Entity> {
    let n = look::looks_of(world, tracker).len() + 1;
    let mut made = None;
    edit(world, "Add look", |tx| {
        let e = tx.spawn((Name::new(format!("Look {n}")), look));
        tx.modify::<Inputs>(tracker, |i| i.0.push(("look".to_string(), e)));
        made = Some(e);
    });
    made
}

fn tracker_count(world: &mut World) -> usize {
    let mut q = world.query::<&Operator>();
    q.iter(world).filter(|o| o.kind == "track").count()
}

/// Add a tracker following `guide` (any box producer) from its point at
/// `frame` (moved into the guide's frames if outside them), its look a
/// square of the guide's box there, tracking in `space` (a view; None = the
/// source). One undo step; the tracker is selected.
pub fn add_tracker(world: &mut World, guide: Entity, frame: FrameIndex, space: Option<Entity>) -> Option<Entity> {
    let anchor = anchor_in(world, guide, frame)?;
    let look = look_from_guide(world, guide, anchor)?;
    let guide_name = world.get::<Name>(guide).map_or("box".to_string(), |n| n.to_string());
    let name = format!("Tracker {}", tracker_count(world) + 1);
    let mut made = None;
    edit(world, &format!("Track {guide_name}"), |tx| made = Some(spawn_tracker(tx, name, guide, look, space, false)));
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
    // A re-seed starts the tracker from the look on that frame (a painted
    // one first), which moves first so it seeds. With no look there, the
    // Track tool asks for one: the tracker's own position there may be
    // wrong, so it never makes a look from it (it once seeded trackers on
    // background that way).
    let mut reseed: Vec<(Entity, FrameIndex, Entity)> = Vec::new();
    let mut show: Option<Entity> = None;
    let mut guides: Vec<(Entity, Look)> = Vec::new();
    for e in selected {
        if is_tracker(world, e) {
            let Some(a) = guide_of(world, e).and_then(|g| anchor_in(world, g, frame)) else { continue };
            let here: Vec<Entity> = look::looks_of(world, e).into_iter().filter(|l| world.get::<Look>(*l).is_some_and(|l| l.frame == a)).collect();
            match here.iter().find(|l| world.get::<Look>(**l).is_some_and(|l| l.painted().is_some())).or(here.first()) {
                // Already its anchor and first look: nothing to do.
                Some(l) if world.get::<Tracker>(e).is_some_and(|t| t.anchor == a) && look::looks_of(world, e).first() == Some(l) => {}
                Some(l) => reseed.push((e, a, *l)),
                None => show = Some(e),
            }
        } else if let Some(s) = tt_core::sketch::sketch_of(world, e)
            && !guides.iter().any(|(g, _)| *g == s)
            && let Some(look) = anchor_in(world, s, frame).and_then(|a| look_from_guide(world, s, a))
        {
            guides.push((s, look));
        }
    }
    let name_of = |world: &World, e: Entity| world.get::<Name>(e).map_or("box".to_string(), |n| n.to_string());
    if let Some(t) = show.filter(|_| reseed.is_empty() && guides.is_empty()) {
        world.resource_mut::<Selection>().select_only(t);
        world.resource_mut::<tt_core::tool::ActiveTool>().0 = tt_core::tool::Tool::Track;
        let mut tool = world.resource_mut::<tool::TrackTool>();
        (tool.reseed, tool.refused) = (Some(t), None);
        return;
    }
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
        for (e, a, l) in reseed {
            tx.modify::<Tracker>(e, |t| t.anchor = a);
            tx.modify::<Inputs>(e, |i| {
                let Some(from) = i.0.iter().position(|(s, p)| s == "look" && *p == l) else { return };
                let item = i.0.remove(from);
                let at = i.0.iter().position(|(s, _)| s == "look").unwrap_or(i.0.len());
                i.0.insert(at, item);
            });
        }
        for (i, (g, look)) in guides.into_iter().enumerate() {
            made.push(spawn_tracker(tx, format!("Tracker {}", first + i), g, look, space, false));
        }
    });
    if !made.is_empty() {
        world.resource_mut::<Selection>().entities = made;
    }
}

/// A look edited (moved, resized, its mask painted): its trackers re-track.
fn look_changed(changed: Query<Entity, Changed<Look>>, mut inv: ResMut<tt_core::op::Invalidations>, t: Res<Transport>) {
    for e in &changed {
        inv.output_changed(e, 0..t.frame_count);
    }
}

pub struct TrackModule;

impl Module for TrackModule {
    fn build(&self, app: &mut AppBuilder) {
        app.operator(TrackKind)
            .operator_params::<Tracker>()
            .component::<runner::TrackBook>(Class::Document)
            .component::<Look>(Class::Document)
            .component::<TrackRun>(Class::Document)
            .declare::<NewTrackers>(Class::Session)
            .init_resource::<NewTrackers>()
            .declare::<tool::TrackTool>(Class::Session)
            .init_resource::<tool::TrackTool>()
            .declare::<look::LookDefaults>(Class::Session)
            .init_resource::<look::LookDefaults>()
            .declare::<look::LookMasker>(Class::Session)
            .init_resource::<look::LookMasker>()
            .declare::<export::StabilizerDefaults>(Class::Session)
            .init_resource::<export::StabilizerDefaults>()
            .register_type::<Direction>()
            .register_type::<Rendition>()
            .register_type::<Matching>()
            .register_type::<Method>()
            .declare::<TrackStatus>(Class::Derived)
            .declare::<runner::TrackJobs>(Class::Derived)
            .declare::<Footage>(Class::Derived)
            .init_resource::<runner::TrackJobs>()
            .add_systems((apply_track_actions, look_changed).in_set(Set::Intents))
            .add_systems(tool::track_tool.in_set(Set::Tools))
            .add_systems(runner::run_trackers.in_set(Set::Jobs));
    }
}
