//! Tracker jobs (DESIGN §6: Job-cost operators). A worker thread decodes the
//! frames a tracker needs with its own ffmpeg, resamples each through the
//! tracker's view, tracks, and sends the results back in chunks.
//!
//! - Forward jobs read the video in order.
//! - Backward jobs decode it in keyframe-aligned segments, keep the patches
//!   (small: only the guide's region; at most [`MAX_PATCHES`] at a time), and
//!   track each segment in reverse. Any strategy runs backward this way,
//!   however long the source's GOPs.
//! - Both stop at a *limit* the runner moves (catch-up-to-playhead mode) and
//!   stop at once when cancelled (an input changed; the runner restarts them).
//!   Held at the limit for a while, a job lets go of its decoder (*parked*).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tt_core::time::FrameIndex;
use tt_core::view::SpaceMap;
use tt_media::{DecodeOptions, FrameStream, VideoIndex};

use crate::image::{Grid, Luma, Patch, resample_xy};
use crate::ncc::best_match;
use crate::template::{LookTemplate, Settings, TEMPLATE_R, TemplateTracker};
use crate::{LOST, OUTSIDE, TRACK_CHANNELS};

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
/// Largest patch half-size, patch pixels.
const MAX_HALF: f64 = 200.0;
/// Results are sent every this many frames or this often.
const FLUSH_FRAMES: usize = 8;
const FLUSH_EVERY: Duration = Duration::from_millis(40);

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
}

/// State shared between a job and the runner.
#[derive(Debug)]
pub struct Shared {
    pub cancel: AtomicBool,
    /// Catch-up limit: forward jobs track frames before it, backward jobs frames at or after it.
    pub limit: AtomicI64,
    /// The last frame produced.
    pub at: AtomicI64,
    /// Waiting at the limit.
    pub waiting: AtomicBool,
    /// Waiting long enough to have let go of its decoder: not using a job slot.
    pub parked: AtomicBool,
}

impl Shared {
    pub fn new(limit: FrameIndex, at: FrameIndex) -> Self {
        Self {
            cancel: AtomicBool::new(false),
            limit: AtomicI64::new(limit),
            at: AtomicI64::new(at),
            waiting: AtomicBool::new(false),
            parked: AtomicBool::new(false),
        }
    }
}

pub enum Msg {
    Frames(Vec<(FrameIndex, [f32; TRACK_CHANNELS])>),
    Finished,
    Failed(String),
}

/// Counted in `threads` until the thread exits (cancelled jobs included:
/// they hold an ffmpeg until they notice).
struct Alive(Arc<AtomicUsize>);

impl Drop for Alive {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Start a job's thread, counted in `threads` while it lives.
pub fn spawn(spec: JobSpec, shared: Arc<Shared>, tx: Sender<Msg>, threads: &Arc<AtomicUsize>) -> JoinHandle<()> {
    threads.fetch_add(1, Ordering::Relaxed);
    let alive = Alive(threads.clone());
    std::thread::Builder::new()
        .name(format!("tracker {:?}", spec.side))
        .spawn(move || {
            let _alive = alive;
            let mut worker = Worker { spec, shared, tx: tx.clone(), out: Vec::new(), flushed: Instant::now(), half: [TEMPLATE_R as f64; 2], margin: TEMPLATE_R as f64 + 2.0, offsets: Vec::new() };
            let result = worker.run();
            worker.flush();
            let _ = tx.send(match result {
                Ok(()) => Msg::Finished,
                Err(e) => Msg::Failed(format!("{e:#}")),
            });
        })
        .expect("spawn tracker thread")
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

    /// One frame: pinned where a look says, else tracked in its patch.
    fn track(&mut self, tracker: &mut TemplateTracker, f: FrameIndex, grid: Grid, patch: &Patch) {
        let guide = self.guide_point(f);
        match self.pin(f) {
            Some(c) => {
                tracker.pin(c, guide);
                self.emit(f, c, 1.0, false);
            }
            None => {
                let step = tracker.step(patch, grid, guide);
                self.emit(f, step.pos, step.score, step.lost);
            }
        }
    }

    /// The guide's point on frame `f`, view px.
    fn guide_point(&self, f: FrameIndex) -> [f64; 2] {
        let g = self.guide(f);
        self.map(f).from_source([g[0], g[1]])
    }

    /// The patch tracked on frame `f`: the guide's box (× `search`) around its point, plus the template's margin.
    fn patch(&self, frame: &[u8], f: FrameIndex) -> (Grid, Patch) {
        let (map, s) = (self.map(f), &self.spec);
        let b = map.box_from_source(self.guide(f));
        let margin = self.margin;
        let half = |h: f64| (h * s.search * s.scale + margin).clamp(margin + 16.0, MAX_HALF + margin);
        let (hw, hh) = (half((b[0] - b[2]).max(b[4] - b[0])), half((b[1] - b[3]).max(b[5] - b[1])));
        let (w, h) = ((2.0 * hw).ceil() as usize, (2.0 * hh).ceil() as usize);
        let grid = Grid { origin: [b[0] - w as f64 / 2.0 / s.scale, b[1] - h as f64 / 2.0 / s.scale], scale: s.scale };
        let (vw, vh) = (s.video.width as usize, s.video.height as usize);
        let luma = Luma { data: &frame[..vw * vh], width: vw, height: vh };
        (grid, resample_xy(&luma, s.k, map, grid, w, h))
    }

    /// A patch just big enough for a template of half-size `r`, around view point `c` on frame `f`.
    fn patch_around(&self, frame: &[u8], f: FrameIndex, c: [f64; 2], r: [usize; 2]) -> (Grid, Patch) {
        let (map, s) = (self.map(f), &self.spec);
        let (w, h) = (2 * (r[0] + 4) + 1, 2 * (r[1] + 4) + 1);
        let grid = Grid { origin: [c[0] - w as f64 / 2.0 / s.scale, c[1] - h as f64 / 2.0 / s.scale], scale: s.scale };
        let (vw, vh) = (s.video.width as usize, s.video.height as usize);
        let luma = Luma { data: &frame[..vw * vh], width: vw, height: vh };
        (grid, resample_xy(&luma, s.k, map, grid, w, h))
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
            let Some(mut t) = LookTemplate::cut(&patch, grid.from_view(c), r, look.mask.clone()) else {
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
        let map = self.map(f);
        let [x, y] = map.to_source(pos);
        let [hx, hy] = self.half.map(|h| (h + 0.5) / self.spec.scale * map.a);
        let g = self.guide(f);
        let outside = !(g[2]..=g[4]).contains(&x) || !(g[3]..=g[5]).contains(&y);
        let flags = if lost { LOST } else { 0 } | if outside { OUTSIDE } else { 0 };
        self.out.push((f, [x, y, x - hx, y - hy, x + hx, y + hy, score as f64, flags as f64].map(|v| v as f32)));
        self.shared.at.store(f, Ordering::Relaxed);
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
    fn wait_for(&mut self, f: FrameIndex, mut park: impl FnMut()) -> bool {
        let mut since: Option<Instant> = None;
        loop {
            if self.cancelled() {
                return false;
            }
            let limit = self.shared.limit.load(Ordering::Relaxed);
            let allowed = match self.spec.side {
                Side::Forward => f < limit,
                Side::Backward => f >= limit,
            };
            if allowed {
                self.shared.waiting.store(false, Ordering::Relaxed);
                self.shared.parked.store(false, Ordering::Relaxed);
                return true;
            }
            if !self.shared.waiting.swap(true, Ordering::Relaxed) {
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
                let (grid, patch) = self.patch(&frame, f);
                let pos = self.map(f).from_source(src);
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
        if self.cancelled() {
            return Ok(());
        }
        match self.spec.side {
            Side::Forward => self.forward(&mut tracker, start),
            Side::Backward => self.backward(&mut tracker, start),
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
            let (grid, patch) = self.patch(&buf, f);
            self.track(tracker, f, grid, &patch);
        }
        Ok(())
    }

    fn backward(&mut self, tracker: &mut TemplateTracker, start: FrameIndex) -> Result<()> {
        let to = self.spec.to;
        let mut hi = start;
        while hi >= to {
            // A segment of at least SEGMENT frames that starts on a keyframe,
            // or the last MAX_PATCHES frames before `hi` (ffmpeg skips to them).
            let want = (hi - SEGMENT + 1).max(to);
            let key = self.spec.video.group_start(self.presented(want));
            let lo = self.spec.grid.grid_of.get(key).copied().unwrap_or(want).clamp(to, want).max(hi + 1 - MAX_PATCHES);
            let mut stream = FrameStream::start(&self.spec.video, self.presented(lo), &self.spec.decode)?;
            let (mut held, mut buf) = (None, Vec::new());
            let mut patches = Vec::with_capacity((hi - lo + 1) as usize);
            for f in lo..=hi {
                if self.cancelled() {
                    return Ok(());
                }
                self.read_to(&mut stream, &mut held, &mut buf, f)?;
                patches.push(self.patch(&buf, f));
            }
            drop(stream);
            for f in (lo..=hi).rev() {
                if !self.wait_for(f, || {}) {
                    return Ok(());
                }
                let (grid, patch) = &patches[(f - lo) as usize];
                self.track(tracker, f, *grid, patch);
            }
            hi = lo - 1;
        }
        Ok(())
    }
}
