//! The CoTracker3 method (DESIGN §6.2): the same job, with a learned point
//! tracker instead of templates. This side does what every method shares:
//! decoding (in the job's direction: a backward job decodes keyframe-aligned
//! segments and sends them reversed), resampling each frame through the
//! tracker's view, the looks and their alignment, pins, the output. The
//! model runs in a Python worker (`editor/cotracker_worker.py`, v1's online
//! engine), fed over a pipe.
//!
//! - **The crop:** 512 × 384 RGB (the model's input), one fixed scale for the
//!   job, centred on the guide's point on every frame: the rough pass
//!   stabilizes what the model sees, and its box (× `search`, the largest on
//!   the job's frames) fits inside.
//! - **Seeds:** the tracked point where the job starts (the anchor's reset
//!   point, or where the tracker was when resuming), and every reset point
//!   (its looks) on the job's frames, each queried on its own frame exactly
//!   where the user put it: it follows one pixel at a time, and a reset
//!   point says which, from there on (no template alignment, as the
//!   template method's looks get). On each frame the latest seed behind it
//!   answers (a fresher seed has drifted less); a reset point's own frame is
//!   pinned there.
//! - The job says when the worker is loading its model ([`super::Phase`]):
//!   starting Python and loading PyTorch and the model take seconds.
//! - **Score** is the model's visibility × confidence; below `min_score` the
//!   frame is flagged lost (its position stays the model's estimate).
//! - Catch-up mode works, but the model finalizes frames a half window (8)
//!   at a time, so the last few frames before the playhead wait for more.
//! - **Workers:** each job starts its own (a Python process with the model on
//!   the graphics card), and the runner lets one such job live at a time
//!   (`runner::cotracker_jobs`). The anchor alone (the forward side of a
//!   tracker asked only backward) needs none: it is the seed.
//! - **A stuck worker** (the graphics card stopped answering, say) is stopped
//!   after [`HANG`] with nothing from it while its job waits on it (to take
//!   a frame, or for its last results), and the job fails; a
//!   cancelled job's worker is stopped within [`CANCEL_GRACE`] even if the
//!   job is stuck writing to it. Loading the model has no time limit (the
//!   first time takes long); only a cancel ends it. Each worker's start and
//!   end are logged, and a failed one's last messages.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use tt_core::time::FrameIndex;
use tt_media::FrameStream;

use super::{JobSpec, MAX_PATCHES, SEGMENT, Shared, Side, Worker};
use crate::Method;
use crate::image::{Grid, Luma, chroma_planes, resample_xy, with_colour};

/// The model's input size.
pub const CROP_W: usize = 512;
pub const CROP_H: usize = 384;

/// A worker that sends nothing for this long while it owes results (and its
/// job isn't waiting at the playhead) is stuck: it is stopped, and the job fails.
const HANG: Duration = Duration::from_secs(120);
/// A cancelled job's worker is stopped after this long if the job still
/// holds it (stuck writing to a worker that doesn't read).
const CANCEL_GRACE: Duration = Duration::from_secs(2);
/// A worker whose input is closed has this long to exit by itself (it
/// finishes at the end of its input); then it is stopped.
const EXIT_GRACE: Duration = Duration::from_secs(1);
/// How often waits look at the job's cancel flag, and the watchdog at the worker.
const POLL: Duration = Duration::from_millis(50);

impl JobSpec {
    /// The frames a CoTracker job streams to its worker: where the stream
    /// starts (the anchor, or the frame before `from` when resuming), and how many.
    fn stream(&self) -> (FrameIndex, usize) {
        let dir: FrameIndex = if self.side == Side::Forward { 1 } else { -1 };
        let start = if self.resume.is_some() { self.from - dir } else { self.anchor };
        (start, ((self.to - start) * dir + 1).max(1) as usize)
    }

    /// Whether the job starts a CoTracker worker (a Python process with the
    /// model on the graphics card): not for the anchor alone.
    pub fn starts_worker(&self) -> bool {
        self.method == Method::CoTracker && self.stream().1 > 1
    }
}

/// Where the app's doctor sets CoTracker up on a computer without the
/// repository (tt_app::cotracker): `cotracker\` in the data folder.
pub fn installed_dir() -> PathBuf {
    tt_media::proxy::data_dir().join("cotracker")
}

/// The Python of the environment the doctor made.
pub fn installed_python() -> PathBuf {
    installed_dir().join("env").join(if cfg!(windows) { "Scripts/python.exe" } else { "bin/python" })
}

/// The worker, as the doctor wrote it out (the code is packed into the app).
pub fn installed_worker() -> PathBuf {
    installed_dir().join("code").join("editor").join("cotracker_worker.py")
}

/// The model the doctor downloaded.
pub fn installed_weights() -> PathBuf {
    installed_dir().join("scaled_online.pth")
}

/// The Python interpreter and worker script to run: `TT_PYTHON` and
/// `TT_COTRACKER_WORKER`, else what the doctor set up, else the repository's
/// `.venv` (as v1 set it up) or `python3` / `python`, and
/// `editor/cotracker_worker.py` next to this crate.
pub fn worker_command() -> (PathBuf, PathBuf) {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let python = std::env::var_os("TT_PYTHON").map(PathBuf::from).unwrap_or_else(|| {
        [installed_python(), repo.join(".venv/Scripts/python.exe"), repo.join(".venv/bin/python")]
            .into_iter()
            .find(|p| p.exists())
            .unwrap_or_else(|| PathBuf::from(if cfg!(windows) { "python" } else { "python3" }))
    });
    let script = std::env::var_os("TT_COTRACKER_WORKER").map(PathBuf::from).unwrap_or_else(|| {
        let installed = installed_worker();
        if installed.is_file() { installed } else { repo.join("editor/cotracker_worker.py") }
    });
    (python, script)
}

static CHECKED: std::sync::Mutex<Option<Result<(), String>>> = std::sync::Mutex::new(None);

/// Whether CoTracker can run on this computer: the worker script, a Python
/// and the weights where the worker looks for them. Err: what's missing, in
/// words for the person (ASD-STE100, like the doctor). (Whether that Python
/// has PyTorch shows when a job starts.) Checked once, until
/// [`forget_availability`].
pub fn availability() -> Result<(), String> {
    let mut checked = CHECKED.lock().unwrap_or_else(|e| e.into_inner());
    checked
        .get_or_insert_with(|| {
            let (python, script) = worker_command();
            let bare = python.components().count() == 1;
            if !script.is_file() || !(python.is_file() || bare && on_path(&python)) || !weights().is_some_and(|w| w.is_file()) {
                return Err("CoTracker is not set up on this computer.".into());
            }
            Ok(())
        })
        .clone()
}

/// Something was installed: [`availability`] looks again.
pub fn forget_availability() {
    *CHECKED.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

fn on_path(program: &std::path::Path) -> bool {
    let exe = if cfg!(windows) { program.with_extension("exe") } else { program.to_path_buf() };
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(&exe).is_file()))
}

/// Where the weights are: `TT_COTRACKER_WEIGHTS`, else the doctor's
/// download, else torch hub's cache (`torch.hub.get_dir()`, as v1
/// downloaded them). The worker is told (`--weights`).
pub fn weights() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("TT_COTRACKER_WEIGHTS") {
        return Some(PathBuf::from(p));
    }
    let installed = installed_weights();
    if installed.is_file() {
        return Some(installed);
    }
    let hub = match std::env::var_os("TORCH_HOME") {
        Some(t) => PathBuf::from(t).join("hub"),
        None => {
            let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(|h| PathBuf::from(h).join(".cache"));
            std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from).or(home)?.join("torch").join("hub")
        }
    };
    Some(hub.join("checkpoints").join("scaled_online.pth"))
}

/// A worker's message.
enum Reply {
    Ready,
    Frame(usize, Vec<Option<[f64; 3]>>),
    Done,
    Error(String),
}

/// What a worker's watchdog knows: its job's cancel flag, and how the
/// worker answers (told by the thread reading its replies).
struct Watch {
    job: Arc<Shared>,
    start: Instant,
    /// It said it is ready (its model is loaded).
    ready: AtomicBool,
    /// The job waits on it: writing a frame to it (it reads the next once it
    /// has tracked a window), or waiting for its last results. (Not while the
    /// job decodes, or waits at the playhead.)
    owed: AtomicBool,
    /// When it last said anything, ms after `start`.
    heard: AtomicU64,
    /// Its owner is stopping it: the watchdog goes.
    stop: AtomicBool,
    /// The watchdog stopped it: it sent nothing for [`HANG`].
    hung: AtomicBool,
}

impl Watch {
    fn new(job: Arc<Shared>) -> Self {
        let no = || AtomicBool::new(false);
        Self { job, start: Instant::now(), ready: no(), owed: no(), heard: AtomicU64::new(0), stop: no(), hung: no() }
    }

    fn hear(&self) {
        self.heard.store(self.start.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    fn last_heard(&self) -> Instant {
        self.start + Duration::from_millis(self.heard.load(Ordering::Relaxed))
    }
}

/// Stop the worker when it is stuck (nothing from it for [`HANG`] while its
/// job waits on it), or when its job was cancelled [`CANCEL_GRACE`] ago and
/// still holds it. Stopping it ends the job's waits and writes. Leaves when
/// the worker's owner stops it.
fn watchdog(child: &Mutex<Child>, w: &Watch, name: &str) {
    let mut excused = Instant::now();
    let mut cancelled: Option<Instant> = None;
    loop {
        std::thread::sleep(POLL);
        if w.stop.load(Ordering::Relaxed) {
            return;
        }
        if w.job.cancel.load(Ordering::Relaxed) {
            if cancelled.get_or_insert_with(Instant::now).elapsed() < CANCEL_GRACE {
                continue;
            }
            tracing::info!("CoTracker worker {name}: its job was cancelled and still holds it; stopping it");
        } else {
            if !(w.ready.load(Ordering::Relaxed) && w.owed.load(Ordering::Relaxed)) {
                excused = Instant::now();
            }
            if excused.max(w.last_heard()).elapsed() < HANG {
                continue;
            }
            w.hung.store(true, Ordering::Relaxed);
            tracing::warn!("CoTracker worker {name}: no answer for {} s; stopping it", HANG.as_secs());
        }
        let _ = child.lock().unwrap_or_else(PoisonError::into_inner).kill();
        return;
    }
}

/// The worker process: its stdin, its replies (read on a thread), the end of
/// its stderr (for errors), and its watchdog. Lives on the job's thread
/// (in `run_learned`), so stopping it never holds up the app.
struct Process {
    child: Arc<Mutex<Child>>,
    stdin: Option<ChildStdin>,
    replies: Receiver<Reply>,
    log: Arc<Mutex<Vec<String>>>,
    /// The thread reading its stderr (it ends when the worker does).
    stderr: JoinHandle<()>,
    watch: Arc<Watch>,
    /// "pid 1234 (Tracker 2, Forward, 61 frames)", for the log.
    name: String,
    /// It sent `done`: the job ended as it should.
    finished: bool,
}

impl Process {
    /// Start the worker for the job sharing `job`; `what` says which, for the log.
    fn start(job: Arc<Shared>, what: &str) -> Result<Self> {
        let (python, script) = worker_command();
        let mut cmd = Command::new(&python);
        cmd.arg(&script).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        if let Some(w) = weights().filter(|w| w.is_file()) {
            cmd.arg("--weights").arg(w);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: the app has no console
        }
        let mut child = cmd.spawn().with_context(|| format!("starting the CoTracker worker ({} {})", python.display(), script.display()))?;
        let name = format!("pid {} ({what})", child.id());
        tracing::info!("CoTracker worker {name} started: {} {}", python.display(), script.display());
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let watch = Arc::new(Watch::new(job));
        let (tx, replies) = channel();
        let heard = watch.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                heard.hear();
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
                let reply = if v.get("ready").is_some() {
                    heard.ready.store(true, Ordering::Relaxed);
                    Reply::Ready
                } else if let Some(f) = v.get("f").and_then(|f| f.as_u64()) {
                    let points = v["points"]
                        .as_array()
                        .map(|a| a.iter().map(|p| p.as_array().filter(|p| p.len() == 3).map(|p| [0, 1, 2].map(|i| p[i].as_f64().unwrap_or(f64::NAN)))).collect())
                        .unwrap_or_default();
                    Reply::Frame(f as usize, points)
                } else if v.get("done").is_some() {
                    Reply::Done
                } else {
                    Reply::Error(v.get("error").and_then(|e| e.as_str()).unwrap_or("the worker failed").to_string())
                };
                if tx.send(reply).is_err() {
                    break;
                }
            }
        });
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink = log.clone();
        let stderr = std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut l = sink.lock().unwrap_or_else(PoisonError::into_inner);
                l.push(line);
                if l.len() > 20 {
                    l.remove(0);
                }
            }
        });
        let stdin = child.stdin.take();
        let child = Arc::new(Mutex::new(child));
        {
            let (child, watch, name) = (child.clone(), watch.clone(), name.clone());
            std::thread::spawn(move || watchdog(&child, &watch, &name));
        }
        Ok(Self { child, stdin, replies, log, stderr, watch, name, finished: false })
    }

    /// Write to it (waiting on it while it tracks: the pipe holds little).
    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        let stdin = self.stdin.as_mut().context("the worker's input is closed")?;
        self.watch.owed.store(true, Ordering::Relaxed);
        let written = stdin.write_all(bytes);
        self.watch.owed.store(false, Ordering::Relaxed);
        written.map_err(|e| self.stopped(Some(&e)))
    }

    /// Why the worker can't be talked to any more: it was stuck (its watchdog
    /// stopped it), or it stopped by itself (and what it said last). `why`:
    /// what failed here.
    fn stopped(&self, why: Option<&dyn std::fmt::Display>) -> anyhow::Error {
        if self.watch.hung.load(Ordering::Relaxed) {
            return anyhow!("CoTracker stopped answering.");
        }
        let why = why.map_or(String::new(), |e| format!(" ({e})"));
        anyhow!("the CoTracker worker stopped{why}: {}", self.last_words(3, " | "))
    }

    /// The last `n` lines of its stderr, joined by `sep` (waiting a moment
    /// for the rest when it is ending).
    fn last_words(&self, n: usize, sep: &str) -> String {
        let until = Instant::now() + Duration::from_millis(250);
        while !self.stderr.is_finished() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        let log = self.log.lock().unwrap_or_else(PoisonError::into_inner);
        log[log.len().saturating_sub(n)..].join(sep)
    }
}

impl Drop for Process {
    /// Close its input and give it [`EXIT_GRACE`] to exit by itself, then
    /// stop it. Logs how it ended, and a failed one's last messages.
    fn drop(&mut self) {
        self.watch.stop.store(true, Ordering::Relaxed);
        drop(self.stdin.take());
        let (status, stopped) = {
            let mut child = self.child.lock().unwrap_or_else(PoisonError::into_inner);
            let deadline = Instant::now() + EXIT_GRACE;
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => break (Some(status), false),
                    Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
                    _ => {
                        let _ = child.kill();
                        break (child.wait().ok(), true);
                    }
                }
            }
        };
        let secs = self.watch.start.elapsed().as_secs_f64();
        let status = status.map_or("status unknown".to_string(), |s| s.to_string());
        let how = if stopped { "was stopped" } else { "exited" };
        tracing::info!("CoTracker worker {} {how} after {secs:.1} s ({status})", self.name);
        if !self.finished && !self.watch.job.cancel.load(Ordering::Relaxed) {
            tracing::warn!("CoTracker worker {} failed; its last messages:\n{}", self.name, self.last_words(20, "\n"));
        }
    }
}

/// Where the job's frames sit in the stream the worker sees.
struct Stream {
    /// The stream's first frame, and which way the frames go.
    start: FrameIndex,
    dir: FrameIndex,
    /// Per stream index sent, the crop's grid.
    grids: Vec<Grid>,
}

impl Stream {
    fn frame(&self, i: usize) -> FrameIndex {
        self.start + self.dir * i as FrameIndex
    }
}

impl Worker {
    /// The crop's scale for this job (crop px per view px): the guide's box
    /// (× `search`) on every frame the job reads fits, with a margin.
    fn crop_scale(&self, frames: impl Iterator<Item = FrameIndex>) -> f64 {
        let s = &self.spec;
        let (mut hw, mut hh) = (8.0f64, 8.0f64);
        // With no guide the "box" is the whole frame: it fits exactly (the
        // model sees the whole picture, as CoTracker is usually run).
        let (search, k) = if s.root { (1.0, 2.0) } else { (s.search, 2.5) };
        for f in frames {
            let b = self.map(f).box_from_source(self.guide(f));
            hw = hw.max((b[0] - b[2]).max(b[4] - b[0]) * search);
            hh = hh.max((b[1] - b[3]).max(b[5] - b[1]) * search);
        }
        let w = (k * hw).max(k * hh * CROP_W as f64 / CROP_H as f64);
        CROP_W as f64 / w
    }

    /// Frame `f` (decoded, NV12) through the tracker's view: the crop around
    /// the guide's point at `scale`, as RGB bytes, and its grid.
    fn crop(&self, frame: &[u8], f: FrameIndex, scale: f64) -> (Grid, Vec<u8>) {
        let (map, s) = (self.map(f), &self.spec);
        let g = self.guide_point(f);
        let grid = Grid { origin: [g[0] - CROP_W as f64 / 2.0 / scale, g[1] - CROP_H as f64 / 2.0 / scale], scale };
        let (vw, vh) = (s.video.width as usize, s.video.height as usize);
        let luma = resample_xy(&Luma { data: &frame[..vw * vh], width: vw, height: vh }, s.k, map, grid, CROP_W, CROP_H);
        let patch = with_colour(luma, &chroma_planes(frame, vw, vh), vw, vh, s.k, map, grid);
        let [u, v] = &**patch.colour.as_ref().expect("coloured");
        // Limited range; BT.709 for HD, BT.601 below (the usual convention).
        let (rv, gu, gv, bu) = if vh >= 720 { (1.793, -0.213, -0.533, 2.112) } else { (1.596, -0.392, -0.813, 2.017) };
        let mut rgb = Vec::with_capacity(CROP_W * CROP_H * 3);
        for i in 0..CROP_W * CROP_H {
            let (y, u, v) = (1.164 * (patch.data[i] - 16.0), u[i] - 128.0, v[i] - 128.0);
            rgb.extend([y + rv * v, y + gu * u + gv * v, y + bu * u].map(|c| c.round().clamp(0.0, 255.0) as u8));
        }
        (grid, rgb)
    }

    /// Track with CoTracker3 (see the module docs).
    pub(super) fn run_learned(&mut self) -> Result<()> {
        let s = &self.spec;
        let dir: FrameIndex = if s.side == Side::Forward { 1 } else { -1 };
        let anchor = s.anchor;
        // (The output box: the first reset point's size, in patch px as the template method's.)
        if let Some(l) = s.looks.first() {
            let a = self.map(l.frame).a;
            self.half = l.half.map(|h| (h / a * s.scale).max(2.0));
        }
        let seed = s.seed.map_or_else(|| self.guide_point(anchor), |p| self.map(anchor).from_source(p));
        // Where the stream starts: the frame before `from` when resuming, else the anchor.
        let (start, n) = s.stream();
        // The anchor alone is the seed: no worker for it.
        if n == 1 && s.resume.is_none() {
            if s.side == Side::Forward && self.wait_for(anchor, || {}) {
                self.emit(anchor, seed, 1.0, false);
            }
            return Ok(());
        }
        let first_seed = match s.resume {
            Some((src, _)) => self.map(start).from_source(src),
            None => seed,
        };
        let to = s.to;
        let scale = self.crop_scale((0..n).map(|i| start + dir * i as FrameIndex));
        let mut stream = Stream { start, dir, grids: Vec::with_capacity(n) };

        self.shared.set_phase(super::Phase::Loading);
        let what = format!("{}, {:?}, {n} frames", s.label, s.side);
        let mut worker = Process::start(self.shared.clone(), &what)?;
        // (No time limit: loading takes long the first time. A cancel ends it.)
        loop {
            match worker.replies.recv_timeout(POLL) {
                Ok(Reply::Ready) => break,
                Ok(Reply::Error(e)) => bail!("CoTracker: {e}"),
                Err(RecvTimeoutError::Timeout) if self.cancelled() => return Ok(()),
                Err(RecvTimeoutError::Timeout) => {}
                Ok(_) | Err(RecvTimeoutError::Disconnected) => bail!("the CoTracker worker didn't start: {}", worker.last_words(3, " | ")),
            }
        }
        self.shared.set_phase(super::Phase::Tracking);
        // Seeds, in crop pixels on their frames: the start, and each look's aligned point further on.
        let crop_point = |f: FrameIndex, p: [f64; 2]| {
            let g = self.guide_point(f);
            [(p[0] - g[0]) * scale + CROP_W as f64 / 2.0, (p[1] - g[1]) * scale + CROP_H as f64 / 2.0]
        };
        let mut queries = vec![(0usize, crop_point(start, first_seed))];
        for i in 1..n {
            if let Some(p) = self.pin(stream.frame(i)) {
                queries.push((i, crop_point(stream.frame(i), p)));
            }
        }
        let header = serde_json::json!({ "width": CROP_W, "height": CROP_H, "queries": queries.iter().map(|(i, p)| [*i as f64, p[0], p[1]]).collect::<Vec<_>>() });
        worker.send(format!("{header}\n").as_bytes())?;

        // The anchor itself is the seed (when not resuming, on the forward side).
        if s.resume.is_none() && s.side == Side::Forward && self.wait_for(anchor, || {}) {
            self.emit(anchor, seed, 1.0, false);
        }
        let queries: Vec<usize> = queries.iter().map(|(i, _)| *i).collect();
        match self.spec.side {
            Side::Forward => {
                let mut decoder: Option<FrameStream> = None;
                let (mut held, mut buf) = (None, Vec::new());
                for i in 0..n {
                    let f = stream.frame(i);
                    // (Frames other than the start are sent only once the catch-up limit allows.)
                    if i > 0 && !self.wait_for(f, || decoder = None) {
                        return Ok(());
                    }
                    if decoder.is_none() {
                        held = None;
                        decoder = Some(FrameStream::start(&self.spec.video, self.presented(f), &self.spec.decode)?);
                    }
                    self.read_to(decoder.as_mut().expect("opened"), &mut held, &mut buf, f)?;
                    let (grid, rgb) = self.crop(&buf, f, scale);
                    self.send_frame(&mut worker, &mut stream, grid, &rgb, &queries)?;
                }
            }
            Side::Backward => {
                // Keyframe-aligned segments decoded forward, sent in reverse.
                let mut hi = start;
                while hi >= to {
                    let want = (hi - SEGMENT + 1).max(to);
                    let key = self.spec.video.group_start(self.presented(want));
                    let lo = self.spec.grid.grid_of.get(key).copied().unwrap_or(want).clamp(to, want).max(hi + 1 - MAX_PATCHES);
                    let mut decoder = FrameStream::start(&self.spec.video, self.presented(lo), &self.spec.decode)?;
                    let (mut held, mut buf) = (None, Vec::new());
                    let mut crops = Vec::with_capacity((hi - lo + 1) as usize);
                    for f in lo..=hi {
                        if self.cancelled() {
                            return Ok(());
                        }
                        self.read_to(&mut decoder, &mut held, &mut buf, f)?;
                        crops.push(self.crop(&buf, f, scale));
                    }
                    drop(decoder);
                    for (f, (grid, rgb)) in (lo..=hi).rev().zip(crops.into_iter().rev()) {
                        if f != start && !self.wait_for(f, || {}) {
                            return Ok(());
                        }
                        self.send_frame(&mut worker, &mut stream, grid, &rgb, &queries)?;
                    }
                    hi = lo - 1;
                }
            }
        }
        worker.send(b"E")?;
        drop(worker.stdin.take());
        // (A worker that stops answering is stopped by its watchdog: the replies end.)
        worker.watch.owed.store(true, Ordering::Relaxed);
        loop {
            if self.cancelled() {
                return Ok(());
            }
            match worker.replies.recv_timeout(POLL) {
                Ok(Reply::Frame(i, points)) => self.take(&stream, &queries, i, &points),
                Ok(Reply::Done) => {
                    worker.finished = true;
                    return Ok(());
                }
                Ok(Reply::Error(e)) => bail!("CoTracker: {e}"),
                Ok(Reply::Ready) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err(worker.stopped(None)),
            }
        }
    }

    /// Send one crop, and take whatever results have come back meanwhile.
    fn send_frame(&mut self, worker: &mut Process, stream: &mut Stream, grid: Grid, rgb: &[u8], queries: &[usize]) -> Result<()> {
        if self.cancelled() {
            return Ok(());
        }
        stream.grids.push(grid);
        worker.send(b"F")?;
        worker.send(rgb)?;
        while let Ok(reply) = worker.replies.try_recv() {
            match reply {
                Reply::Frame(i, points) => self.take(stream, queries, i, &points),
                Reply::Error(e) => bail!("CoTracker: {e}"),
                Reply::Ready | Reply::Done => {}
            }
        }
        Ok(())
    }

    /// Stream frame `i`'s result: from the latest seed at or before it; a
    /// look's own frame is pinned where the user put it.
    fn take(&mut self, stream: &Stream, queries: &[usize], i: usize, points: &[Option<[f64; 3]>]) {
        let Some(grid) = stream.grids.get(i).copied() else { return };
        let f = stream.frame(i);
        // The stream's first frame is the anchor (emitted already) or the resume point (tracked before).
        if i == 0 {
            return;
        }
        if let Some(p) = self.pin(f) {
            self.emit(f, p, 1.0, false);
            return;
        }
        let latest = queries.iter().enumerate().filter(|(_, q)| **q <= i).map(|(k, _)| k).rev().find_map(|k| points.get(k).copied().flatten());
        match latest {
            Some([x, y, p]) if x.is_finite() && y.is_finite() => {
                let score = p.clamp(0.0, 1.0) as f32;
                self.emit(f, grid.to_view([x, y]), score, score < self.spec.settings.min_score);
            }
            _ => {
                let g = self.guide_point(f);
                self.emit(f, g, 0.0, true);
            }
        }
    }
}
