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
//!   the result just before them. A running job stays if its inputs didn't
//!   change and it covers exactly what is left on its side; otherwise it is
//!   cancelled and replaced.
//! - Results land as they arrive; old ones stay on screen as stale until
//!   replaced (stale-while-revalidate), and dependents update chunk by chunk.
//! - A [`TrackBook`] (saved) stamps the inputs complete results came from, so
//!   a reopened project keeps them instead of tracking again. The stamp is 0
//!   while results are incomplete or mixed.
//! - When the results stop coming in (finished, or parked at the playhead),
//!   the path is shifted onto the guide's: by the median offset between the
//!   two over the frames it saw the subject. The tracker gives the motion's
//!   shape; the rough pass, on average, where the subject is (instead of
//!   wherever the guide happened to be at the anchor).

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::prelude::*;
use bevy_reflect::Reflect;
use tt_core::history::History;
use tt_core::op::{Dirty, Inputs, Invalidations, OpError, Operator, Output, extent};
use tt_core::ranges::RangeSet;
use tt_core::signal::{FrameState, SignalStore};
use tt_core::time::FrameIndex;
use tt_core::transport::Transport;
use tt_core::view::{SpaceMap, home_of, map_at};
use tt_media::{DecodeOptions, VideoIndex};

use crate::job::{JobSpec, Msg, Shared, Side, spawn};
use crate::template::{Settings, TEMPLATE_R};
use crate::{Direction, Rendition, Tracker, guide_of};

/// Part of every stamp: bump it when a change to the tracking makes saved
/// results out of date (they re-track when their project is opened).
const ALGO_VERSION: u64 = 1;

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
}

impl TrackStatus {
    pub fn busy(&self) -> bool {
        self.forward.is_some() || self.backward.is_some()
    }
}

/// Worker threads decoding at once, across trackers (each runs its own
/// ffmpeg); more jobs wait their turn. Cancelled threads count until they
/// stop; jobs parked at the playhead don't.
pub const MAX_JOBS: usize = 4;

#[derive(Resource, Default)]
pub struct TrackJobs {
    running: HashMap<(Entity, Side), Running>,
    /// What each live tracker's results came from.
    basis: HashMap<Entity, Basis>,
    /// Worker threads alive, cancelled ones included.
    threads: Arc<AtomicUsize>,
    /// The video of the document the jobs belong to.
    video: Option<Arc<VideoIndex>>,
    /// A tracker has work it hasn't started (inputs still settling, a drag
    /// in progress, no free slot).
    pending: bool,
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
    /// A job failed: nothing restarts until the inputs change.
    failed: bool,
    /// Results changed since the last re-centring.
    unsettled: bool,
}

/// Inputs snapshotted for jobs.
struct Plan {
    lo: FrameIndex,
    hi: FrameIndex,
    /// `params.anchor` moved into the guide's frames.
    anchor: FrameIndex,
    params: Tracker,
    guide: Arc<Vec<[f64; 6]>>,
    maps: Arc<Vec<SpaceMap>>,
    scale: f64,
    video: Arc<VideoIndex>,
    k: [f64; 2],
    stamp: u64,
}

impl Plan {
    /// Frames a side produces: forward from the anchor (the anchor itself
    /// included), backward the frames before it.
    fn side(&self, side: Side) -> Range<FrameIndex> {
        match (side, self.params.direction) {
            (Side::Forward, Direction::Backward) => self.anchor..self.anchor + 1,
            (Side::Forward, _) => self.anchor..self.hi + 1,
            (Side::Backward, Direction::Forward) => self.anchor..self.anchor,
            (Side::Backward, _) => self.lo..self.anchor,
        }
    }

    /// Frames the tracker produces.
    fn produced(&self) -> Range<FrameIndex> {
        self.side(Side::Backward).start..self.side(Side::Forward).end
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
            && (p.feature, p.search, p.adapt, p.min_score) == (q.feature, q.search, q.adapt, q.min_score);
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
        drain(world, op);
        replan(world, op, &footage);
        let waiting = start_jobs(world, op, &footage);
        settle(world, op);
        update_status(world, op);
        let dirty = world.get::<Dirty>(op).is_some_and(|d| !d.0.is_empty());
        world.resource_mut::<TrackJobs>().pending |= waiting || dirty;
    }
}

/// Write finished frames into the tracker's output.
fn drain(world: &mut World, op: Entity) {
    let mut msgs: Vec<(Side, Msg)> = Vec::new();
    {
        let jobs = world.resource::<TrackJobs>();
        for side in [Side::Forward, Side::Backward] {
            if let Some(job) = jobs.running.get(&(op, side)) {
                msgs.extend(job.rx.lock().expect("not poisoned").try_iter().map(|m| (side, m)));
            }
        }
    }
    if msgs.is_empty() {
        return;
    }
    let Some(out) = world.get::<Output>(op).map(|o| o.0) else { return };
    let [ox, oy] = world.get::<TrackBook>(op).map_or([0.0; 2], |b| b.offset);
    // Frames whose inputs changed since the job started (the new plan waits
    // for a drag to end): their new values are already out of date.
    let outdated = world.get::<Dirty>(op).map(|d| d.0.clone()).unwrap_or_default();
    let mut changed = RangeSet::default();
    for (side, msg) in msgs {
        match msg {
            Msg::Frames(frames) => {
                if let Some(sig) = world.resource_mut::<SignalStore>().get_mut(out) {
                    for (f, v) in &frames {
                        let [x, y, l, t, r, b, s] = *v;
                        sig.set(*f, &[x + ox, y + oy, l + ox, t + oy, r + ox, b + oy, s]);
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
                for (f, _) in &frames {
                    changed.insert(*f..*f + 1);
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
    let mut inv = world.resource_mut::<Invalidations>();
    for r in changed.ranges() {
        inv.output_changed(op, r.clone());
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
    let Some(out) = world.get::<Output>(op).map(|o| o.0) else { return };
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
    // with no basis (a reopened project), saved results whose stamp matches.
    let produced = plan.produced();
    let present: RangeSet = {
        let sig = world.resource::<SignalStore>().get(out);
        let mut set = RangeSet::new();
        for (r, _) in sig.map(|s| s.runs(produced.clone())).unwrap_or_default() {
            set.insert(r);
        }
        set
    };
    let kept = old.as_ref().map_or(plan.anchor..plan.anchor, |b| plan.kept(&b.plan));
    let done = match &old {
        Some(b) => intersection(&b.done.intersect(&kept), &present),
        None if world.get::<TrackBook>(op).is_some_and(|b| b.stamp == plan.stamp) && present.len() == produced.end - produced.start => present,
        None => RangeSet::new(),
    };

    // A job stays if its inputs didn't change and what's left to do on its
    // side is exactly what it will produce (a new anchor, direction, setting
    // or edit on its frames replaces it).
    {
        let mut jobs = world.resource_mut::<TrackJobs>();
        for side in [Side::Forward, Side::Backward] {
            let todo = difference(&RangeSet::from_range(plan.side(side)), &done);
            let stays = jobs.running.get(&(op, side)).is_some_and(|job| {
                job.owned == todo && job.owned.hull().is_none_or(|h| kept.start <= h.start && h.end <= kept.end)
            });
            if !stays {
                jobs.running.remove(&(op, side));
            }
        }
    }

    // The output: what holds is valid, the rest stale until replaced, and
    // nothing outside the frames the tracker produces.
    let mut cleared = Vec::new();
    if let Some(sig) = world.resource_mut::<SignalStore>().get_mut(out) {
        for r in [extent.start..produced.start, produced.end..extent.end] {
            if !r.is_empty() && !sig.runs(r.clone()).is_empty() {
                sig.clear(r.clone());
                cleared.push(r);
            }
        }
        sig.mark_stale(produced.clone());
        for r in done.ranges() {
            sig.mark_valid(r.clone());
        }
    }
    let mut inv = world.resource_mut::<Invalidations>();
    for r in cleared {
        inv.output_changed(op, r);
    }
    world.resource_mut::<TrackJobs>().basis.insert(op, Basis { plan, done, failed: false, unsettled: true });
}

/// Start a job on each side that has frames to do and no job. True if some
/// work is waiting (for a free slot, or for new dirt to be planned first).
fn start_jobs(world: &mut World, op: Entity, footage: &Footage) -> bool {
    let Some((plan, done)) = world.resource::<TrackJobs>().basis.get(&op).filter(|b| !b.failed).map(|b| (b.plan.clone(), b.done.clone())) else {
        return false;
    };
    if world.get::<Dirty>(op).is_some_and(|d| !d.0.is_empty()) {
        return true; // the plan is about to change
    }
    let Some(out) = world.get::<Output>(op).map(|o| o.0) else { return false };
    // Where the tracker itself was: the output minus the re-centring shift.
    let offset = world.get::<TrackBook>(op).map_or([0.0; 2], |b| b.offset);
    for side in [Side::Forward, Side::Backward] {
        if world.resource::<TrackJobs>().running.contains_key(&(op, side)) {
            continue;
        }
        let range = plan.side(side);
        let todo = difference(&RangeSet::from_range(range.clone()), &done);
        let Some(hull) = todo.hull() else { continue };
        if world.resource::<TrackJobs>().working() >= MAX_JOBS {
            return true;
        }
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
        let to = if side == Side::Forward { range.end - 1 } else { range.start };
        start_job(world, op, side, &plan, from, to, resume, footage);
    }
    false
}

#[allow(clippy::too_many_arguments)]
fn start_job(world: &mut World, op: Entity, side: Side, plan: &Plan, from: FrameIndex, to: FrameIndex, resume: Option<([f64; 2], f32)>, footage: &Footage) {
    let p = &plan.params;
    let spec = JobSpec {
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
        settings: Settings { adapt: p.adapt.clamp(0.0, 1.0), min_score: p.min_score.clamp(-1.0, 1.0) },
        video: plan.video.clone(),
        k: plan.k,
        grid: footage.original.clone(),
        decode: footage.decode.clone(),
    };
    let shared = Arc::new(Shared::new(catch_up_limit(world, op, side), from));
    let (tx, rx) = channel();
    let owned = RangeSet::from_range(from.min(to)..from.max(to) + 1);
    let threads = world.resource::<TrackJobs>().threads.clone();
    spawn(spec, shared.clone(), tx, &threads);
    let job = Running { shared, rx: Mutex::new(rx), owned, from, to, started: Instant::now(), produced: 0 };
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
    let (Some(out), Some(guide)) = (world.get::<Output>(op).map(|o| o.0), guide.and_then(|g| world.get::<Output>(g)).map(|o| o.0)) else { return };
    let new = if params.center_on_guide {
        let store = world.resource::<SignalStore>();
        let (Some(sig), Some(g)) = (store.get(out), store.get(guide)) else { return };
        let Some((lo, hi)) = sig.present_hull() else { return };
        let (mut dx, mut dy) = (Vec::new(), Vec::new());
        for f in lo..=hi {
            if let (Some(v), Some(gv)) = (sig.get_valid(f), g.get(f))
                && v[6] >= params.min_score
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
        let Some(v) = sig.get(f).map(|v| [v[0] + dx, v[1] + dy, v[2] + dx, v[3] + dy, v[4] + dx, v[5] + dy, v[6]]) else { continue };
        sig.set(f, &v);
        if state == FrameState::Stale {
            sig.mark_stale(f..f + 1);
        }
    }
    world.resource_mut::<Invalidations>().output_changed(op, lo..hi + 1);
    set_book(world, op, TrackBook { offset: new, ..book });
}

/// Move the catch-up limits and refresh the status.
fn update_status(world: &mut World, op: Entity) {
    let limits = [Side::Forward, Side::Backward].map(|s| catch_up_limit(world, op, s));
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
            }
        });
        match side {
            Side::Forward => status.forward = s,
            Side::Backward => status.backward = s,
        }
    }
    world.entity_mut(op).insert(status);
}

/// Snapshot what a tracker's jobs need. `prev`: the plan its results came
/// from. Err = why it can't run.
fn plan(world: &World, op: Entity, footage: &Footage, prev: Option<&Plan>) -> Result<Plan, String> {
    let params = world.get::<Tracker>(op).cloned().ok_or("no tracker settings")?;
    let guide = guide_of(world, op).filter(|g| world.get::<Disabled>(*g).is_none()).ok_or("the guide was deleted")?;
    let sig = world.get::<Output>(guide).and_then(|o| world.resource::<SignalStore>().get(o.0)).ok_or("the guide has no output")?;
    if sig.channels() < 6 {
        return Err("the guide is not a box".into());
    }
    let (lo, hi) = sig.present_hull().ok_or("the guide has no frames yet")?;
    let anchor = params.anchor.clamp(lo, hi);

    // The guide's boxes over its whole span, gaps interpolated.
    let mut boxes: Vec<Option<[f64; 6]>> = (lo..=hi).map(|f| sig.get(f).map(|v| std::array::from_fn(|c| v[c] as f64))).collect();
    fill_gaps(&mut boxes);
    let guide_boxes: Vec<[f64; 6]> = boxes.into_iter().map(|b| b.expect("filled")).collect();
    let space = home_of(world, op);
    let maps: Vec<SpaceMap> = (lo..=hi).map(|f| map_at(world, space, f)).collect();

    // Patch scale: the feature (a fraction of the guide's box at the anchor) spans the template.
    let at = (anchor - lo) as usize;
    let b = maps[at].box_from_source(guide_boxes[at]);
    let half = ((b[0] - b[2]).max(b[4] - b[0])).min((b[1] - b[3]).max(b[5] - b[1])).max(1.0);
    let scale = (TEMPLATE_R as f64 / (params.feature.max(0.01) as f64 * half)).clamp(1.0 / 32.0, 8.0);

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
    for x in [ALGO_VERSION as f64, anchor as f64, p.direction as u8 as f64, p.rendition as u8 as f64, p.feature as f64, p.search as f64, p.adapt as f64, p.min_score as f64] {
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
    h.update(original.path.to_string_lossy().as_bytes());
    let stamp = u64::from_le_bytes(h.finalize().as_bytes()[..8].try_into().expect("8 bytes"));

    Ok(Plan { lo, hi, anchor, params, guide: Arc::new(guide_boxes), maps: Arc::new(maps), scale, video, k, stamp })
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
/// results complete (or a job failed). For tests and scripts.
pub fn settled(world: &World, op: Entity) -> bool {
    let jobs = world.resource::<TrackJobs>();
    let running = [Side::Forward, Side::Backward].iter().any(|s| jobs.running.contains_key(&(op, *s)));
    let dirty = world.get::<Dirty>(op).is_some_and(|d| !d.0.is_empty());
    let complete = jobs.basis.get(&op).is_some_and(|b| {
        let p = b.plan.produced();
        b.failed || b.done.intersect(&p).len() == p.end - p.start
    });
    !running && !dirty && (complete || world.get::<OpError>(op).is_some())
}
