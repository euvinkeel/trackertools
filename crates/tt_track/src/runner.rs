//! The tracker runner (Set::Jobs): turns trackers' dirty frames into
//! background jobs, and their results into signal writes.
//!
//! - A tracker waits until its inputs (guide, view) are evaluated and no drag
//!   is in progress, then snapshots them into a [`Plan`]: the guide's boxes
//!   and the view's mapping per frame, the anchor, the rendition, the settings.
//! - Dirt only says "look again". What to re-track comes from comparing the
//!   plan with the one the results came from (the tracker's *basis*): walking
//!   out from the anchor on each side, results hold up to the first frame
//!   whose guide box or view map differs. A guide edit on frames 700..760
//!   re-tracks from ~700 on, even though a sketch reports its whole extent as
//!   changed; a recompute that changes nothing re-tracks nothing.
//! - Each side of the anchor with frames left to do gets a job, resuming from
//!   the result just before them, if the tracker is asked to track that way
//!   ([`crate::TrackRun`]: forward, backward, both, or paused). Pausing or
//!   turning stops the jobs it no longer asks for; their results stay. A running job stays if its inputs didn't
//!   change and it covers exactly what is left on its side; otherwise it is
//!   cancelled and replaced.
//! - At most [`MAX_JOBS`] jobs work at once, and one CoTracker job with a
//!   worker ([`cotracker_jobs`]): the others wait their turn, in creation
//!   order, forward before backward.
//! - A failed job stops its tracker (an [`OpError`]) until its inputs change
//!   or it is asked to track again (`crate::set_run`). A job thread that
//!   ends without saying how counts as failed.
//! - Results land as they arrive; old ones stay on screen as stale until
//!   replaced (stale-while-revalidate), and dependents update chunk by chunk.
//! - A [`TrackBook`] (saved) stamps the inputs complete results came from, so
//!   a reopened project keeps them instead of tracking again. The stamp is 0
//!   while results are incomplete or mixed. Results arriving are a change to
//!   save at most every [`RESULTS_SAVED_EVERY`] s, so autosave keeps a
//!   running tracker's results too.
//! - A tracker's span (`tt_core::span::Span`, its lifetime on the timeline)
//!   says where its jobs stop: they still start at the anchor (the path
//!   depends on where it began), but track nothing past the span's edges.
//!   Results outside the span stay (hidden from consumers), so extending the
//!   span brings back the frames it had without tracking them again.
//! - When the results stop coming in (finished, or parked at the playhead),
//!   the path is shifted onto the guide's: by the median offset between the
//!   two over the frames it saw the subject. The tracker gives the motion's
//!   shape; the rough pass, on average, where the subject is (instead of
//!   wherever the guide happened to be at the anchor).
//! - Results go into the tracker's automatic layer (`human::AutoOutput`); its
//!   output is that with its human layer over it (`human::compose`, run
//!   right after). A manual dot has no algorithm: nothing is planned for it.
//! - A tracker with no guide searches the whole frame: its plan's guide is
//!   the frame itself on every frame of the video.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::name::Name;
use bevy_ecs::prelude::*;
use bevy_reflect::Reflect;
use tt_core::history::History;
use tt_core::op::{Dirty, Inputs, Invalidations, OpError, Operator, Output, extent};
use tt_core::ranges::RangeSet;
use tt_core::signal::{FrameState, SignalId, SignalStore};
use tt_core::time::{FrameIndex, WallClock};
use tt_core::transport::Transport;
use tt_core::view::{SpaceMap, home_of, map_at};
use tt_media::{DecodeOptions, VideoIndex};

use crate::human::{AutoOutput, ensure_auto, is_manual};
use crate::job::{JobSpec, LookSpec, Msg, Phase, Shared, Side, spawn};
use crate::look::Look;
use crate::template::{LOOK_PX, Settings, TEMPLATE_R};
use crate::{Direction, Method, Rendition, TRACK_CHANNELS, TrackRun, Tracker, guide_of, run_of};

/// Part of every stamp: bump it when a change to the tracking makes saved
/// results out of date (they re-track when their project is opened).
const ALGO_VERSION: u64 = 3;

/// The video trackers read (set by the host when media opens and when its proxy is ready).
#[derive(Resource, Clone)]
pub struct Footage {
    pub original: Arc<VideoIndex>,
    pub proxy: Option<Arc<VideoIndex>>,
    pub decode: DecodeOptions,
}

/// A tracker's bookkeeping, saved with its results: the hash of the inputs
/// complete results came from (0 = incomplete), and the shift applied to them
/// (re-centring on the guide).
#[derive(Component, Reflect, Clone, Copy, Debug, Default, PartialEq)]
#[reflect(Component)]
pub struct TrackBook {
    pub stamp: u64,
    pub offset: [f32; 2],
}

/// What a tracker is doing, for panels.
#[derive(Component, Clone, Debug, Default)]
pub struct TrackStatus {
    pub forward: Option<SideStatus>,
    pub backward: Option<SideStatus>,
    /// The rendition being read ("original 1920×1080", "proxy 1280×720").
    pub rendition: String,
    /// The frame it tracks from (its anchor, moved into the guide's frames).
    pub anchor: FrameIndex,
    /// Asked to track, with frames left, but no job yet: waiting for a free
    /// slot (at most [`MAX_JOBS`] at once) or for its inputs to settle.
    pub queued: bool,
    /// Queued because another CoTracker job runs (one at a time, [`cotracker_jobs`]).
    pub waits_for_cotracker: bool,
    /// Catch-up mode: frames left to track, but past the playhead, and the
    /// CoTracker worker is taken (by a job waiting at the playhead too). It
    /// starts when the playhead gets there. Not queued: nothing to do now.
    pub waits_at_playhead: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct SideStatus {
    /// Where the job started, the last frame produced, and where it ends.
    pub from: FrameIndex,
    pub at: FrameIndex,
    pub to: FrameIndex,
    /// Frames per second so far.
    pub fps: f64,
    /// Holding at the playhead (catch-up mode).
    pub waiting: bool,
    /// Starting, loading its model (CoTracker's Python worker), or tracking.
    pub phase: Phase,
}

impl TrackStatus {
    pub fn busy(&self) -> bool {
        self.forward.is_some() || self.backward.is_some()
    }

    /// A job starts up: decoding its first frames, or CoTracker's worker
    /// loading its model (`Some(true)`: the model).
    pub fn starting(&self) -> Option<bool> {
        let sides = [self.forward, self.backward];
        let mut s = sides.iter().flatten();
        if s.clone().any(|s| s.phase == Phase::Loading) {
            Some(true)
        } else if s.any(|s| s.phase == Phase::Starting) {
            Some(false)
        } else {
            None
        }
    }
}

/// Worker threads decoding at once, across trackers (each runs its own
/// ffmpeg); more jobs wait their turn. Cancelled threads count until they
/// stop; jobs parked at the playhead don't.
pub const MAX_JOBS: usize = 4;

/// Results arriving mark the document changed at most this often (seconds),
/// so autosave keeps a running tracker's results without being put off by
/// every chunk (it waits for 1.5 s without changes).
pub const RESULTS_SAVED_EVERY: f64 = 5.0;

/// CoTracker jobs tracking at once, across trackers: 3, or
/// `TT_COTRACKER_JOBS` (1 to 4). They are streams through one shared worker
/// (one Python process, the model on the graphics card once: several
/// processes at once made the card reset), which runs their windows in turn
/// (job::learned). They count from start until their thread ends, parked and
/// cancelled ones included (they hold their stream until then); they count
/// toward [`MAX_JOBS`] too. A tracker's two sides run one after the other.
pub fn cotracker_jobs() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        let asked = std::env::var("TT_COTRACKER_JOBS").ok();
        let n = asked.as_deref().and_then(|v| v.trim().parse::<usize>().ok()).map_or(3, |n| n.clamp(1, 4));
        if let Some(v) = asked {
            tracing::info!("TT_COTRACKER_JOBS={v}: {n} CoTracker job(s) at once");
        }
        n
    })
}

#[derive(Resource, Default)]
pub struct TrackJobs {
    running: HashMap<(Entity, Side), Running>,
    /// What each live tracker's results came from.
    basis: HashMap<Entity, Basis>,
    /// Worker threads alive, cancelled ones included.
    threads: Arc<AtomicUsize>,
    /// Of those, the ones running a CoTracker worker.
    workers: Arc<AtomicUsize>,
    /// The video of the document the jobs belong to.
    video: Option<Arc<VideoIndex>>,
    /// A tracker has work it hasn't started (inputs still settling, a drag
    /// in progress, no free slot).
    pending: bool,
    /// A CoTracker job that could track now waited for a worker, this pass
    /// and the one before: jobs parked at the playhead let theirs go, and
    /// jobs that could only wait there don't take one.
    worker_wanted: bool,
    worker_wanted_before: bool,
    /// The same, waiting for a free slot ([`MAX_JOBS`]) instead: jobs that
    /// could only wait at the playhead don't take the worker either.
    worker_queued: bool,
    worker_queued_before: bool,
    /// When drained results last marked the document changed (`WallClock`
    /// seconds): at most every [`RESULTS_SAVED_EVERY`].
    results_touched: Option<f64>,
}

impl TrackJobs {
    /// Jobs running now (parked ones included).
    pub fn busy(&self) -> usize {
        self.running.len()
    }

    /// Worker threads alive, including cancelled ones still winding down.
    pub fn threads(&self) -> usize {
        self.threads.load(Ordering::Relaxed)
    }

    /// CoTracker workers alive: job threads running one, parked and
    /// cancelled ones included until they end (at most [`cotracker_jobs`]).
    pub fn cotracker_workers(&self) -> usize {
        self.workers.load(Ordering::Relaxed)
    }

    /// Whether results are on their way or work is waiting to start: the
    /// host should keep running frames (jobs waiting at the playhead aren't).
    pub fn active(&self) -> bool {
        self.pending || self.running.values().any(|j| !j.shared.waiting.load(Ordering::Relaxed))
    }

    /// Threads holding a decoder.
    fn working(&self) -> usize {
        let parked = self.running.values().filter(|j| j.shared.parked.load(Ordering::Relaxed)).count();
        self.threads().saturating_sub(parked)
    }
}

struct Running {
    shared: Arc<Shared>,
    /// (In a mutex only because resources must be `Sync`.)
    rx: Mutex<Receiver<Msg>>,
    /// It runs a CoTracker worker.
    worker: bool,
    /// Frames this job will produce and hasn't yet.
    owned: RangeSet,
    from: FrameIndex,
    to: FrameIndex,
    started: Instant,
    produced: u64,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
    }
}

/// What a tracker's results came from. Every running job of the tracker
/// works for this plan.
struct Basis {
    plan: Arc<Plan>,
    /// Frames whose results came from `plan`'s inputs.
    done: RangeSet,
    /// A job failed: nothing restarts until the inputs change (or it is
    /// asked to track again: `set_run` plans again).
    failed: bool,
    /// Results changed since the last re-centring.
    unsettled: bool,
}

/// Inputs snapshotted for jobs.
struct Plan {
    lo: FrameIndex,
    hi: FrameIndex,
    /// The tracker's span: jobs track nothing outside it (but for the lead-in from the anchor).
    span: Range<FrameIndex>,
    /// `params.anchor` moved into the guide's frames.
    anchor: FrameIndex,
    params: Tracker,
    guide: Arc<Vec<[f64; 6]>>,
    maps: Arc<Vec<SpaceMap>>,
    scale: f64,
    video: Arc<VideoIndex>,
    k: [f64; 2],
    stamp: u64,
    /// The looks (in the tracked range), and the seed: the look on the anchor frame's centre, source px.
    looks: Arc<Vec<LookSpec>>,
    seed: Option<[f64; 2]>,
    /// No guide: `guide` is the whole frame, and the search follows the tracker.
    root: bool,
}

impl Plan {
    /// Frames a side may produce, before the span: forward from the anchor
    /// (the anchor itself included), backward the frames before it.
    fn reach(&self, side: Side) -> Range<FrameIndex> {
        match (side, self.params.direction) {
            (Side::Forward, Direction::Backward) => self.anchor..self.anchor + 1,
            (Side::Forward, _) => self.anchor..self.hi + 1,
            (Side::Backward, Direction::Forward) => self.anchor..self.anchor,
            (Side::Backward, _) => self.lo..self.anchor,
        }
    }

    /// Frames a side's jobs produce: its reach, up to the span's far edge.
    fn side(&self, side: Side) -> Range<FrameIndex> {
        let r = self.reach(side);
        match side {
            Side::Forward => r.start..r.end.min(self.span.end).max(r.start + 1).min(r.end),
            Side::Backward => r.start.max(self.span.start).min(r.end)..r.end,
        }
    }

    /// The frames of a side `run` asks for: all of it if it tracks that way;
    /// tracking only backward, the forward side's first frame (the anchor)
    /// too, so the path has its start; else none.
    fn wanted(&self, side: Side, run: TrackRun) -> Range<FrameIndex> {
        let r = self.side(side);
        let on = if side == Side::Forward { run.forward() } else { run.backward() };
        if on {
            r
        } else if side == Side::Forward && run.backward() {
            r.start..(r.start + 1).min(r.end)
        } else {
            r.start..r.start
        }
    }

    /// Frames the tracker produces (what its results are complete over).
    fn produced(&self) -> Range<FrameIndex> {
        self.side(Side::Backward).start..self.side(Side::Forward).end
    }

    /// Frames it may have results on, span or not: anything else is cleared.
    fn reachable(&self) -> Range<FrameIndex> {
        self.reach(Side::Backward).start..self.reach(Side::Forward).end
    }

    /// The guide's box and the view's mapping on frame `f`.
    fn inputs(&self, f: FrameIndex) -> Option<(&[f64; 6], &SpaceMap)> {
        let i = usize::try_from(f - self.lo).ok()?;
        Some((self.guide.get(i)?, self.maps.get(i)?))
    }

    /// Frames whose results from `old` still hold for this plan: with the
    /// same seed and settings, a frame depends on the inputs from the anchor
    /// out to it, so results hold out to the first frame whose inputs differ
    /// on each side.
    fn kept(&self, old: &Plan) -> Range<FrameIndex> {
        let (p, q) = (&self.params, &old.params);
        let same_seed = self.anchor == old.anchor
            && close(self.scale, old.scale)
            && Arc::ptr_eq(&self.video, &old.video)
            && (p.feature, p.search, p.adapt, p.min_score, p.fuse) == (q.feature, q.search, q.adapt, q.min_score, q.fuse)
            && p.matching == q.matching
            && p.method == q.method
            && self.root == old.root
            && self.looks == old.looks
            && self.seed == old.seed;
        let same = |f: FrameIndex| match (self.inputs(f), old.inputs(f)) {
            (Some((g, m)), Some((h, n))) => g.iter().zip(h).all(|(a, b)| close(*a, *b)) && close(m.a, n.a) && close(m.b[0], n.b[0]) && close(m.b[1], n.b[1]),
            _ => false,
        };
        if !same_seed || !same(self.anchor) {
            return self.anchor..self.anchor;
        }
        let mut end = self.anchor + 1;
        while end <= self.hi && same(end) {
            end += 1;
        }
        let mut start = self.anchor;
        while start > self.lo && same(start - 1) {
            start -= 1;
        }
        start..end
    }
}

/// Equal but for rounding (a recompute that changed nothing).
fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-6 * (1.0 + a.abs().max(b.abs()))
}

fn intersection(a: &RangeSet, b: &RangeSet) -> RangeSet {
    let mut out = RangeSet::new();
    for r in b.ranges() {
        out.union(&a.intersect(r));
    }
    out
}

fn difference(a: &RangeSet, b: &RangeSet) -> RangeSet {
    let mut out = a.clone();
    for r in b.ranges() {
        out.remove(r.clone());
    }
    out
}

pub fn run_trackers(world: &mut World) {
    let live: HashSet<Entity> = {
        let mut q = world.query_filtered::<(Entity, &Operator), Without<Disabled>>();
        q.iter(world).filter(|(_, o)| o.kind == "track").map(|(e, _)| e).collect()
    };
    // Deleted trackers stop (dropping a job cancels it) and forget their basis:
    // brought back, they plan afresh (the stamp may still hold).
    {
        let mut jobs = world.resource_mut::<TrackJobs>();
        jobs.running.retain(|(e, _), _| live.contains(e));
        jobs.basis.retain(|e, _| live.contains(e));
        jobs.pending = false;
        jobs.worker_wanted_before = std::mem::take(&mut jobs.worker_wanted);
        jobs.worker_queued_before = std::mem::take(&mut jobs.worker_queued);
    }
    let Some(footage) = world.get_resource::<Footage>().cloned() else { return };
    // Another video: its document replaces this one later this frame
    // (Set::Prepare). Nothing starts on the wrong video or touches the
    // outgoing results before they are saved; next frame starts over (a
    // tracker still there plans afresh: saved results may still hold).
    if !world.resource::<TrackJobs>().video.as_ref().is_some_and(|v| Arc::ptr_eq(v, &footage.original)) {
        let mut jobs = world.resource_mut::<TrackJobs>();
        jobs.running.clear();
        jobs.basis.clear();
        jobs.video = Some(footage.original.clone());
        jobs.pending = !live.is_empty();
        let extent = extent(world);
        let mut inv = world.resource_mut::<Invalidations>();
        for op in live {
            inv.recompute(op, extent.clone());
        }
        return;
    }
    let mut live: Vec<Entity> = live.into_iter().collect();
    tt_core::meta::creation_order(world, &mut live);
    for op in live {
        // (A tracker saved before the human layer: its output was its results.)
        ensure_auto(world, op);
        if is_manual(world, op) {
            // No algorithm: nothing to plan or run, and nothing to wait for.
            let mut jobs = world.resource_mut::<TrackJobs>();
            jobs.running.retain(|(o, _), _| *o != op);
            jobs.basis.remove(&op);
            if world.get::<Dirty>(op).is_some_and(|d| !d.0.is_empty()) {
                world.get_mut::<Dirty>(op).expect("checked").0 = RangeSet::new();
            }
            if world.get::<OpError>(op).is_some() {
                world.entity_mut(op).remove::<OpError>();
            }
            if world.get::<TrackStatus>(op).is_some() {
                world.entity_mut(op).remove::<TrackStatus>();
            }
            continue;
        }
        drain(world, op);
        replan(world, op, &footage);
        stop_unwanted(world, op);
        let waiting = start_jobs(world, op, &footage);
        settle(world, op);
        update_status(world, op, waiting);
        let dirty = world.get::<Dirty>(op).is_some_and(|d| !d.0.is_empty());
        // (Work that can only wait at the playhead keeps nothing busy: moving the playhead runs a frame anyway.)
        world.resource_mut::<TrackJobs>().pending |= matches!(waiting, Waiting::Yes | Waiting::ForCoTracker) || dirty;
    }
    // A CoTracker job that could track waits for a worker: jobs parked at the
    // playhead (catch-up mode), and still held there, let theirs go. Their
    // results stay; they go on from there when they can track again and a
    // worker is free.
    let mut jobs = world.resource_mut::<TrackJobs>();
    if jobs.worker_wanted {
        jobs.running.retain(|(_, side), j| !(j.worker && j.shared.held(*side)));
    }
}

/// Write finished frames into the tracker's automatic layer (its output
/// follows when it is composed).
fn drain(world: &mut World, op: Entity) {
    let mut msgs: Vec<(Side, Msg)> = Vec::new();
    {
        let jobs = world.resource::<TrackJobs>();
        for side in [Side::Forward, Side::Backward] {
            let Some(job) = jobs.running.get(&(op, side)) else { continue };
            let rx = job.rx.lock().expect("not poisoned");
            loop {
                match rx.try_recv() {
                    Ok(m) => {
                        let last = matches!(m, Msg::Finished | Msg::Failed(_));
                        msgs.push((side, m));
                        if last {
                            break;
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    // (Its thread ended without saying how: it failed.)
                    Err(TryRecvError::Disconnected) => {
                        msgs.push((side, Msg::Failed("the tracker stopped on an error (its job ended without a result)".into())));
                        break;
                    }
                }
            }
        }
    }
    if msgs.is_empty() {
        return;
    }
    let Some(out) = world.get::<AutoOutput>(op).map(|o| o.0) else { return };
    let [ox, oy] = world.get::<TrackBook>(op).map_or([0.0; 2], |b| b.offset);
    // Frames whose inputs changed since the job started (the new plan waits
    // for a drag to end): their new values are already out of date.
    let outdated = world.get::<Dirty>(op).map(|d| d.0.clone()).unwrap_or_default();
    for (side, msg) in msgs {
        match msg {
            Msg::Frames(frames) => {
                if let Some(sig) = world.resource_mut::<SignalStore>().get_mut(out) {
                    for (f, v) in &frames {
                        let [x, y, l, t, r, b, s, flags] = *v;
                        sig.set(*f, &[x + ox, y + oy, l + ox, t + oy, r + ox, b + oy, s, flags]);
                        if outdated.contains(*f) {
                            sig.mark_stale(*f..*f + 1);
                        }
                    }
                }
                let mut jobs = world.resource_mut::<TrackJobs>();
                let jobs = &mut *jobs;
                if let Some(job) = jobs.running.get_mut(&(op, side)) {
                    job.produced += frames.len() as u64;
                    for (f, _) in &frames {
                        job.owned.remove(*f..*f + 1);
                    }
                }
                if let Some(basis) = jobs.basis.get_mut(&op) {
                    for (f, _) in &frames {
                        basis.done.insert(*f..*f + 1);
                    }
                    basis.unsettled = true;
                }
                // Results are part of the document: now and then, a change to save.
                let now = world.get_resource::<WallClock>().map_or(0.0, |c| c.now);
                let mut jobs = world.resource_mut::<TrackJobs>();
                if jobs.results_touched.is_none_or(|t| now - t >= RESULTS_SAVED_EVERY || now < t) {
                    jobs.results_touched = Some(now);
                    world.resource_mut::<History>().touch();
                }
            }
            Msg::Finished => {
                world.resource_mut::<TrackJobs>().running.remove(&(op, side));
            }
            Msg::Failed(e) => {
                tracing::warn!("tracker {op} ({side:?}) failed: {e}");
                let mut jobs = world.resource_mut::<TrackJobs>();
                jobs.running.remove(&(op, side));
                if let Some(basis) = jobs.basis.get_mut(&op) {
                    basis.failed = true;
                }
                world.entity_mut(op).insert(OpError(e));
            }
        }
    }
}

/// New dirt: plan again once the inputs have settled, keep what still holds,
/// and stop the jobs that no longer fit.
fn replan(world: &mut World, op: Entity, footage: &Footage) {
    if world.get::<Dirty>(op).is_none_or(|d| d.0.is_empty()) {
        return;
    }
    // A drag (an inspector slider, say) plans once, when it ends.
    if world.resource::<History>().in_gesture() {
        return;
    }
    // Wait for the guide and the view to finish evaluating (a deleted one never will).
    let inputs: Vec<Entity> = world.get::<Inputs>(op).map(|i| i.0.iter().map(|(_, e)| *e).collect()).unwrap_or_default();
    if inputs.iter().any(|e| world.get::<Disabled>(*e).is_none() && world.get::<Dirty>(*e).is_some_and(|d| !d.0.is_empty())) {
        return;
    }
    world.get_mut::<Dirty>(op).expect("checked").0 = RangeSet::new();
    // Results from before the flags channel: start them afresh (the output too: it is composed from them).
    let signals: Vec<SignalId> = [world.get::<Output>(op).map(|o| o.0), world.get::<AutoOutput>(op).map(|o| o.0)].into_iter().flatten().collect();
    if signals.iter().any(|id| world.resource::<SignalStore>().get(*id).is_some_and(|s| s.channels() != TRACK_CHANNELS)) {
        for id in signals {
            world.resource_mut::<SignalStore>().insert(id, tt_core::signal::Signal::new(TRACK_CHANNELS));
        }
        world.resource_mut::<TrackJobs>().basis.remove(&op);
        world.entity_mut(op).remove::<crate::human::Composed>();
        let book = world.get::<TrackBook>(op).copied().unwrap_or_default();
        set_book(world, op, TrackBook { stamp: 0, ..book });
    }
    let Some(out) = world.get::<AutoOutput>(op).map(|o| o.0) else { return };
    let extent = extent(world);
    let old = world.resource_mut::<TrackJobs>().basis.remove(&op);
    let plan = match plan(world, op, footage, old.as_ref().map(|b| b.plan.as_ref())) {
        Ok(p) => Arc::new(p),
        Err(e) => {
            // Can't run: stop. The results stay on screen as stale, and the
            // basis stays, so they come back if the inputs do (an undone delete).
            let mut jobs = world.resource_mut::<TrackJobs>();
            jobs.running.retain(|(o, _), _| *o != op);
            if let Some(old) = old {
                jobs.basis.insert(op, old);
            }
            if let Some(sig) = world.resource_mut::<SignalStore>().get_mut(out) {
                sig.mark_stale(extent);
            }
            world.entity_mut(op).insert(OpError(e));
            return;
        }
    };
    world.entity_mut(op).remove::<OpError>();

    // What still holds: results of the basis whose inputs didn't change or,
    // with no basis (a reopened project), saved results whose stamp matches
    // (the stamp vouches for the span only: outside it they may be older).
    let (produced, reachable) = (plan.produced(), plan.reachable());
    let present: RangeSet = {
        let sig = world.resource::<SignalStore>().get(out);
        let mut set = RangeSet::new();
        for (r, _) in sig.map(|s| s.runs(reachable.clone())).unwrap_or_default() {
            set.insert(r);
        }
        set
    };
    let kept = old.as_ref().map_or(plan.anchor..plan.anchor, |b| plan.kept(&b.plan));
    let done = match &old {
        Some(b) => intersection(&b.done.intersect(&kept), &present),
        None if world.get::<TrackBook>(op).is_some_and(|b| b.stamp == plan.stamp) && present.intersect(&produced).len() == produced.end - produced.start => present.intersect(&produced),
        None => RangeSet::new(),
    };

    // A job stays if its inputs didn't change and what's left to do on its
    // side is exactly what it will produce (a new anchor, direction, setting
    // or edit on its frames replaces it).
    {
        let run = run_of(world, op);
        let mut jobs = world.resource_mut::<TrackJobs>();
        for side in [Side::Forward, Side::Backward] {
            let todo = difference(&RangeSet::from_range(plan.wanted(side, run)), &done);
            let stays = jobs.running.get(&(op, side)).is_some_and(|job| {
                job.owned == todo && job.owned.hull().is_none_or(|h| kept.start <= h.start && h.end <= kept.end)
            });
            if !stays {
                jobs.running.remove(&(op, side));
            }
        }
    }

    // The results: what holds is valid, the rest stale until replaced, and
    // nothing outside the frames the tracker can reach (its span only hides).
    if let Some(sig) = world.resource_mut::<SignalStore>().get_mut(out) {
        for r in [extent.start..reachable.start, reachable.end..extent.end] {
            if !r.is_empty() && !sig.runs(r.clone()).is_empty() {
                sig.clear(r);
            }
        }
        sig.mark_stale(reachable.clone());
        for r in done.ranges() {
            sig.mark_valid(r.clone());
        }
    }
    let reach = tt_core::span::Reach(reachable.start, reachable.end - 1);
    if world.get::<tt_core::span::Reach>(op) != Some(&reach) {
        world.entity_mut(op).insert(reach);
    }
    world.resource_mut::<TrackJobs>().basis.insert(op, Basis { plan, done, failed: false, unsettled: true });
}

/// Stop the jobs the tracker's run state no longer asks for (paused, or
/// turned the other way): dropping one cancels it; its results so far stay.
fn stop_unwanted(world: &mut World, op: Entity) {
    let run = run_of(world, op);
    let Some(plan) = world.resource::<TrackJobs>().basis.get(&op).map(|b| b.plan.clone()) else { return };
    let mut jobs = world.resource_mut::<TrackJobs>();
    for side in [Side::Forward, Side::Backward] {
        let want = plan.wanted(side, run);
        let unwanted = jobs.running.get(&(op, side)).is_some_and(|j| want.is_empty() || j.owned.hull().is_some_and(|h| h.start < want.start || h.end > want.end));
        if unwanted {
            jobs.running.remove(&(op, side));
        }
    }
}

/// Whether a tracker's work waits to start, and for what.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Waiting {
    No,
    /// For a free slot ([`MAX_JOBS`]), or for new dirt to be planned first.
    Yes,
    /// For another CoTracker job's worker to end ([`cotracker_jobs`]).
    ForCoTracker,
    /// CoTracker work that could only wait at the playhead anyway, while the
    /// worker is taken: nothing to do until the playhead moves.
    AtPlayhead,
}

/// Start a job on each side that has frames to do (that the tracker is
/// asked to track) and no job, and say whether some work waits.
///
/// A side that can track now goes first (in catch-up mode, the side the
/// playhead is on). A CoTracker job waits while [`cotracker_jobs`] workers
/// are alive; one that could only wait at the playhead also waits while a
/// CoTracker job that could track wants a worker (or a slot, to start one).
fn start_jobs(world: &mut World, op: Entity, footage: &Footage) -> Waiting {
    let Some((plan, done)) = world.resource::<TrackJobs>().basis.get(&op).filter(|b| !b.failed).map(|b| (b.plan.clone(), b.done.clone())) else {
        return Waiting::No;
    };
    if world.get::<Dirty>(op).is_some_and(|d| !d.0.is_empty()) {
        return Waiting::Yes; // the plan is about to change
    }
    let Some(out) = world.get::<AutoOutput>(op).map(|o| o.0) else { return Waiting::No };
    // Where the tracker itself was: its results minus the re-centring shift
    // (not what a person drew: drawing never changes what it tracks).
    let offset = world.get::<TrackBook>(op).map_or([0.0; 2], |b| b.offset);
    let run = run_of(world, op);
    let mut specs = Vec::new();
    for side in [Side::Forward, Side::Backward] {
        if world.resource::<TrackJobs>().running.contains_key(&(op, side)) {
            continue;
        }
        let range = plan.side(side);
        let want = plan.wanted(side, run);
        let todo = difference(&RangeSet::from_range(want.clone()), &done);
        let Some(hull) = todo.hull() else { continue };
        // From the frame nearest the anchor still to do, resuming from the result just before it.
        let (from, before) = match side {
            Side::Forward => (hull.start, hull.start - 1),
            Side::Backward => (hull.end - 1, hull.end),
        };
        let resume = range.contains(&before).then(|| world.resource::<SignalStore>().get(out)?.get_valid(before).map(|v| ([(v[0] - offset[0]) as f64, (v[1] - offset[1]) as f64], v[6]))).flatten();
        let from = match (resume, side) {
            (Some(_), _) => from,
            (None, Side::Forward) => plan.anchor,
            (None, Side::Backward) => plan.anchor - 1,
        };
        let to = if side == Side::Forward { want.end - 1 } else { want.start };
        let limit = catch_up_limit(world, op, side);
        let now = if side == Side::Forward { from < limit } else { from >= limit };
        let label = world.get::<Name>(op).map_or_else(|| format!("tracker {op}"), |n| format!("{n} ({op})"));
        specs.push((now, job_spec(&plan, side, from, to, resume, footage, label)));
    }
    specs.sort_by_key(|(now, _)| !*now);
    for (now, spec) in specs {
        {
            let mut jobs = world.resource_mut::<TrackJobs>();
            if spec.starts_worker() {
                if jobs.cotracker_workers() >= cotracker_jobs() {
                    jobs.worker_wanted |= now;
                    return if now { Waiting::ForCoTracker } else { Waiting::AtPlayhead };
                }
                if !now && (jobs.worker_wanted_before || jobs.worker_queued_before) {
                    return Waiting::AtPlayhead;
                }
            }
            if jobs.working() >= MAX_JOBS {
                jobs.worker_queued |= now && spec.starts_worker();
                return Waiting::Yes;
            }
        }
        start_job(world, op, spec);
    }
    Waiting::No
}

/// What a job needs from the tracker's plan.
fn job_spec(plan: &Plan, side: Side, from: FrameIndex, to: FrameIndex, resume: Option<([f64; 2], f32)>, footage: &Footage, label: String) -> JobSpec {
    let p = &plan.params;
    JobSpec {
        side,
        anchor: plan.anchor,
        from,
        to,
        resume,
        lo: plan.lo,
        guide: plan.guide.clone(),
        maps: plan.maps.clone(),
        scale: plan.scale,
        search: p.search.max(0.1) as f64,
        settings: Settings { adapt: p.adapt.clamp(0.0, 1.0), min_score: p.min_score.clamp(-1.0, 1.0), tolerance: p.matching.tolerance() },
        video: plan.video.clone(),
        k: plan.k,
        grid: footage.original.clone(),
        decode: footage.decode.clone(),
        looks: plan.looks.clone(),
        seed: plan.seed,
        fuse: p.fuse,
        method: p.method,
        root: plan.root,
        label,
    }
}

fn start_job(world: &mut World, op: Entity, spec: JobSpec) {
    let (side, from, to, worker) = (spec.side, spec.from, spec.to, spec.starts_worker());
    let shared = Arc::new(Shared::new(catch_up_limit(world, op, side), from));
    let (tx, rx) = channel();
    let owned = RangeSet::from_range(from.min(to)..from.max(to) + 1);
    let jobs = world.resource::<TrackJobs>();
    spawn(spec, shared.clone(), tx, &jobs.threads, &jobs.workers);
    let job = Running { shared, rx: Mutex::new(rx), worker, owned, from, to, started: Instant::now(), produced: 0 };
    world.resource_mut::<TrackJobs>().running.insert((op, side), job);
}

/// Catch-up mode: forward jobs track frames before the limit, backward jobs
/// frames at or after it.
fn catch_up_limit(world: &World, op: Entity, side: Side) -> FrameIndex {
    let follow = world.get::<Tracker>(op).is_some_and(|t| t.follow_playhead);
    let playhead = world.resource::<Transport>().frame();
    match (follow, side) {
        (true, Side::Forward) => playhead + 1,
        (true, Side::Backward) => playhead,
        (false, Side::Forward) => FrameIndex::MAX,
        (false, Side::Backward) => FrameIndex::MIN,
    }
}

/// Once the results stop coming in (finished, or parked at the playhead),
/// re-centre them; and stamp them if they are complete.
fn settle(world: &mut World, op: Entity) {
    let Some((stamp, complete, unsettled)) = world.resource::<TrackJobs>().basis.get(&op).map(|b| {
        let p = b.plan.produced();
        (b.plan.stamp, b.done.intersect(&p).len() == p.end - p.start, b.unsettled)
    }) else {
        return;
    };
    let resting = {
        let jobs = world.resource::<TrackJobs>();
        [Side::Forward, Side::Backward].iter().filter_map(|s| jobs.running.get(&(op, *s))).all(|j| j.shared.parked.load(Ordering::Relaxed))
    };
    // (Not while new inputs are pending: most frames are stale then. Nor with
    // an error: the path on screen is only what was left.)
    let dirty = world.get::<Dirty>(op).is_some_and(|d| !d.0.is_empty());
    if unsettled && resting && !dirty && world.get::<OpError>(op).is_none() {
        if let Some(b) = world.resource_mut::<TrackJobs>().basis.get_mut(&op) {
            b.unsettled = false;
        }
        recenter(world, op);
    }
    let book = world.get::<TrackBook>(op).copied().unwrap_or_default();
    set_book(world, op, TrackBook { stamp: if complete { stamp } else { 0 }, ..book });
}

/// Store a tracker's bookkeeping; a change is a document change (autosave),
/// though not an undo step.
fn set_book(world: &mut World, op: Entity, book: TrackBook) {
    if world.get::<TrackBook>(op) != Some(&book) {
        world.entity_mut(op).insert(book);
        world.resource_mut::<History>().touch();
    }
}

/// Shift the tracker's path onto its guide's (or back, with re-centring off
/// or too few frames where it saw the subject).
fn recenter(world: &mut World, op: Entity) {
    let Some(params) = world.get::<Tracker>(op).cloned() else { return };
    let book = world.get::<TrackBook>(op).copied().unwrap_or_default();
    let guide = guide_of(world, op).filter(|g| world.get::<Disabled>(*g).is_none());
    let (Some(out), Some(guide)) = (world.get::<AutoOutput>(op).map(|o| o.0), guide.and_then(|g| world.get::<Output>(g)).map(|o| o.0)) else { return };
    let new = if params.center_on_guide {
        let store = world.resource::<SignalStore>();
        let (Some(sig), Some(g)) = (store.get(out), store.get(guide)) else { return };
        let Some((lo, hi)) = sig.present_hull() else { return };
        let (mut dx, mut dy) = (Vec::new(), Vec::new());
        for f in lo..=hi {
            if let (Some(v), Some(gv)) = (sig.get_valid(f), g.get(f))
                && crate::flags(v) == 0
            {
                dx.push(gv[0] - (v[0] - book.offset[0]));
                dy.push(gv[1] - (v[1] - book.offset[1]));
            }
        }
        let median = |v: &mut Vec<f32>| {
            v.sort_by(f32::total_cmp);
            v[v.len() / 2]
        };
        if dx.len() < 3 { [0.0, 0.0] } else { [median(&mut dx), median(&mut dy)] }
    } else {
        [0.0, 0.0]
    };
    let [dx, dy] = [new[0] - book.offset[0], new[1] - book.offset[1]];
    if dx.abs() < 1e-3 && dy.abs() < 1e-3 {
        return;
    }
    let mut store = world.resource_mut::<SignalStore>();
    let Some(sig) = store.get_mut(out) else { return };
    let Some((lo, hi)) = sig.present_hull() else { return };
    for f in lo..=hi {
        let state = sig.state(f);
        let Some(v) = sig.get(f).filter(|v| v.len() == TRACK_CHANNELS).map(|v| [v[0] + dx, v[1] + dy, v[2] + dx, v[3] + dy, v[4] + dx, v[5] + dy, v[6], v[7]]) else { continue };
        sig.set(f, &v);
        if state == FrameState::Stale {
            sig.mark_stale(f..f + 1);
        }
    }
    set_book(world, op, TrackBook { offset: new, ..book });
}

/// Move the catch-up limits and refresh the status. `waiting`: what its
/// work waits for (from [`start_jobs`]).
fn update_status(world: &mut World, op: Entity, waiting: Waiting) {
    let limits = [Side::Forward, Side::Backward].map(|s| catch_up_limit(world, op, s));
    let run = run_of(world, op);
    let jobs = world.resource::<TrackJobs>();
    let mut status = match jobs.basis.get(&op) {
        Some(b) => {
            let rendition = if jobs.video.as_ref().is_some_and(|v| Arc::ptr_eq(v, &b.plan.video)) { "original" } else { "proxy" };
            TrackStatus { rendition: format!("{rendition} {}×{}", b.plan.video.width, b.plan.video.height), anchor: b.plan.anchor, ..Default::default() }
        }
        None => TrackStatus { anchor: world.get::<Tracker>(op).map_or(0, |t| t.anchor), ..Default::default() },
    };
    for (side, limit) in [Side::Forward, Side::Backward].into_iter().zip(limits) {
        let s = jobs.running.get(&(op, side)).map(|job| {
            job.shared.limit.store(limit, Ordering::Relaxed);
            let secs = job.started.elapsed().as_secs_f64();
            SideStatus {
                from: job.from,
                at: job.shared.at.load(Ordering::Relaxed),
                to: job.to,
                fps: if secs > 0.2 { job.produced as f64 / secs } else { 0.0 },
                waiting: job.shared.waiting.load(Ordering::Relaxed),
                phase: job.shared.phase(),
            }
        });
        match side {
            Side::Forward => status.forward = s,
            Side::Backward => status.backward = s,
        }
    }
    // Frames left on a side it is asked to track, and no job there yet.
    if let Some(b) = jobs.basis.get(&op).filter(|b| !b.failed) {
        status.queued = [Side::Forward, Side::Backward].into_iter().any(|s| {
            let w = b.plan.wanted(s, run);
            !jobs.running.contains_key(&(op, s)) && b.done.intersect(&w).len() < w.end - w.start
        });
    }
    // (What is left waits for the playhead, not for a turn.)
    if waiting == Waiting::AtPlayhead {
        status.waits_at_playhead = status.queued;
        status.queued = false;
    }
    status.waits_for_cotracker = status.queued && waiting == Waiting::ForCoTracker;
    world.entity_mut(op).insert(status);
}

/// Snapshot what a tracker's jobs need. `prev`: the plan its results came
/// from. Err = why it can't run.
fn plan(world: &World, op: Entity, footage: &Footage, prev: Option<&Plan>) -> Result<Plan, String> {
    let params = world.get::<Tracker>(op).cloned().ok_or("no tracker settings")?;
    if params.method == Method::Manual {
        return Err("a manual dot has nothing to track".into());
    }
    let (lo, hi, guide_boxes) = match guide_of(world, op) {
        Some(guide) => {
            if world.get::<Disabled>(guide).is_some() {
                return Err("the guide was deleted".into());
            }
            // (Through the guide's span: a trimmed sketch guides only where it lives.)
            let sig = tt_core::span::output(world, guide).ok_or("the guide has no output")?;
            if sig.channels() < 6 {
                return Err("the guide is not a box".into());
            }
            let (lo, hi) = sig.present_hull().ok_or("the guide has no frames yet")?;
            // The guide's boxes over its whole span, gaps interpolated.
            let mut boxes: Vec<Option<[f64; 6]>> = (lo..=hi).map(|f| sig.get(f).map(|v| std::array::from_fn(|c| v[c] as f64))).collect();
            fill_gaps(&mut boxes);
            (lo, hi, boxes.into_iter().map(|b| b.expect("filled")).collect::<Vec<[f64; 6]>>())
        }
        // No guide: the whole frame is the search region, on every frame of the video.
        None => {
            let n = footage.original.frame_count();
            if n == 0 {
                return Err("the video has no frames".into());
            }
            let (w, h) = (footage.original.width as f64, footage.original.height as f64);
            (0, n - 1, vec![[w / 2.0, h / 2.0, 0.0, 0.0, w, h]; n as usize])
        }
    };
    let space = home_of(world, op);
    let maps: Vec<SpaceMap> = (lo..=hi).map(|f| map_at(world, space, f)).collect();

    // The looks the user showed it (those on frames the guide covers), and the seed.
    let looks: Vec<LookSpec> = crate::look::looks_of(world, op)
        .iter()
        .filter_map(|e| world.get::<Look>(*e))
        .filter(|l| (lo..=hi).contains(&l.frame))
        .map(|l| LookSpec { frame: l.frame, center: l.center(), half: l.half(), mask: l.painted().map(<[u8]>::to_vec) })
        .collect();
    // It starts on its anchor frame, from the look there (a painted one
    // first); with no look there (it was deleted), from its first look.
    let anchor = params.anchor.clamp(lo, hi);
    let anchor = if looks.is_empty() || looks.iter().any(|l| l.frame == anchor) { anchor } else { looks[0].frame };
    let seed = crate::job::look_on(&looks, anchor).map(|i| looks[i].center);

    // Patch scale: the first look's larger half-size spans `LOOK_PX` patch
    // pixels; with no looks (an older tracker), the feature (a fraction of
    // the guide's box at the anchor) spans the template.
    let scale = match looks.first() {
        Some(l) => {
            let a = maps[(l.frame - lo) as usize].a;
            (LOOK_PX / (l.half[0].max(l.half[1]) / a).max(0.5)).clamp(1.0 / 32.0, 2.0)
        }
        None => {
            let at = (anchor - lo) as usize;
            let b = maps[at].box_from_source(guide_boxes[at]);
            let half = ((b[0] - b[2]).max(b[4] - b[0])).min((b[1] - b[3]).max(b[5] - b[1])).max(1.0);
            (TEMPLATE_R as f64 / (params.feature.max(0.01) as f64 * half)).clamp(1.0 / 32.0, 8.0)
        }
    };

    // Rendition: the proxy only where it has a pixel per patch pixel on every
    // frame (on both axes: a proxy's width is rounded to even).
    let original = &footage.original;
    let k_of = |v: &VideoIndex| [v.width as f64 / original.width as f64, v.height as f64 / original.height as f64];
    let proxy_ok = |p: &VideoIndex| {
        let k = k_of(p);
        maps.iter().all(|m| m.a * k[0].min(k[1]) / scale >= 1.0)
    };
    let video = match (params.rendition, &footage.proxy) {
        (Rendition::Original, _) | (_, None) => original.clone(),
        (Rendition::Proxy, Some(p)) => p.clone(),
        // Keep reading what the results so far came from while it serves
        // (the original always does), so a proxy that became ready meanwhile
        // doesn't re-track everything on the next edit.
        (Rendition::Auto, Some(p)) => match prev.filter(|q| q.params.rendition == Rendition::Auto) {
            Some(q) if Arc::ptr_eq(&q.video, original) => original.clone(),
            _ if proxy_ok(p) => p.clone(),
            _ => original.clone(),
        },
    };
    let k = k_of(&video);

    // The stamp: everything complete results depend on. Not which copy of the
    // video was read: Auto may pick the proxy next session, and the
    // original's results still hold.
    let mut h = blake3::Hasher::new();
    let mut put = |x: f64| {
        h.update(&x.to_le_bytes());
    };
    let p = &params;
    for x in [ALGO_VERSION as f64, anchor as f64, p.direction as u8 as f64, p.rendition as u8 as f64, p.feature as f64, p.search as f64, p.adapt as f64, p.min_score as f64, p.fuse as u8 as f64] {
        put(x);
    }
    let m = &p.matching;
    for x in [m.contrast as f64, m.brightness as f64, m.colour as u8 as f64, m.colour_slack as f64, p.method as u8 as f64] {
        put(x);
    }
    for x in [lo as f64, hi as f64, original.width as f64, original.height as f64, original.frames.len() as f64] {
        put(x);
    }
    for b in &guide_boxes {
        b.iter().for_each(|x| put(*x));
    }
    for m in &maps {
        [m.a, m.b[0], m.b[1]].iter().for_each(|x| put(*x));
    }
    for l in &looks {
        [l.frame as f64, l.center[0], l.center[1], l.half[0], l.half[1]].iter().for_each(|x| put(*x));
        l.mask.iter().flatten().for_each(|c| put(*c as f64));
    }
    // (Only when there is no guide, so a guided tracker's stamp stays what it was.)
    let root = guide_of(world, op).is_none();
    if root {
        put(-1.0);
    }
    h.update(original.path.to_string_lossy().as_bytes());
    let stamp = u64::from_le_bytes(h.finalize().as_bytes()[..8].try_into().expect("8 bytes"));

    let span = tt_core::span::span_of(world, op).range();
    Ok(Plan { lo, hi, span, anchor, params, guide: Arc::new(guide_boxes), maps: Arc::new(maps), scale, video, k, stamp, looks: Arc::new(looks), seed, root })
}

/// Fill gaps by linear interpolation; ends hold the nearest value.
fn fill_gaps(v: &mut [Option<[f64; 6]>]) {
    let known: Vec<usize> = (0..v.len()).filter(|i| v[*i].is_some()).collect();
    let (Some(&first), Some(&last)) = (known.first(), known.last()) else { return };
    let (head, tail) = (v[first], v[last]);
    v[..first].fill(head);
    v[last + 1..].fill(tail);
    for w in known.windows(2) {
        let (a, b) = (w[0], w[1]);
        let (va, vb) = (v[a].expect("known"), v[b].expect("known"));
        for (i, slot) in v.iter_mut().enumerate().take(b).skip(a + 1) {
            let t = (i - a) as f64 / (b - a) as f64;
            *slot = Some(std::array::from_fn(|c| va[c] + (vb[c] - va[c]) * t));
        }
    }
}

/// Frames the output covers (for tests and panels).
pub fn coverage(world: &World, op: Entity) -> Option<Range<FrameIndex>> {
    let sig = world.resource::<SignalStore>().get(world.get::<Output>(op)?.0)?;
    let (lo, hi) = sig.present_hull()?;
    Some(lo..hi + 1)
}

/// Whether a tracker has nothing left to do: no jobs, no dirt, and its
/// results complete where it is asked to track (or a job failed; a paused
/// tracker has nothing to do). For tests and scripts.
pub fn settled(world: &World, op: Entity) -> bool {
    // (A manual dot never runs anything.)
    if is_manual(world, op) {
        return true;
    }
    let jobs = world.resource::<TrackJobs>();
    let running = [Side::Forward, Side::Backward].iter().any(|s| jobs.running.contains_key(&(op, *s)));
    let dirty = world.get::<Dirty>(op).is_some_and(|d| !d.0.is_empty());
    let run = run_of(world, op);
    let complete = jobs.basis.get(&op).is_some_and(|b| {
        b.failed
            || [Side::Forward, Side::Backward].iter().all(|s| {
                let w = b.plan.wanted(*s, run);
                b.done.intersect(&w).len() == w.end - w.start
            })
    });
    !running && !dirty && (complete || world.get::<OpError>(op).is_some())
}
