//! Tracker jobs (DESIGN §6: Job-cost operators). A worker thread decodes the
//! frames a tracker needs with its own ffmpeg, resamples each through the
//! tracker's view, tracks, and sends the results back in chunks.
//!
//! - Forward jobs read the video in order.
//! - Backward jobs decode it in keyframe-aligned segments, keep the patches
//!   (small: only the guide's region; at most [`MAX_PATCHES`] at a time), and
//!   track each segment in reverse. Any strategy runs backward this way,
//!   however long the source's GOPs.
//! - Both track each stretch between two pins (looks' frames) from both
//!   ends: they keep its patches, and on reaching the next pin track the
//!   stretch back from it and send again the frames where the other pass
//!   was better (`template::fuse`; up to [`MAX_STRETCH_BYTES`] of patches).
//! - Both stop at a *limit* the runner moves (catch-up-to-playhead mode) and
//!   stop at once when cancelled (an input changed; the runner restarts them).
//!   Held at the limit for a while, a job lets go of its decoder (*parked*).
//! - A tracker with no guide (its guide is the whole frame) searches a patch
//!   around where it was going instead of around the guide's boxes. Going
//!   backward, that is only known while tracking, so a segment keeps its
//!   decoded frames (up to [`ROOT_SEGMENT_BYTES`]) instead of its patches.
//! - Each job says what it is doing ([`Phase`]): starting, loading its model
//!   (CoTracker's Python worker), or tracking.
//! - Each job ends with one message: finished, or failed (a panic in its
//!   thread too).

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tt_core::time::FrameIndex;
use tt_core::view::SpaceMap;
use tt_media::{DecodeOptions, FrameStream, VideoIndex};

use crate::image::{Grid, Luma, Patch, chroma_planes, resample_xy, with_colour};
use crate::ncc::best_match;
use crate::template::{Estimate, LookTemplate, Settings, TEMPLATE_R, TemplateTracker, fuse, off_box};
use crate::{LOST, Method, OUTSIDE, TRACK_CHANNELS};

mod learned;
pub mod paint;

pub use learned::{
    availability as cotracker_availability, forget_availability as forget_cotracker_availability, installed_dir as cotracker_dir, installed_python as cotracker_python,
    installed_weights as cotracker_weights, installed_worker as cotracker_worker, weights as cotracker_model,
};

pub use learned::{tapnext_availability, tapnext_weights, worker_command};
pub use learned::{
    Engine as CoTrackerEngine, close_worker as close_cotracker_worker, engine as cotracker_engine, keep_warm as keep_cotracker_warm, processes_started as cotracker_processes_started,
    warm_up as warm_up_cotracker,
};

/// A look, as a job reads it: a frame, a rectangle there (source px), a mask.
#[derive(Clone, Debug, PartialEq)]
pub struct LookSpec {
    pub frame: FrameIndex,
    pub center: [f64; 2],
    pub half: [f64; 2],
    pub mask: Option<Vec<u8>>,
}

/// The look on frame `f` that counts there (a painted one first), if any.
pub fn look_on(looks: &[LookSpec], f: FrameIndex) -> Option<usize> {
    let on = || looks.iter().enumerate().filter(|(_, l)| l.frame == f);
    on().find(|(_, l)| l.mask.is_some()).or_else(|| on().next()).map(|(i, _)| i)
}

/// A look matching another this well on its frame is aligned to it.
const ALIGN_SCORE: f32 = 0.75;

/// Which way a job runs from the anchor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    Forward,
    Backward,
}

/// Frames decoded per backward segment, at least (then back to a keyframe).
const SEGMENT: FrameIndex = 64;
/// Patches a backward job keeps at once: a longer GOP is decoded again from
/// its keyframe for each next batch, so memory doesn't grow with the GOP.
pub const MAX_PATCHES: FrameIndex = 256;
/// Held at the limit this long, a job closes its decoder (reopened on resume).
const PARK_AFTER: Duration = Duration::from_millis(1000);
/// A patch covers the guide's boxes this many frames either side.
const NEIGHBOURS: FrameIndex = 8;
/// Largest patch half-size, patch pixels.
const MAX_HALF: f64 = 200.0;
/// Patches kept for tracking a stretch back from its far pin; a longer
/// stretch is tracked one way only.
pub const MAX_STRETCH_BYTES: usize = 256 << 20;
/// Tracking a stretch back stops after this many frames in a row where it
/// agrees with the first pass (within half a view pixel).
const CONVERGED: usize = 12;
/// Results are sent every this many frames or this often.
const FLUSH_FRAMES: usize = 8;
const FLUSH_EVERY: Duration = Duration::from_millis(40);
/// Decoded frames a backward job without a guide keeps at once (it cuts
/// each frame's patch only while tracking it).
pub const ROOT_SEGMENT_BYTES: usize = 256 << 20;

/// Everything a job needs, copied out of the world when it starts.
pub struct JobSpec {
    pub side: Side,
    pub anchor: FrameIndex,
    /// Frames to produce: from `from` (nearest the anchor) to `to` (the far
    /// end, inclusive). A forward job starting at the anchor emits the anchor.
    pub from: FrameIndex,
    pub to: FrameIndex,
    /// Where the tracker was (source px) on the frame before `from` in the
    /// job's direction, and its score there: resume there instead of
    /// starting at the anchor.
    pub resume: Option<([f64; 2], f32)>,
    /// The tracked range starts here; `guide` and `maps` cover it frame by frame.
    pub lo: FrameIndex,
    /// The guide's boxes `[x, y, left, top, right, bottom]`, source px, gaps filled.
    pub guide: Arc<Vec<[f64; 6]>>,
    /// The tracker's view, per frame (view → source).
    pub maps: Arc<Vec<SpaceMap>>,
    /// Patch pixels per view pixel.
    pub scale: f64,
    /// Searched region, as a multiple of the guide's box.
    pub search: f64,
    pub settings: Settings,
    /// The rendition decoded, and its size relative to the source per axis.
    pub video: Arc<VideoIndex>,
    pub k: [f64; 2],
    /// The source's index (the frame grid).
    pub grid: Arc<VideoIndex>,
    pub decode: DecodeOptions,
    /// What the subject looks like; none: a square around the guide's point.
    pub looks: Arc<Vec<LookSpec>>,
    /// Where the tracker starts on the anchor frame (source px); none: the guide's point.
    pub seed: Option<[f64; 2]>,
    /// Track each stretch between pins from both ends ([`template::fuse`](crate::template::fuse)).
    pub fuse: bool,
    /// Templates, or a learned point tracker ([`learned`]).
    pub method: Method,
    /// No guide: `guide` is the whole frame, and the search goes around where
    /// the tracker was going (its patch follows it).
    pub root: bool,
    /// Which tracker it is, for the log ("Tracker 2 (14v1)").
    pub label: String,
}

/// What a job is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Phase {
    /// Decoding its first frames, cutting its looks.
    #[default]
    Starting,
    /// CoTracker's Python worker starting up and loading its model onto the card.
    Loading,
    /// Tracking frames.
    Tracking,
}

/// State shared between a job and the runner.
#[derive(Debug)]
pub struct Shared {
    pub cancel: AtomicBool,
    /// Catch-up limit: forward jobs track frames before it, backward jobs frames at or after it.
    pub limit: AtomicI64,
    /// The last frame produced.
    pub at: AtomicI64,
    /// The frame it waits for at the limit (or last waited for).
    pub next: AtomicI64,
    /// Waiting at the limit.
    pub waiting: AtomicBool,
    /// Waiting long enough to have let go of its decoder: not using a job slot.
    pub parked: AtomicBool,
    /// Its [`Phase`].
    phase: AtomicU8,
}

impl Shared {
    pub fn new(limit: FrameIndex, at: FrameIndex) -> Self {
        Self {
            cancel: AtomicBool::new(false),
            limit: AtomicI64::new(limit),
            at: AtomicI64::new(at),
            next: AtomicI64::new(at),
            waiting: AtomicBool::new(false),
            parked: AtomicBool::new(false),
            phase: AtomicU8::new(Phase::Starting as u8),
        }
    }

    pub fn phase(&self) -> Phase {
        match self.phase.load(Ordering::Relaxed) {
            1 => Phase::Loading,
            2 => Phase::Tracking,
            _ => Phase::Starting,
        }
    }

    pub fn set_phase(&self, phase: Phase) {
        self.phase.store(phase as u8, Ordering::Relaxed);
    }

    /// Whether the limit lets a job on `side` track frame `f`.
    pub fn allows(&self, side: Side, f: FrameIndex) -> bool {
        let limit = self.limit.load(Ordering::Relaxed);
        match side {
            Side::Forward => f < limit,
            Side::Backward => f >= limit,
        }
    }

    /// Parked, and its limit still holds it there. (`parked` alone can be
    /// stale: a new limit may have freed it a moment ago, before it looked.)
    pub fn held(&self, side: Side) -> bool {
        self.parked.load(Ordering::Relaxed) && !self.allows(side, self.next.load(Ordering::Relaxed))
    }
}

pub enum Msg {
    Frames(Vec<(FrameIndex, [f32; TRACK_CHANNELS])>),
    Finished,
    Failed(String),
}

/// Counted in each of its counters until the thread exits (cancelled jobs
/// included: they hold an ffmpeg, or a CoTracker worker, until they notice).
struct Alive(Vec<Arc<AtomicUsize>>);

impl Drop for Alive {
    fn drop(&mut self) {
        for count in &self.0 {
            count.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// Start a job's thread, counted in `threads` while it lives, and in
/// `workers` too if it runs a CoTracker worker ([`JobSpec::starts_worker`]).
/// The thread always ends with [`Msg::Finished`] or [`Msg::Failed`] (a
/// panic in it too).
pub fn spawn(spec: JobSpec, shared: Arc<Shared>, tx: Sender<Msg>, threads: &Arc<AtomicUsize>, workers: &Arc<AtomicUsize>) -> JoinHandle<()> {
    let mut counts = vec![threads.clone()];
    if spec.starts_worker() {
        counts.push(workers.clone());
    }
    for count in &counts {
        count.fetch_add(1, Ordering::Relaxed);
    }
    let alive = Alive(counts);
    std::thread::Builder::new()
        .name(format!("tracker {:?}", spec.side))
        .spawn(move || {
            let _alive = alive;
            let out = tx.clone();
            // (Whatever it held is dropped while unwinding: a CoTracker worker is stopped.)
            let result = std::panic::catch_unwind(AssertUnwindSafe(move || {
                let mut worker = Worker {
                    spec,
                    shared,
                    tx: out,
                    out: Vec::new(),
                    flushed: Instant::now(),
                    half: [TEMPLATE_R as f64; 2],
                    margin: TEMPLATE_R as f64 + 2.0,
                    offsets: Vec::new(),
                    stretch: Vec::new(),
                    stretch_bytes: Some(0),
                    near: None,
                    paint: None,
                };
                let result = match worker.spec.method {
                    Method::Template => worker.run(),
                    Method::CoTracker | Method::Paint | Method::TapNext => worker.run_learned(),
                    // (A manual dot is never planned: nothing to track.)
                    Method::Manual => Ok(()),
                };
                worker.flush();
                result
            }));
            let _ = tx.send(match result {
                Ok(Ok(())) => Msg::Finished,
                Ok(Err(e)) => Msg::Failed(format!("{e:#}")),
                Err(panic) => Msg::Failed(format!("the tracker stopped on an error: {}", panic_message(panic.as_ref()))),
            });
        })
        .expect("spawn tracker thread")
}

/// What a panic said.
fn panic_message(panic: &(dyn std::any::Any + Send)) -> &str {
    panic.downcast_ref::<&str>().copied().or_else(|| panic.downcast_ref::<String>().map(String::as_str)).unwrap_or("no message")
}

struct Worker {
    spec: JobSpec,
    shared: Arc<Shared>,
    tx: Sender<Msg>,
    out: Vec<(FrameIndex, [f32; TRACK_CHANNELS])>,
    flushed: Instant,
    /// The output box's half-size, and the patch margin the templates need (patch px).
    half: [f64; 2],
    margin: f64,
    /// Per look (the spec's order): its centre minus the tracked point, view px.
    offsets: Vec<[f64; 2]>,
    /// The frames tracked since the last pin, with their patches and
    /// estimates (to track back from the next pin); `stretch_bytes` counts
    /// their patches, and None = over [`MAX_STRETCH_BYTES`] (not kept).
    stretch: Vec<(FrameIndex, Grid, Patch, Estimate)>,
    stretch_bytes: Option<usize>,
    /// With no guide: where the next patch is centred (view px), where the
    /// tracker was going.
    near: Option<[f64; 2]>,
    /// A paint tracker's points and motion (`paint`).
    paint: Option<paint::PaintFit>,
}

impl Worker {
    fn cancelled(&self) -> bool {
        self.shared.cancel.load(Ordering::Relaxed)
    }

    fn presented(&self, f: FrameIndex) -> usize {
        self.spec.grid.presented_at(f)
    }

    fn map(&self, f: FrameIndex) -> &SpaceMap {
        &self.spec.maps[(f - self.spec.lo) as usize]
    }

    fn guide(&self, f: FrameIndex) -> [f64; 6] {
        self.spec.guide[(f - self.spec.lo) as usize]
    }

    /// Where the user showed the subject on frame `f` (a look there, a
    /// painted one first: the tracked point on it, view px), if they did.
    fn pin(&self, f: FrameIndex) -> Option<[f64; 2]> {
        let i = look_on(&self.spec.looks, f)?;
        let c = self.map(f).from_source(self.spec.looks[i].center);
        let o = self.offsets.get(i).copied().unwrap_or_default();
        Some([c[0] - o[0], c[1] - o[1]])
    }

    /// One frame: pinned where a look says, else tracked in its patch. On a
    /// pin, the stretch since the last one is tracked back from it too.
    fn track(&mut self, tracker: &mut TemplateTracker, f: FrameIndex, grid: Grid, patch: &Patch) {
        let guide = self.guide_point(f);
        match self.pin(f) {
            Some(c) => {
                tracker.pin(c, guide);
                self.emit(f, c, 1.0, false);
                if self.spec.fuse {
                    self.track_back(tracker);
                }
                (self.stretch, self.stretch_bytes) = (Vec::new(), Some(0));
            }
            None => {
                let step = tracker.step_in(patch, grid, self.guide_box(f));
                self.emit(f, step.pos, step.score, step.lost);
                if self.spec.fuse
                    && let Some(bytes) = self.stretch_bytes
                {
                    let bytes = bytes + patch.bytes();
                    if bytes > MAX_STRETCH_BYTES {
                        (self.stretch, self.stretch_bytes) = (Vec::new(), None);
                    } else {
                        let e = Estimate { pos: step.pos, score: step.score, lost: step.lost, off: off_box(step.pos, &self.guide_box(f)) };
                        self.stretch.push((f, grid, patch.clone(), e));
                        self.stretch_bytes = Some(bytes);
                    }
                }
            }
        }
    }

    /// The stretch kept since the last pin, tracked back from the pin just
    /// reached (`tracker`, as it stands there) and fused with the first
    /// pass: frames that change are sent again.
    fn track_back(&mut self, tracker: &TemplateTracker) {
        let stretch = std::mem::take(&mut self.stretch);
        if stretch.is_empty() {
            return;
        }
        let mut back = tracker.clone();
        let mut other: Vec<Estimate> = Vec::with_capacity(stretch.len());
        // Once both passes agree for a while, they follow the same thing:
        // the rest of the way back would be the first pass again.
        let mut agreeing = 0;
        for (f, grid, patch, first) in stretch.iter().rev() {
            if self.cancelled() {
                return;
            }
            if agreeing >= CONVERGED {
                other.push(*first);
                continue;
            }
            let step = back.step_in(patch, *grid, self.guide_box(*f));
            let e = Estimate { pos: step.pos, score: step.score, lost: step.lost, off: off_box(step.pos, &self.guide_box(*f)) };
            let same = !e.lost && !first.lost && (e.pos[0] - first.pos[0]).hypot(e.pos[1] - first.pos[1]) <= 0.5;
            agreeing = if same { agreeing + 1 } else { 0 };
            other.push(e);
        }
        other.reverse();
        let first: Vec<Estimate> = stretch.iter().map(|(_, _, _, e)| *e).collect();
        for ((f, _, _, a), e) in stretch.iter().zip(fuse(&first, &other)) {
            if e != *a {
                self.emit_again(*f, e.pos, e.score, e.lost);
            }
        }
    }

    /// The guide's box on frame `f`, view px.
    fn guide_box(&self, f: FrameIndex) -> [f64; 6] {
        self.map(f).box_from_source(self.guide(f))
    }

    /// Whether view point `pos` on frame `f` is outside the guide's box.
    fn outside(&self, f: FrameIndex, pos: [f64; 2]) -> bool {
        let [x, y] = self.map(f).to_source(pos);
        let g = self.guide(f);
        !(g[2]..=g[4]).contains(&x) || !(g[3]..=g[5]).contains(&y)
    }

    /// The guide's point on frame `f`, view px.
    fn guide_point(&self, f: FrameIndex) -> [f64; 2] {
        let g = self.guide(f);
        self.map(f).from_source([g[0], g[1]])
    }

    /// The patch tracked on frame `f`: the guide's boxes (× `search`) on the
    /// frames around it, plus the template's margin. A rough pass is late or
    /// early by a few frames where the subject starts or stops (a sketch's
    /// smoothing can't follow a flick), so the subject on frame `f` is inside
    /// the guide's box of some frame near `f`, not always of `f` itself.
    fn patch(&self, frame: &[u8], f: FrameIndex) -> (Grid, Patch) {
        let (map, s) = (self.map(f), &self.spec);
        let margin = self.margin;
        let (c, hw, hh) = if s.root {
            // No guide: as big a patch as any, around where the tracker was going.
            (self.near.unwrap_or_else(|| self.guide_point(f)), MAX_HALF + margin, MAX_HALF + margin)
        } else {
            let last = s.lo + s.guide.len() as FrameIndex - 1;
            let (mut lo, mut hi) = ([f64::INFINITY; 2], [f64::NEG_INFINITY; 2]);
            for g in (f - NEIGHBOURS).max(s.lo)..=(f + NEIGHBOURS).min(last) {
                let b = map.box_from_source(self.guide(g));
                let (hw, hh) = ((b[0] - b[2]).max(b[4] - b[0]) * s.search, (b[1] - b[3]).max(b[5] - b[1]) * s.search);
                lo = [lo[0].min(b[0] - hw), lo[1].min(b[1] - hh)];
                hi = [hi[0].max(b[0] + hw), hi[1].max(b[1] + hh)];
            }
            let half = |h: f64| (h * s.scale + margin).clamp(margin + 16.0, MAX_HALF + margin);
            let (hw, hh) = (half((hi[0] - lo[0]) / 2.0), half((hi[1] - lo[1]) / 2.0));
            // Centred on the neighbours' boxes; where that is too big, on this frame's point.
            let b = map.box_from_source(self.guide(f));
            let centre = |i: usize, h: f64| if h < MAX_HALF + margin { (lo[i] + hi[i]) / 2.0 } else { b[i] };
            ([centre(0, hw), centre(1, hh)], hw, hh)
        };
        let (w, h) = ((2.0 * hw).ceil() as usize, (2.0 * hh).ceil() as usize);
        let grid = Grid { origin: [c[0] - w as f64 / 2.0 / s.scale, c[1] - h as f64 / 2.0 / s.scale], scale: s.scale };
        let (vw, vh) = (s.video.width as usize, s.video.height as usize);
        let luma = Luma { data: &frame[..vw * vh], width: vw, height: vh };
        (grid, self.dress(frame, map, grid, resample_xy(&luma, s.k, map, grid, w, h)))
    }

    /// `patch` (luma, resampled from `frame` on `grid`) with what the
    /// tracker's tolerance also compares: the colour.
    fn dress(&self, frame: &[u8], map: &SpaceMap, grid: Grid, patch: Patch) -> Patch {
        let (s, tolerance) = (&self.spec, self.spec.settings.tolerance);
        let (vw, vh) = (s.video.width as usize, s.video.height as usize);
        match tolerance.colour {
            Some(_) => with_colour(patch, &chroma_planes(frame, vw, vh), vw, vh, s.k, map, grid),
            None => patch,
        }
    }

    /// A patch just big enough for a template of half-size `r`, around view point `c` on frame `f`.
    fn patch_around(&self, frame: &[u8], f: FrameIndex, c: [f64; 2], r: [usize; 2]) -> (Grid, Patch) {
        let (map, s) = (self.map(f), &self.spec);
        let (w, h) = (2 * (r[0] + 4) + 1, 2 * (r[1] + 4) + 1);
        let grid = Grid { origin: [c[0] - w as f64 / 2.0 / s.scale, c[1] - h as f64 / 2.0 / s.scale], scale: s.scale };
        let (vw, vh) = (s.video.width as usize, s.video.height as usize);
        let luma = Luma { data: &frame[..vw * vh], width: vw, height: vh };
        (grid, self.dress(frame, map, grid, resample_xy(&luma, s.k, map, grid, w, h)))
    }

    /// The looks' templates, each cut from its own frame through the view,
    /// and aligned on one point. The seed look (on the anchor) defines it;
    /// every other look, nearest the anchor first, is matched on its own
    /// frame (within its own half-size of where the user put it) against
    /// the looks aligned before it. Where one matches well, the look takes
    /// that point: looks dragged a little differently around the same
    /// cursor all report the same point on it. A look nothing matches
    /// (another icon) keeps its own centre.
    fn look_templates(&mut self) -> Result<Vec<LookTemplate>> {
        let looks = self.spec.looks.clone();
        let anchor = self.spec.anchor;
        let seed = look_on(&looks, anchor).unwrap_or(0);
        let mut order: Vec<usize> = (0..looks.len()).collect();
        order.sort_by_key(|i| (*i != seed, (looks[*i].frame - anchor).abs(), *i));
        let mut frames: Vec<(FrameIndex, Vec<u8>)> = Vec::new();
        let mut out: Vec<(usize, LookTemplate)> = Vec::new();
        self.offsets = vec![[0.0, 0.0]; looks.len()];
        for i in order {
            let look = &looks[i];
            if !frames.iter().any(|(f, _)| *f == look.frame) {
                let frame = self.decode_one(look.frame)?;
                frames.push((look.frame, frame));
            }
            if self.cancelled() {
                return Ok(Vec::new());
            }
            let frame = &frames.iter().find(|(f, _)| *f == look.frame).expect("decoded").1;
            let map = self.map(look.frame);
            let c = map.from_source(look.center);
            let r = [look.half[0], look.half[1]].map(|h| ((h / map.a * self.spec.scale).round() as usize).max(2));
            let reach = r[0].max(r[1]);
            let (grid, patch) = self.patch_around(frame, look.frame, c, [r[0] + reach, r[1] + reach]);
            let Some(mut t) = LookTemplate::cut(&patch, grid.from_view(c), r, look.mask.clone(), self.spec.settings.tolerance) else {
                tracing::warn!("a look on frame {} has no detail to follow (flat or an empty mask); skipped", look.frame);
                continue;
            };
            let at = grid.from_view(c);
            let window = [[at[0] - reach as f64, at[1] - reach as f64], [at[0] + reach as f64, at[1] + reach as f64]];
            let aligned = out
                .iter()
                .filter_map(|(_, a)| best_match(&patch, &a.template, window, None).map(|m| (m, a.offset)))
                .filter(|(m, _)| m.score >= ALIGN_SCORE)
                .max_by(|a, b| a.0.score.total_cmp(&b.0.score));
            if let Some((m, o)) = aligned {
                // There, the other look's centre is at m (its point at m − o): this look's offset from that point.
                let p = grid.to_view(m.pos);
                t.offset = [c[0] - (p[0] - o[0]), c[1] - (p[1] - o[1])];
            }
            self.offsets[i] = t.offset;
            out.push((i, t));
        }
        out.sort_by_key(|(i, _)| *i);
        Ok(out.into_iter().map(|(_, t)| t).collect())
    }

    fn decode_one(&self, f: FrameIndex) -> Result<Vec<u8>> {
        let mut stream = FrameStream::start(&self.spec.video, self.presented(f), &self.spec.decode)?;
        let mut buf = Vec::new();
        stream.read(&mut buf)?.with_context(|| format!("no frame {f}"))?;
        Ok(buf)
    }

    /// Read `stream` until it holds frame `f` in `buf`.
    fn read_to(&self, stream: &mut FrameStream, held: &mut Option<usize>, buf: &mut Vec<u8>, f: FrameIndex) -> Result<()> {
        let p = self.presented(f);
        while held.is_none_or(|h| h < p) {
            *held = stream.read(buf)?;
            if held.is_none() {
                bail!("the video ended before frame {f}");
            }
        }
        Ok(())
    }

    fn emit(&mut self, f: FrameIndex, pos: [f64; 2], score: f32, lost: bool) {
        self.emit_again(f, pos, score, lost);
        self.shared.at.store(f, Ordering::Relaxed);
    }

    /// Send frame `f`'s result (again, after a fuse: progress stays where it is).
    fn emit_again(&mut self, f: FrameIndex, pos: [f64; 2], score: f32, lost: bool) {
        let map = self.map(f);
        let [x, y] = map.to_source(pos);
        let [hx, hy] = self.half.map(|h| (h + 0.5) / self.spec.scale * map.a);
        let flags = if lost { LOST } else { 0 } | if self.outside(f, pos) { OUTSIDE } else { 0 };
        self.out.push((f, [x, y, x - hx, y - hy, x + hx, y + hy, score as f64, flags as f64].map(|v| v as f32)));
        if self.out.len() >= FLUSH_FRAMES || self.flushed.elapsed() >= FLUSH_EVERY {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if !self.out.is_empty() {
            let _ = self.tx.send(Msg::Frames(std::mem::take(&mut self.out)));
        }
        self.flushed = Instant::now();
    }

    /// Wait until frame `f` is within the limit. False if cancelled. A long
    /// wait calls `park` once (to let go of the decoder) and marks the job parked.
    fn wait_for(&mut self, f: FrameIndex, park: impl FnMut()) -> bool {
        self.wait_while(f, park, |_| true)
    }

    /// [`Self::wait_for`], calling `tick` now and then while it waits (a
    /// CoTracker job takes the results still coming in). `tick` false: stop
    /// waiting (false, as for a cancel).
    fn wait_while(&mut self, f: FrameIndex, mut park: impl FnMut(), mut tick: impl FnMut(&mut Self) -> bool) -> bool {
        let mut since: Option<Instant> = None;
        self.shared.next.store(f, Ordering::Relaxed);
        loop {
            if self.cancelled() {
                return false;
            }
            if self.shared.allows(self.spec.side, f) {
                self.shared.waiting.store(false, Ordering::Relaxed);
                self.shared.parked.store(false, Ordering::Relaxed);
                return true;
            }
            if !tick(self) {
                return false;
            }
            if !self.shared.waiting.swap(true, Ordering::Relaxed) || !self.out.is_empty() {
                self.flush();
            }
            let since = *since.get_or_insert_with(Instant::now);
            if since.elapsed() >= PARK_AFTER && !self.shared.parked.load(Ordering::Relaxed) {
                park();
                self.shared.parked.store(true, Ordering::Relaxed);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn run(&mut self) -> Result<()> {
        let s = &self.spec;
        let (anchor, settings) = (s.anchor, s.settings);
        let dir: FrameIndex = if s.side == Side::Forward { 1 } else { -1 };

        // Seed on the anchor frame: the user's look there, or the guide's point.
        let guide_at = self.guide_point(anchor);
        let seed = self.spec.seed.map_or(guide_at, |s| self.map(anchor).from_source(s));
        let mut tracker = if self.spec.looks.is_empty() {
            let frame = self.decode_one(anchor)?;
            if self.cancelled() {
                return Ok(());
            }
            self.near = Some(seed);
            let (grid, patch) = self.patch(&frame, anchor);
            TemplateTracker::seed(&patch, grid, seed, settings).context("the guide's point at the anchor frame has no detail to follow (flat)")?
        } else {
            let looks = self.look_templates()?;
            if self.cancelled() {
                return Ok(());
            }
            let r = looks.first().map_or([TEMPLATE_R; 2], |l| l.r);
            self.half = r.map(|v| v as f64);
            self.margin = looks.iter().map(|l| l.r[0].max(l.r[1])).max().unwrap_or(TEMPLATE_R) as f64 + 2.0;
            TemplateTracker::with_looks(looks, [seed[0] - guide_at[0], seed[1] - guide_at[1]], settings)
                .context("no look has detail to follow (flat, or an empty mask)")?
        };
        let start = match self.spec.resume {
            Some((src, score)) => {
                let f = self.spec.from - dir;
                let frame = self.decode_one(f)?;
                if self.cancelled() {
                    return Ok(());
                }
                let pos = self.map(f).from_source(src);
                self.near = Some(pos);
                let (grid, patch) = self.patch(&frame, f);
                tracker.resume(&patch, grid, pos, self.guide_point(f), score);
                self.spec.from
            }
            None if self.spec.side == Side::Forward => {
                if self.wait_for(anchor, || {}) {
                    self.emit(anchor, seed, 1.0, false);
                }
                anchor + 1
            }
            None => anchor - 1,
        };
        if self.spec.resume.is_none() {
            tracker.start_at(seed);
        }
        if self.cancelled() {
            return Ok(());
        }
        self.shared.set_phase(Phase::Tracking);
        match self.spec.side {
            Side::Forward => self.forward(&mut tracker, start),
            Side::Backward => self.backward(&mut tracker, start),
        }
    }

    /// With no guide, the patch for frame `f` goes around where `tracker` was going.
    fn follow(&mut self, tracker: &TemplateTracker, f: FrameIndex) {
        if self.spec.root {
            self.near = Some(tracker.expected(self.guide_point(f)));
        }
    }

    fn forward(&mut self, tracker: &mut TemplateTracker, start: FrameIndex) -> Result<()> {
        let to = self.spec.to;
        let mut stream: Option<FrameStream> = None;
        let (mut held, mut buf) = (None, Vec::new());
        for f in start..=to {
            // Parked at the playhead: the decoder closes, and reopens here.
            if !self.wait_for(f, || stream = None) {
                return Ok(());
            }
            if stream.is_none() {
                held = None;
                stream = Some(FrameStream::start(&self.spec.video, self.presented(f), &self.spec.decode)?);
            }
            self.read_to(stream.as_mut().expect("opened"), &mut held, &mut buf, f)?;
            self.follow(tracker, f);
            let (grid, patch) = self.patch(&buf, f);
            self.track(tracker, f, grid, &patch);
        }
        Ok(())
    }

    fn backward(&mut self, tracker: &mut TemplateTracker, start: FrameIndex) -> Result<()> {
        let to = self.spec.to;
        // Frames kept at once: their patches, or with no guide the decoded
        // frames themselves (the patch goes where the tracker is going, known
        // only while tracking), as many as fit in ROOT_SEGMENT_BYTES.
        let frame_bytes = (self.spec.video.width as usize * self.spec.video.height as usize * 3 / 2).max(1);
        let keep = if self.spec.root { (ROOT_SEGMENT_BYTES / frame_bytes).clamp(4, MAX_PATCHES as usize) as FrameIndex } else { MAX_PATCHES };
        let mut hi = start;
        while hi >= to {
            // A segment of at least SEGMENT frames that starts on a keyframe,
            // or the last `keep` frames before `hi` (ffmpeg skips to them).
            let want = (hi - SEGMENT.min(keep) + 1).max(to);
            let key = self.spec.video.group_start(self.presented(want));
            let lo = self.spec.grid.grid_of.get(key).copied().unwrap_or(want).clamp(to, want).max(hi + 1 - keep);
            let mut stream = FrameStream::start(&self.spec.video, self.presented(lo), &self.spec.decode)?;
            let (mut held, mut buf) = (None, Vec::new());
            let mut patches = Vec::with_capacity((hi - lo + 1) as usize);
            let mut frames: Vec<Vec<u8>> = Vec::new();
            for f in lo..=hi {
                if self.cancelled() {
                    return Ok(());
                }
                self.read_to(&mut stream, &mut held, &mut buf, f)?;
                if self.spec.root {
                    frames.push(buf.clone());
                } else {
                    patches.push(self.patch(&buf, f));
                }
            }
            drop(stream);
            for f in (lo..=hi).rev() {
                if !self.wait_for(f, || {}) {
                    return Ok(());
                }
                let i = (f - lo) as usize;
                if self.spec.root {
                    self.follow(tracker, f);
                    let (grid, patch) = self.patch(&frames[i], f);
                    self.track(tracker, f, grid, &patch);
                } else {
                    let (grid, patch) = &patches[i];
                    self.track(tracker, f, *grid, patch);
                }
            }
            hi = lo - 1;
        }
        Ok(())
    }
}
