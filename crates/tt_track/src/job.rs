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

use crate::TRACK_CHANNELS;
use crate::image::{Grid, Luma, Patch, resample};
use crate::template::{Settings, TEMPLATE_R, TemplateTracker};

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
            let mut worker = Worker { spec, shared, tx: tx.clone(), out: Vec::new(), flushed: Instant::now() };
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

    /// The guide's point on frame `f`, view px.
    fn guide_point(&self, f: FrameIndex) -> [f64; 2] {
        let g = self.guide(f);
        self.map(f).from_source([g[0], g[1]])
    }

    /// The patch tracked on frame `f`: the guide's box (× `search`) around its point, plus the template's margin.
    fn patch(&self, frame: &[u8], f: FrameIndex) -> (Grid, Patch) {
        let (map, s) = (self.map(f), &self.spec);
        let b = map.box_from_source(self.guide(f));
        let margin = TEMPLATE_R as f64 + 2.0;
        let half = |h: f64| (h * s.search * s.scale + margin).clamp(margin + 16.0, MAX_HALF);
        let (hw, hh) = (half((b[0] - b[2]).max(b[4] - b[0])), half((b[1] - b[3]).max(b[5] - b[1])));
        let (w, h) = ((2.0 * hw).ceil() as usize, (2.0 * hh).ceil() as usize);
        let grid = Grid { origin: [b[0] - w as f64 / 2.0 / s.scale, b[1] - h as f64 / 2.0 / s.scale], scale: s.scale };
        let (vw, vh) = (s.video.width as usize, s.video.height as usize);
        let luma = Luma { data: &frame[..vw * vh], width: vw, height: vh };
        (grid, resample(&luma, s.k, map, grid, w, h))
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

    fn emit(&mut self, f: FrameIndex, pos: [f64; 2], score: f32) {
        let map = self.map(f);
        let [x, y] = map.to_source(pos);
        let half = (TEMPLATE_R as f64 + 0.5) / self.spec.scale * map.a;
        self.out.push((f, [x, y, x - half, y - half, x + half, y + half, score as f64].map(|v| v as f32)));
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

        // Seed on the anchor frame: its appearance defines the tracked point.
        let seed = self.guide_point(anchor);
        let frame = self.decode_one(anchor)?;
        if self.cancelled() {
            return Ok(());
        }
        let mut tracker = {
            let (grid, patch) = self.patch(&frame, anchor);
            TemplateTracker::seed(&patch, grid, seed, settings)
                .context("the guide's point at the anchor frame has no detail to follow (flat)")?
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
                    self.emit(anchor, seed, 1.0);
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
            let step = tracker.step(&patch, grid, self.guide_point(f));
            self.emit(f, step.pos, step.score);
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
                let step = tracker.step(patch, *grid, self.guide_point(f));
                self.emit(f, step.pos, step.score);
            }
            hi = lo - 1;
        }
        Ok(())
    }
}
