//! Automatic trackers (DESIGN §6, M6): operators that look at pixels.
//!
//! A tracker is a `track` operator entity:
//! - input `guide`: a box producer, normally a sketch. The rough pass does
//!   the hard part: it says roughly where the subject is on every frame, so
//!   the tracker only searches the guide's box and predicts from the guide's
//!   motion. Where the tracker loses the subject, it follows the guide.
//!   Without one, its guide is the whole frame (the root): it searches
//!   around where it was going.
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
//!   It is the tracker's automatic results with its human layer over them
//!   ([`human`]): where a person drew the point, that is the output.
//!
//! Trackers are *job* operators: evaluation leaves their dirty frames to
//! [`runner`], which runs them in background threads forward and backward
//! from the anchor, keeps old results on screen as stale until new ones
//! arrive, and can hold them to the playhead (catch-up mode). Which way
//! they track is the user's to ask ([`TrackRun`]): new trackers wait, and so
//! do CoTracker trackers when their project opens ([`pause_cotrackers_on_open`]).

pub mod export;
pub mod human;
pub mod image;
pub mod job;
pub mod look;
pub mod ncc;
pub mod off;
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
use tt_core::op::{EvalCtx, Footprint, Inputs, Invalidations, OpError, Operator, OperatorKind, Output};
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
/// Flag: the tracker is switched off on this frame (`off`): left out, as a lost frame is.
pub const OFF: u32 = 4;

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
    /// CoTracker3 (Meta's learned point tracker, CC-BY-NC weights): one
    /// pixel followed at a time, from its reset points (its looks, each a
    /// point exactly where it was put), run by a Python worker on the job's frames.
    CoTracker,
    /// No algorithm: only what a person draws (a manual dot, [`human`]).
    Manual,
    /// CoTracker3 on many points painted over the subject, and one motion
    /// made from them (`job::paint`): its looks are paints, the first where
    /// it starts, later ones its reset paints.
    Paint,
    /// TAPNext++ (Google DeepMind's online point tracker, Apache-2.0): one
    /// pixel followed, as a CoTracker, from its reset points; each frame's
    /// result as soon as that frame is in. Run by the same worker.
    TapNext,
}

impl Method {
    /// It runs on CoTracker's model (a stream through the shared worker).
    pub fn learned(self) -> bool {
        matches!(self, Method::CoTracker | Method::Paint | Method::TapNext)
    }

    /// It follows one pixel (its looks are reset points: a click places one).
    pub fn point(self) -> bool {
        matches!(self, Method::CoTracker | Method::TapNext)
    }

    /// What its looks are called: a template tracker's are patterns it
    /// matches ("look"), CoTracker's the pixel it follows from there on
    /// ("reset point").
    pub fn look_word(self) -> &'static str {
        match self {
            Method::CoTracker | Method::TapNext => "reset point",
            Method::Paint => "paint",
            _ => "look",
        }
    }
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

/// A tracker's run state: paused while [`PausedOnOpen`], else what it was
/// asked (one without any, from before: both ways). Everything that starts
/// jobs reads it here.
pub fn run_of(world: &World, tracker: Entity) -> TrackRun {
    if world.get::<PausedOnOpen>(tracker).is_some() {
        return TrackRun::Paused;
    }
    asked_run(world, tracker)
}

/// What a tracker was asked, as saved (paused on open or not).
fn asked_run(world: &World, tracker: Entity) -> TrackRun {
    world.get::<TrackRun>(tracker).copied().unwrap_or(TrackRun::Both)
}

/// Ask `tracker` to track one way, both, or pause. Like its results, not an
/// undo step (undoing an edit shouldn't stop tracking), but saved. Asked to
/// track after it stopped on an error, it plans again and tries again, also
/// when asked the way it already was. Either way it is no longer
/// [`PausedOnOpen`]: what it is asked now is saved (Pause too).
pub fn set_run(world: &mut World, tracker: Entity, run: TrackRun) {
    if !is_tracker(world, tracker) {
        return;
    }
    let paused_on_open = world.get::<PausedOnOpen>(tracker).is_some();
    if paused_on_open {
        world.entity_mut(tracker).remove::<PausedOnOpen>();
    }
    // (As an input edit would: a failed job's tracker otherwise waits for its inputs to change.)
    if run != TrackRun::Paused && world.get::<OpError>(tracker).is_some() {
        let frames = tt_core::op::extent(world);
        world.resource_mut::<Invalidations>().recompute(tracker, frames);
    }
    if paused_on_open || asked_run(world, tracker) != run {
        world.entity_mut(tracker).insert(run);
        world.resource_mut::<History>().touch();
    }
}

/// A CoTracker tracker paused because its project opened
/// ([`pause_cotrackers_on_open`]), and what it was asked (still its saved
/// [`TrackRun`]): it counts as paused ([`run_of`]). Never saved, so the
/// project keeps what it was asked, and the next open pauses it again;
/// gone once it is asked again ([`set_run`]).
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct PausedOnOpen(pub TrackRun);

/// Opening a project never starts CoTracker by itself (each job starts a
/// Python worker that loads its model onto the graphics card): every
/// CoTracker tracker asked to track is held paused by a [`PausedOnOpen`],
/// which keeps how it was asked. Its saved [`TrackRun`] stays: not an undo
/// step, not a change to save, and later saves keep what it was asked (call
/// it right after loading). Returns the trackers paused, in creation order.
pub fn pause_cotrackers_on_open(world: &mut World) -> Vec<Entity> {
    let mut q = world.query::<(Entity, &Tracker, Option<&TrackRun>)>();
    let mut asked: Vec<Entity> = q
        .iter(world)
        .filter(|(_, t, run)| t.method.learned() && run.copied().unwrap_or(TrackRun::Both) != TrackRun::Paused)
        .map(|(e, _, _)| e)
        .collect();
    tt_core::meta::creation_order(world, &mut asked);
    for e in &asked {
        let was = asked_run(world, *e);
        world.entity_mut(*e).insert(PausedOnOpen(was));
    }
    asked
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
/// into the guide's frames (with no guide, the video's). None if the guide
/// isn't a box with frames.
fn anchor_in(world: &World, guide: Option<Entity>, frame: FrameIndex) -> Option<FrameIndex> {
    let Some(guide) = guide else {
        let n = world.resource::<Transport>().frame_count;
        return (n > 0).then(|| frame.clamp(0, n - 1));
    };
    let sig = world.resource::<SignalStore>().get(world.get::<Output>(guide)?.0)?;
    if sig.channels() < 6 {
        return None;
    }
    let (lo, hi) = sig.present_hull()?;
    Some(frame.clamp(lo, hi))
}

/// Its `n`th look's name: "Look 2", or a CoTracker's "Reset point 2".
fn look_name(method: Method, n: usize) -> String {
    let word = method.look_word();
    format!("{}{} {n}", word[..1].to_uppercase(), &word[1..])
}

/// A tracker entity following `guide` (None: the whole frame) from `look`
/// (its seed) in `space`, as part of an edit. `placed`: the user put the
/// look there (else it came from the guide's point, so the finished path is
/// re-centred on the guide).
fn spawn_tracker(tx: &mut Tx<'_>, name: String, guide: Option<Entity>, look: Look, space: Option<Entity>, placed: bool) -> Entity {
    let out = tx.create_signal(TRACK_CHANNELS);
    let auto = tx.create_signal(TRACK_CHANNELS);
    let anchor = look.frame;
    let new = tx.world().get_resource::<NewTrackers>().copied().unwrap_or_default();
    let look = tx.spawn((Name::new(look_name(new.method, 1)), look));
    let mut inputs: Vec<(String, Entity)> = guide.map(|g| ("guide".to_string(), g)).into_iter().collect();
    inputs.extend(space.map(|v| ("space".to_string(), v)));
    inputs.push(("look".to_string(), look));
    let tracker = Tracker { center_on_guide: !placed && guide.is_some(), method: new.method, ..Tracker::at(anchor) };
    tx.spawn((Name::new(name), Operator { kind: "track".into() }, Inputs(inputs), Output(out), human::AutoOutput(auto), tracker, runner::TrackBook::default(), new.run))
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
    add_tracker_in(world, Some(guide), look)
}

/// Add a tracker with no guide (the whole frame is its search region)
/// whose first look is `look`, tracking in the source. One undo step; the
/// tracker is selected.
pub fn add_unguided_tracker(world: &mut World, look: Look) -> Option<Entity> {
    add_tracker_in(world, None, look)
}

fn add_tracker_in(world: &mut World, guide: Option<Entity>, look: Look) -> Option<Entity> {
    if anchor_in(world, guide, look.frame)? != look.frame {
        return None;
    }
    let guide_name = guide.map_or("the whole frame".to_string(), |g| world.get::<Name>(g).map_or("box".to_string(), |n| n.to_string()));
    let name = format!("Tracker {}", tracker_count(world) + 1);
    world.resource_mut::<History>().begin(format!("Track {guide_name}"));
    let space = guide.and_then(|g| tracking_view(world, g));
    let mut made = None;
    edit(world, "Track", |tx| made = Some(spawn_tracker(tx, name, guide, look, space, true)));
    world.resource_mut::<History>().end();
    let tracker = made?;
    world.resource_mut::<Selection>().select_only(tracker);
    Some(tracker)
}

fn method_of(world: &World, tracker: Entity) -> Method {
    world.get::<Tracker>(tracker).map_or(Method::Template, |t| t.method)
}

/// Start `tracker` again from `look` (one undo step): the look goes first
/// (it seeds) and its frame becomes the anchor.
pub fn reseed_with_look(world: &mut World, tracker: Entity, look: Look) -> Option<Entity> {
    let n = look::looks_of(world, tracker).len() + 1;
    let name = look_name(method_of(world, tracker), n);
    let frame = look.frame;
    let mut made = None;
    edit(world, "Re-seed tracker", |tx| {
        let e = tx.spawn((Name::new(name), look));
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
    let method = method_of(world, tracker);
    let name = look_name(method, n);
    let mut made = None;
    edit(world, &format!("Add {}", method.look_word()), |tx| {
        let e = tx.spawn((Name::new(name), look));
        tx.modify::<Inputs>(tracker, |i| i.0.push(("look".to_string(), e)));
        made = Some(e);
    });
    made
}

/// Whether looks of a `from` tracker can go to a `to` tracker: the same kind
/// of look (a paint to a paint tracker, a reset point to a point tracker, a
/// pattern to a template tracker).
pub fn looks_fit(from: Method, to: Method) -> bool {
    let kind = |m: Method| match m {
        Method::Paint => 0,
        m if m.point() => 1,
        Method::Template => 2,
        Method::Manual => 3,
        _ => 4,
    };
    kind(from) == kind(to) && to != Method::Manual
}

/// What moving `looks` to `tracker` would do: the looks that can go (live,
/// of another tracker of the same kind), and the trackers they leave with
/// no looks at all (they go too: a tracker made by mistake).
pub fn looks_to_move(world: &World, looks: &[Entity], tracker: Entity) -> (Vec<(Entity, Entity)>, Vec<Entity>) {
    let Some(to) = world.get::<Tracker>(tracker).map(|t| t.method) else { return (Vec::new(), Vec::new()) };
    let moving: Vec<(Entity, Entity)> = looks
        .iter()
        .filter(|l| world.get::<Look>(**l).is_some() && world.get_entity(**l).is_ok_and(|r| !r.contains::<Disabled>()))
        .filter_map(|l| Some((*l, look::owner_of(world, *l)?)))
        .filter(|(_, owner)| *owner != tracker && world.get::<Tracker>(*owner).is_some_and(|t| looks_fit(t.method, to)))
        .collect();
    let mut emptied: Vec<Entity> = moving.iter().map(|(_, o)| *o).collect();
    emptied.sort();
    emptied.dedup();
    emptied.retain(|o| look::looks_of(world, *o).iter().all(|l| moving.iter().any(|(m, _)| m == l)));
    (moving, emptied)
}

/// Move `looks` (paints, reset points, patterns) to `tracker`, one undo step
/// (on request: "I accidentally make new paintings under a brand new tracker
/// when I mean to have another tracker use those paints … box select the
/// paint keyframes and drag them onto another tracker"): they leave their
/// trackers and become `tracker`'s; a tracker left with no looks goes (with
/// its view). Returns how many moved and how many trackers went.
pub fn move_looks(world: &mut World, looks: &[Entity], tracker: Entity) -> (usize, usize) {
    let (moving, emptied) = looks_to_move(world, looks, tracker);
    if moving.is_empty() {
        return (0, 0);
    }
    let word = method_of(world, tracker).look_word();
    let name = world.get::<Name>(tracker).map_or("the tracker".to_string(), |n| n.to_string());
    let label = match moving.len() {
        1 => format!("Move {word} to {name}"),
        n => format!("Move {n} {word}s to {name}"),
    };
    let mut doomed = emptied.clone();
    for t in &emptied {
        doomed.extend(tt_core::view::view_of(world, *t));
    }
    edit(world, &label, |tx| {
        for (l, owner) in &moving {
            tx.modify::<Inputs>(*owner, |i| i.0.retain(|(s, p)| !(s == "look" && p == l)));
            tx.modify::<Inputs>(tracker, |i| i.0.push(("look".to_string(), *l)));
        }
        for e in &doomed {
            tx.delete(*e);
        }
    });
    (moving.len(), emptied.len())
}

/// A CoTracker's reset point on `look`'s frame (one undo step): it follows
/// one pixel at a time, so a reset point already on that frame moves there;
/// else a new one. Returns it.
pub fn set_reset_point(world: &mut World, tracker: Entity, look: Look) -> Option<Entity> {
    let here = look::looks_of(world, tracker).into_iter().find(|l| world.get::<Look>(*l).is_some_and(|l| l.frame == look.frame));
    match here {
        Some(l) => {
            edit(world, "Move reset point", |tx| {
                tx.modify::<Look>(l, |old| {
                    (old.x, old.y) = (look.x, look.y);
                })
            });
            Some(l)
        }
        None => add_look(world, tracker, look),
    }
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
    let anchor = anchor_in(world, Some(guide), frame)?;
    let look = look_from_guide(world, guide, anchor)?;
    let guide_name = world.get::<Name>(guide).map_or("box".to_string(), |n| n.to_string());
    let name = format!("Tracker {}", tracker_count(world) + 1);
    let mut made = None;
    edit(world, &format!("Track {guide_name}"), |tx| made = Some(spawn_tracker(tx, name, Some(guide), look, space, false)));
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
            // (A manual dot has nothing to re-seed: it is only what was drawn.)
            if method_of(world, e) == Method::Manual {
                continue;
            }
            let Some(a) = anchor_in(world, guide_of(world, e), frame) else { continue };
            let here: Vec<Entity> = look::looks_of(world, e).into_iter().filter(|l| world.get::<Look>(*l).is_some_and(|l| l.frame == a)).collect();
            match here.iter().find(|l| world.get::<Look>(**l).is_some_and(|l| l.painted().is_some())).or(here.first()) {
                // Already its anchor and first look: nothing to do.
                Some(l) if world.get::<Tracker>(e).is_some_and(|t| t.anchor == a) && look::looks_of(world, e).first() == Some(l) => {}
                Some(l) => reseed.push((e, a, *l)),
                None => show = Some(e),
            }
        } else if let Some(s) = tt_core::sketch::sketch_of(world, e)
            && !guides.iter().any(|(g, _)| *g == s)
            && let Some(look) = anchor_in(world, Some(s), frame).and_then(|a| look_from_guide(world, s, a))
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
            made.push(spawn_tracker(tx, format!("Tracker {}", first + i), Some(g), look, space, false));
        }
    });
    if !made.is_empty() {
        world.resource_mut::<Selection>().entities = made;
    }
}

/// Track `trackers` one way, both or pause them (`run`), from frame `f`:
/// a tracker with a look (a reset point, a paint) on `f` starts again from
/// it there (as *Re-seed here*), one undo step for them all; the others go
/// on from where they are (their anchor and results). Manual dots are left
/// alone. Returns how many were asked, and how many started again at `f`.
pub fn track_from(world: &mut World, trackers: &[Entity], f: FrameIndex, run: TrackRun) -> (usize, usize) {
    let trackers: Vec<Entity> = trackers.iter().copied().filter(|e| is_tracker(world, *e) && method_of(world, *e) != Method::Manual).collect();
    let mut reseed: Vec<(Entity, Entity)> = Vec::new();
    if run != TrackRun::Paused {
        for t in &trackers {
            let here: Vec<Entity> = look::looks_of(world, *t).into_iter().filter(|l| world.get::<Look>(*l).is_some_and(|l| l.frame == f)).collect();
            let Some(l) = here.iter().find(|l| world.get::<Look>(**l).is_some_and(|l| l.painted().is_some() || !l.mask.is_empty())).or(here.first()).copied() else { continue };
            // (Already its anchor and first look: nothing to start again.)
            if world.get::<Tracker>(*t).is_some_and(|p| p.anchor == f) && look::looks_of(world, *t).first() == Some(&l) {
                continue;
            }
            reseed.push((*t, l));
        }
    }
    if !reseed.is_empty() {
        edit(world, "Re-seed trackers", |tx| {
            for (e, l) in &reseed {
                tx.modify::<Tracker>(*e, |t| t.anchor = f);
                tx.modify::<Inputs>(*e, |i| {
                    let Some(from) = i.0.iter().position(|(s, p)| s == "look" && p == l) else { return };
                    let item = i.0.remove(from);
                    let at = i.0.iter().position(|(s, _)| s == "look").unwrap_or(i.0.len());
                    i.0.insert(at, item);
                });
            }
        });
    }
    for t in &trackers {
        set_run(world, *t, run);
    }
    (trackers.len(), reseed.len())
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
            .component::<human::AutoOutput>(Class::Document)
            .component::<human::HumanLayer>(Class::Document)
            .component::<off::TrackerOff>(Class::Document)
            .declare::<runner::PaintPoints>(Class::Derived)
            .declare::<human::Composed>(Class::Derived)
            .declare::<PausedOnOpen>(Class::Derived)
            .declare::<human::DrawTool>(Class::Derived)
            .init_resource::<human::DrawTool>()
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
            .add_systems((apply_track_actions, look_changed, off::switch_actions).in_set(Set::Intents))
            .add_systems((tool::track_tool, human::draw_tool).in_set(Set::Tools))
            .add_systems((runner::run_trackers, human::compose_trackers).chain().in_set(Set::Jobs));
    }
}
