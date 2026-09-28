//! The tracker runner (Set::Jobs): turns trackers' dirty frames into
//! background jobs, and their results into signal writes.
//!
//! - A tracker waits until its inputs (guide, view) are evaluated, then
//!   snapshots them into a job per side of the anchor that has dirty frames.
//!   A side whose inputs didn't change keeps running or keeps its results.
//! - New dirt on a side cancels its job and restarts it from the first dirty
//!   frame, resuming from the result just before it when that is still valid.
//! - Results land as they arrive; old ones stay on screen as stale until
//!   replaced (stale-while-revalidate), and dependents update chunk by chunk.
//! - A [`TrackStamp`] (saved) hashes the inputs the results came from, so a
//!   reopened project keeps its results instead of tracking again.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, Mutex};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, channel};
use std::time::Instant;

use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::prelude::*;
use bevy_reflect::Reflect;
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

/// The video trackers read (set by the host when media opens and when its proxy is ready).
#[derive(Resource, Clone)]
pub struct Footage {
    pub original: Arc<VideoIndex>,
    pub proxy: Option<Arc<VideoIndex>>,
    pub decode: DecodeOptions,
}

/// Hash of the inputs a tracker's results came from (saved with the project).
#[derive(Component, Reflect, Clone, Copy, Debug, Default, PartialEq)]
#[reflect(Component)]
pub struct TrackStamp(pub u64);

/// What a tracker is doing, for panels.
#[derive(Component, Clone, Debug, Default)]
pub struct TrackStatus {
    pub forward: Option<SideStatus>,
    pub backward: Option<SideStatus>,
    /// The rendition being read ("original 1920×1080", "proxy 1280×720").
    pub rendition: String,
}

#[derive(Clone, Copy, Debug)]
pub struct SideStatus {
    /// The last frame produced, and where the job ends.
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

/// Jobs running at once, across trackers (each decodes with its own ffmpeg);
/// more wait their turn.
pub const MAX_JOBS: usize = 4;

#[derive(Resource, Default)]
pub struct TrackJobs {
    running: HashMap<(Entity, Side), Running>,
}

impl TrackJobs {
    /// Jobs running now.
    pub fn busy(&self) -> usize {
        self.running.len()
    }
}

struct Running {
    shared: Arc<Shared>,
    /// (In a mutex only because resources must be `Sync`.)
    rx: Mutex<Receiver<Msg>>,
    /// Frames this job will produce and hasn't yet.
    owned: RangeSet,
    to: FrameIndex,
    stamp: u64,
    started: Instant,
    produced: u64,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
    }
}

/// Inputs snapshotted for jobs.
struct Plan {
    lo: FrameIndex,
    hi: FrameIndex,
    anchor: FrameIndex,
    params: Tracker,
    guide: Arc<Vec<[f64; 6]>>,
    maps: Arc<Vec<SpaceMap>>,
    scale: f64,
    video: Arc<VideoIndex>,
    k: f64,
    stamp: u64,
}

pub fn run_trackers(world: &mut World) {
    let live: Vec<Entity> = {
        let mut q = world.query_filtered::<(Entity, &Operator), Without<Disabled>>();
        q.iter(world).filter(|(_, o)| o.kind == "track").map(|(e, _)| e).collect()
    };
    // Deleted trackers: their jobs stop (dropping a job cancels it).
    world.resource_mut::<TrackJobs>().running.retain(|(e, _), _| live.contains(e));
    let Some(footage) = world.get_resource::<Footage>().cloned() else { return };
    for op in live {
        drain(world, op);
        start_dirty(world, op, &footage);
        follow_playhead(world, op);
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
    let mut changed = RangeSet::default();
    let mut finished_stamp = None;
    for (side, msg) in msgs {
        match msg {
            Msg::Frames(frames) => {
                if let Some(sig) = world.resource_mut::<SignalStore>().get_mut(out) {
                    for (f, v) in &frames {
                        sig.set(*f, v);
                    }
                }
                let mut jobs = world.resource_mut::<TrackJobs>();
                if let Some(job) = jobs.running.get_mut(&(op, side)) {
                    job.produced += frames.len() as u64;
                    for (f, _) in &frames {
                        job.owned.remove(*f..*f + 1);
                        changed.insert(*f..*f + 1);
                    }
                }
            }
            Msg::Finished => {
                if let Some(job) = world.resource_mut::<TrackJobs>().running.remove(&(op, side)) {
                    finished_stamp = Some(job.stamp);
                }
            }
            Msg::Failed(e) => {
                tracing::warn!("tracker {op} ({side:?}) failed: {e}");
                world.resource_mut::<TrackJobs>().running.remove(&(op, side));
                world.entity_mut(op).insert(OpError(e));
            }
        }
    }
    let mut inv = world.resource_mut::<Invalidations>();
    for r in changed.ranges() {
        inv.output_changed(op, r.clone());
    }
    // All done with nothing new to do: remember what the results came from.
    let idle = !world.resource::<TrackJobs>().running.keys().any(|(e, _)| *e == op);
    if let Some(stamp) = finished_stamp
        && idle
        && world.get::<Dirty>(op).is_none_or(|d| d.0.is_empty())
        && world.get::<OpError>(op).is_none()
    {
        world.entity_mut(op).insert(TrackStamp(stamp));
    }
}

fn start_dirty(world: &mut World, op: Entity, footage: &Footage) {
    if world.get::<Dirty>(op).is_none_or(|d| d.0.is_empty()) {
        return;
    }
    // Wait for a free slot (restarting this tracker's own jobs doesn't need one).
    let others = world.resource::<TrackJobs>().running.keys().filter(|(e, _)| *e != op).count();
    if others >= MAX_JOBS {
        return;
    }
    // Wait for the guide and the view to finish evaluating.
    let inputs: Vec<Entity> = world.get::<Inputs>(op).map(|i| i.0.iter().map(|(_, e)| *e).collect()).unwrap_or_default();
    if inputs.iter().any(|e| world.get::<Dirty>(*e).is_some_and(|d| !d.0.is_empty())) {
        return;
    }
    let dirty = std::mem::take(&mut world.get_mut::<Dirty>(op).expect("checked").0);
    let Some(out) = world.get::<Output>(op).map(|o| o.0) else { return };
    let extent = extent(world);
    let plan = match plan(world, op, footage) {
        Ok(p) => p,
        Err(e) => {
            world.resource_mut::<TrackJobs>().running.retain(|(o, _), _| *o != op);
            if let Some(sig) = world.resource_mut::<SignalStore>().get_mut(out) {
                sig.clear(extent.clone());
            }
            world.resource_mut::<Invalidations>().output_changed(op, extent);
            world.entity_mut(op).insert(OpError(e));
            return;
        }
    };
    world.entity_mut(op).remove::<OpError>();
    let (lo, hi, anchor) = (plan.lo, plan.hi, plan.anchor);

    // Unchanged inputs (a reopened project): the results still hold.
    let running = world.resource::<TrackJobs>().running.keys().any(|(e, _)| *e == op);
    let complete = world.resource::<SignalStore>().get(out).is_some_and(|s| {
        s.runs(lo..hi + 1).iter().filter(|(_, st)| *st != FrameState::Absent).map(|(r, _)| r.end - r.start).sum::<FrameIndex>() == hi - lo + 1
    });
    if !running && complete && world.get::<TrackStamp>(op).is_some_and(|s| s.0 == plan.stamp) {
        if let Some(sig) = world.resource_mut::<SignalStore>().get_mut(out) {
            sig.mark_valid(lo..hi + 1);
        }
        return;
    }

    // Outside the guide, and on sides the tracker doesn't run: nothing.
    let forward_end = if plan.params.direction == Direction::Backward { anchor } else { hi };
    let backward = plan.params.direction != Direction::Forward && anchor > lo;
    let mut empty = vec![extent.start..lo, forward_end + 1..extent.end];
    if !backward {
        empty.push(lo..anchor);
    }
    if let Some(sig) = world.resource_mut::<SignalStore>().get_mut(out) {
        for r in &empty {
            sig.clear(r.clone());
        }
    }
    for r in empty.into_iter().filter(|r| !r.is_empty()) {
        world.resource_mut::<Invalidations>().output_changed(op, r);
    }

    let plan = Arc::new(plan);
    let sides = [(Side::Forward, anchor..forward_end + 1), (Side::Backward, if backward { lo..anchor } else { lo..lo })];
    for (side, range) in sides {
        let mut todo = dirty.intersect(&range);
        if todo.is_empty() {
            continue;
        }
        if let Some(old) = world.resource_mut::<TrackJobs>().running.remove(&(op, side)) {
            todo.union(&old.owned.intersect(&range));
        }
        let hull = todo.hull().expect("not empty");
        let valid = |world: &World, f: FrameIndex| -> Option<[f64; 2]> {
            let v = world.resource::<SignalStore>().get(out)?.get_valid(f)?;
            Some([v[0] as f64, v[1] as f64])
        };
        let (from, resume) = match side {
            Side::Forward => match valid(world, hull.start - 1).filter(|_| hull.start > anchor) {
                Some(p) => (hull.start, Some(p)),
                None => (anchor, None),
            },
            Side::Backward => match valid(world, hull.end).filter(|_| hull.end < anchor) {
                Some(p) => (hull.end - 1, Some(p)),
                None => (anchor - 1, None),
            },
        };
        let to = if side == Side::Forward { range.end - 1 } else { range.start };
        start_job(world, op, side, &plan, from, to, resume, footage);
    }
    // Sides that kept running have inputs this plan agrees with (the footprint says so).
    for (key, job) in world.resource_mut::<TrackJobs>().running.iter_mut() {
        if key.0 == op {
            job.stamp = plan.stamp;
        }
    }
    let rendition = if Arc::ptr_eq(&plan.video, &footage.original) { "original" } else { "proxy" };
    let status = TrackStatus { rendition: format!("{rendition} {}×{}", plan.video.width, plan.video.height), ..Default::default() };
    world.entity_mut(op).insert(status);
}

#[allow(clippy::too_many_arguments)]
fn start_job(world: &mut World, op: Entity, side: Side, plan: &Arc<Plan>, from: FrameIndex, to: FrameIndex, resume: Option<[f64; 2]>, footage: &Footage) {
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
    let unlimited = if side == Side::Forward { FrameIndex::MAX } else { FrameIndex::MIN };
    let shared = Arc::new(Shared::new(unlimited, from));
    let (tx, rx) = channel();
    let owned = RangeSet::from_range(from.min(to)..from.max(to) + 1);
    spawn(spec, shared.clone(), tx);
    let job = Running { shared, rx: Mutex::new(rx), owned, to, stamp: plan.stamp, started: Instant::now(), produced: 0 };
    world.resource_mut::<TrackJobs>().running.insert((op, side), job);
}

/// Catch-up mode: jobs stop at the playhead. Also refreshes the status.
fn follow_playhead(world: &mut World, op: Entity) {
    let follow = world.get::<Tracker>(op).is_some_and(|t| t.follow_playhead);
    let playhead = world.resource::<Transport>().frame();
    let mut status = world.get::<TrackStatus>(op).cloned().unwrap_or_default();
    let jobs = world.resource::<TrackJobs>();
    for side in [Side::Forward, Side::Backward] {
        let s = jobs.running.get(&(op, side)).map(|job| {
            let limit = match (follow, side) {
                (true, Side::Forward) => playhead + 1,
                (true, Side::Backward) => playhead,
                (false, Side::Forward) => FrameIndex::MAX,
                (false, Side::Backward) => FrameIndex::MIN,
            };
            job.shared.limit.store(limit, Ordering::Relaxed);
            let secs = job.started.elapsed().as_secs_f64();
            SideStatus {
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

/// Snapshot what a tracker's jobs need. Err = why it can't run.
fn plan(world: &World, op: Entity, footage: &Footage) -> Result<Plan, String> {
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

    // Rendition: the proxy only where it has a pixel per patch pixel on every frame.
    let original = &footage.original;
    let k_of = |v: &VideoIndex| v.width as f64 / original.width as f64;
    let proxy_ok = |p: &Arc<VideoIndex>| maps.iter().all(|m| m.a * k_of(p) / scale >= 1.0);
    let video = match (params.rendition, &footage.proxy) {
        (Rendition::Original, _) | (_, None) => original.clone(),
        (Rendition::Proxy, Some(p)) => p.clone(),
        (Rendition::Auto, Some(p)) => {
            if proxy_ok(p) {
                p.clone()
            } else {
                original.clone()
            }
        }
    };
    let k = k_of(&video);

    let mut h = blake3::Hasher::new();
    let mut put = |x: f64| {
        h.update(&x.to_le_bytes());
    };
    for x in [params.anchor as f64, params.direction as u8 as f64, params.feature as f64, params.search as f64, params.adapt as f64, params.min_score as f64] {
        put(x);
    }
    for x in [lo as f64, hi as f64, video.width as f64, video.height as f64, video.frames.len() as f64] {
        put(x);
    }
    for b in &guide_boxes {
        b.iter().for_each(|x| put(*x));
    }
    for m in &maps {
        [m.a, m.b[0], m.b[1]].iter().for_each(|x| put(*x));
    }
    h.update(video.path.to_string_lossy().as_bytes());
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
