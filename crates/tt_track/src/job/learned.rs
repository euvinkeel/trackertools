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
//! - **One worker for all of them** (on request: "ensure that multi
//!   cotracker processing is in"): every CoTracker job is a *stream* through
//!   one Python process (`--shared`), so the model is on the graphics card
//!   once, in one context, its windows running one at a time: several jobs
//!   in their own processes at once made the card reset (2026-10-04). The
//!   worker runs the streams' windows in rounds, so several trackers move on
//!   together (`runner::cotracker_jobs` of them), each round's windows as
//!   one batch on the card (the worker's docs), and only the first job
//!   waits for the model to load. It closes once no stream has used it for
//!   [`IDLE`]. The anchor alone (the forward side of a tracker asked only
//!   backward) needs no stream: it is the seed.
//! - **Started early** (on request: "auto start up cotracker engine so we
//!   don't have to warm it up the moment we place a cotracker point"): the
//!   app calls [`warm_up`] once a video is open, so the model is loaded (and
//!   has run a practice window: the worker's `practice`) before the first
//!   CoTracker tracker needs it, and kept loaded while the app runs
//!   ([`keep_warm`]). [`engine`] says how it is: starting, ready (on which
//!   device), or why it could not start ([`Engine::Failed`]), for the app
//!   to say so; it never retries by itself after a failure (a CoTracker job
//!   still starts a worker when it needs one).
//! - **A stuck worker** (the graphics card stopped answering, say) is stopped
//!   after [`HANG`] with nothing from it while a stream waits on it (to take
//!   a frame, or for its last results), and the jobs on it fail; the next
//!   job starts another. A stream whose job is cancelled is dropped by the
//!   worker (the others go on). Loading the model has no time limit (the
//!   first time takes long); only a cancel ends a job's wait. The worker's
//!   start and end and each stream are logged, and a failed worker's last
//!   messages.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, Mutex, PoisonError};
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

/// A worker that sends nothing for this long while a stream waits on it is
/// stuck: it is stopped, and the jobs using it fail.
const HANG: Duration = Duration::from_secs(120);
/// A worker no stream has used for this long closes (its model leaves the
/// graphics card); the next CoTracker job starts it again.
const IDLE: Duration = Duration::from_secs(30);
/// A worker whose input is closed has this long to exit by itself (it
/// finishes at the end of its input); then it is stopped.
const EXIT_GRACE: Duration = Duration::from_secs(1);
/// Messages waiting for the worker to read them (a few frames).
const QUEUE: usize = 6;
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

/// A stream's message from the worker.
enum Reply {
    Ready,
    Frame(usize, Vec<Option<[f64; 3]>>),
    Done,
    Error(String),
}

/// Worker processes started (tests count them: jobs share one).
static STARTED: AtomicUsize = AtomicUsize::new(0);

/// Keep the worker while nothing uses it ([`keep_warm`]).
static KEEP: AtomicBool = AtomicBool::new(false);

/// Why the last worker could not start or stopped (cleared when one is ready).
static FAILED: Mutex<Option<String>> = Mutex::new(None);

/// How the CoTracker engine (the shared worker) is now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Engine {
    /// No worker runs.
    Off,
    /// Its Python and model are loading (the first time: a minute or so).
    Starting,
    /// Loaded and warmed up, on this device ("cuda", "mps", "cpu").
    Ready(String),
    /// The last one could not start, or stopped: why, in its own words.
    Failed(String),
}

/// How the engine is now.
pub fn engine() -> Engine {
    let shared = SHARED.lock().unwrap_or_else(PoisonError::into_inner);
    match shared.as_ref().filter(|p| !p.dead.load(Ordering::Relaxed)) {
        Some(p) if p.ready.load(Ordering::Relaxed) => Engine::Ready(p.device.lock().unwrap_or_else(PoisonError::into_inner).clone().unwrap_or_default()),
        Some(_) => Engine::Starting,
        None => match FAILED.lock().unwrap_or_else(PoisonError::into_inner).clone() {
            Some(why) => Engine::Failed(why),
            None => Engine::Off,
        },
    }
}

/// Start the engine now if it isn't running (in the background: this
/// returns at once), and keep it loaded while nothing uses it. Err: the
/// worker could not even start (also kept for [`engine`]).
pub fn warm_up() -> Result<()> {
    keep_warm(true);
    Proc::current().map(|_| ())
}

/// Keep the worker loaded while no tracker uses it (true), or close it
/// [`IDLE`] after its last stream as before (false).
pub fn keep_warm(on: bool) {
    KEEP.store(on, Ordering::Relaxed);
}

/// Note why a worker failed (for [`engine`]); its stderr's last line says most.
fn failed(why: String) {
    tracing::warn!("CoTracker engine: {why}");
    *FAILED.lock().unwrap_or_else(PoisonError::into_inner) = Some(why);
}

/// How many CoTracker worker processes have started since the app did.
pub fn processes_started() -> usize {
    STARTED.load(Ordering::Relaxed)
}

/// Close the shared worker now (its streams' jobs fail); the next CoTracker
/// job starts another. For a worker set up anew (the doctor), and tests
/// whose fake worker's switches changed.
pub fn close_worker() {
    *FAILED.lock().unwrap_or_else(PoisonError::into_inner) = None;
    if let Some(p) = SHARED.lock().unwrap_or_else(PoisonError::into_inner).take() {
        p.dead.store(true, Ordering::Relaxed);
        drop(p.queue.lock().unwrap_or_else(PoisonError::into_inner).take());
    }
}

/// The worker all CoTracker jobs share, while it lives.
static SHARED: Mutex<Option<Arc<Proc>>> = Mutex::new(None);

/// The one worker process (module docs): the model on the graphics card
/// once, every CoTracker job a stream through it. Its threads read its
/// replies (routing each to its stream), its stderr, and watch it: stopped
/// when it hangs, closed once no stream has used it for [`IDLE`].
struct Proc {
    child: Mutex<Child>,
    /// Whole messages for its input, written in order by its writer thread
    /// (so a job never blocks on a worker that doesn't read: it waits for
    /// room here, and a cancel ends the wait). None: closed.
    queue: Mutex<Option<std::sync::mpsc::SyncSender<Vec<u8>>>>,
    /// Each open stream's way to its job.
    routes: Mutex<HashMap<u32, std::sync::mpsc::Sender<Reply>>>,
    next: AtomicU32,
    /// Its model is loaded.
    ready: AtomicBool,
    /// The device it said it runs on.
    device: Mutex<Option<String>>,
    /// It is gone or going (failed, hung, idle): the next job starts another.
    dead: AtomicBool,
    /// Its watchdog stopped it: nothing came from it for [`HANG`].
    hung: AtomicBool,
    start: Instant,
    /// When it last said anything, ms after `start`.
    heard: AtomicU64,
    /// Streams waiting on it now (writing a frame, or waiting for their last results).
    owed: AtomicUsize,
    /// Streams open.
    open: AtomicUsize,
    log: Arc<Mutex<Vec<String>>>,
    /// "pid 1234", for the log.
    name: String,
}

impl Proc {
    fn hear(&self) {
        self.heard.store(self.start.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    fn last_heard(&self) -> Instant {
        self.start + Duration::from_millis(self.heard.load(Ordering::Relaxed))
    }

    /// The last `n` lines of its stderr, joined by `sep`.
    fn last_words(&self, n: usize, sep: &str) -> String {
        let log = self.log.lock().unwrap_or_else(PoisonError::into_inner);
        log[log.len().saturating_sub(n)..].join(sep)
    }

    /// The live shared worker, started if there is none.
    fn current() -> Result<Arc<Proc>> {
        let mut shared = SHARED.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(p) = shared.as_ref().filter(|p| !p.dead.load(Ordering::Relaxed)) {
            return Ok(p.clone());
        }
        let p = Proc::spawn().inspect_err(|e| failed(format!("{e:#}")))?;
        *shared = Some(p.clone());
        Ok(p)
    }

    fn spawn() -> Result<Arc<Proc>> {
        let (python, script) = worker_command();
        let mut cmd = Command::new(&python);
        cmd.arg(&script).arg("--shared").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        if let Some(w) = weights().filter(|w| w.is_file()) {
            cmd.arg("--weights").arg(w);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: the app has no console
        }
        let mut child = cmd.spawn().with_context(|| format!("starting the CoTracker worker ({} {})", python.display(), script.display()))?;
        STARTED.fetch_add(1, Ordering::Relaxed);
        let name = format!("pid {}", child.id());
        tracing::info!("CoTracker worker {name} started (shared by every CoTracker job): {} {}", python.display(), script.display());
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let mut stdin = child.stdin.take().expect("piped");
        let (queue, outbox) = std::sync::mpsc::sync_channel::<Vec<u8>>(QUEUE);
        let p = Arc::new(Proc {
            child: Mutex::new(child),
            queue: Mutex::new(Some(queue)),
            routes: Mutex::new(HashMap::new()),
            next: AtomicU32::new(1),
            ready: AtomicBool::new(false),
            device: Mutex::new(None),
            dead: AtomicBool::new(false),
            hung: AtomicBool::new(false),
            start: Instant::now(),
            heard: AtomicU64::new(0),
            owed: AtomicUsize::new(0),
            open: AtomicUsize::new(0),
            log: Arc::new(Mutex::new(Vec::new())),
            name,
        });
        // Its input: the queued messages, in order (closing the queue closes it).
        let me = p.clone();
        std::thread::spawn(move || {
            for msg in outbox {
                if stdin.write_all(&msg).and_then(|()| stdin.flush()).is_err() {
                    me.dead.store(true, Ordering::Relaxed);
                    break;
                }
            }
        });
        // Its replies, each to its stream.
        let me = p.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                me.hear();
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
                let routes = me.routes.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some(r) = v.get("ready") {
                    *me.device.lock().unwrap_or_else(PoisonError::into_inner) = r.get("device").and_then(|d| d.as_str()).map(str::to_string);
                    *FAILED.lock().unwrap_or_else(PoisonError::into_inner) = None;
                    me.ready.store(true, Ordering::Relaxed);
                    tracing::info!("CoTracker worker {} ready on {}", me.name, r.get("device").and_then(|d| d.as_str()).unwrap_or("?"));
                    for tx in routes.values() {
                        let _ = tx.send(Reply::Ready);
                    }
                    continue;
                }
                let Some(sid) = v.get("s").and_then(|s| s.as_u64()) else {
                    // The worker failed as a whole: every stream hears it.
                    let e = v.get("error").and_then(|e| e.as_str()).unwrap_or("the worker failed").to_string();
                    failed(e.clone());
                    me.dead.store(true, Ordering::Relaxed);
                    for tx in routes.values() {
                        let _ = tx.send(Reply::Error(e.clone()));
                    }
                    continue;
                };
                let reply = if let Some(f) = v.get("f").and_then(|f| f.as_u64()) {
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
                if let Some(tx) = routes.get(&(sid as u32)) {
                    let _ = tx.send(reply);
                }
            }
            // It ended: every stream's replies end (their jobs see it). Before it was ready, and not
            // closed by us: it could not start (Python or PyTorch missing, say).
            if !me.ready.load(Ordering::Relaxed) && !me.dead.load(Ordering::Relaxed) && FAILED.lock().unwrap_or_else(PoisonError::into_inner).is_none() {
                std::thread::sleep(Duration::from_millis(200)); // (its stderr's last lines come in)
                let words = me.last_words(1, "");
                failed(if words.is_empty() { "the CoTracker worker stopped while it loaded".to_string() } else { words });
            }
            me.dead.store(true, Ordering::Relaxed);
            me.routes.lock().unwrap_or_else(PoisonError::into_inner).clear();
        });
        // Its stderr, the last lines kept.
        let sink = p.log.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut l = sink.lock().unwrap_or_else(PoisonError::into_inner);
                l.push(line);
                if l.len() > 30 {
                    l.remove(0);
                }
            }
        });
        let me = p.clone();
        std::thread::spawn(move || me.watch());
        Ok(p)
    }

    /// Its watchdog: stops it when it hangs (nothing from it for [`HANG`]
    /// while a stream waits on it, once its model is loaded: loading takes
    /// long the first time), closes it once no stream has used it for
    /// [`IDLE`], and logs how it ended.
    fn watch(self: Arc<Self>) {
        let mut idle: Option<Instant> = None;
        let mut excused = Instant::now();
        let (closed, killed) = loop {
            std::thread::sleep(POLL);
            if self.dead.load(Ordering::Relaxed) {
                break (false, false);
            }
            if self.open.load(Ordering::Relaxed) == 0 && !KEEP.load(Ordering::Relaxed) {
                if idle.get_or_insert_with(Instant::now).elapsed() >= IDLE {
                    // Nothing uses it: it goes (its model leaves the graphics card).
                    self.dead.store(true, Ordering::Relaxed);
                    drop(self.queue.lock().unwrap_or_else(PoisonError::into_inner).take());
                    break (true, false);
                }
            } else {
                idle = None;
            }
            if !(self.ready.load(Ordering::Relaxed) && self.owed.load(Ordering::Relaxed) > 0) {
                excused = Instant::now();
            }
            if excused.max(self.last_heard()).elapsed() >= HANG {
                self.hung.store(true, Ordering::Relaxed);
                failed(format!("CoTracker stopped answering for {} s, so it was stopped", HANG.as_secs()));
                self.dead.store(true, Ordering::Relaxed);
                tracing::warn!("CoTracker worker {}: no answer for {} s; stopping it", self.name, HANG.as_secs());
                let _ = self.child.lock().unwrap_or_else(PoisonError::into_inner).kill();
                break (false, true);
            }
        };
        // Its end: given EXIT_GRACE to go by itself, then stopped.
        let status = {
            let mut child = self.child.lock().unwrap_or_else(PoisonError::into_inner);
            let deadline = Instant::now() + EXIT_GRACE;
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => break Some(status),
                    Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
                    _ => {
                        let _ = child.kill();
                        break child.wait().ok();
                    }
                }
            }
        };
        let secs = self.start.elapsed().as_secs_f64();
        let status = status.map_or("status unknown".to_string(), |s| s.to_string());
        let how = if closed { format!("closed after {} s unused", IDLE.as_secs()) } else if killed { "was stopped".to_string() } else { "ended".to_string() };
        tracing::info!("CoTracker worker {} {how}, {secs:.1} s after it started ({status})", self.name);
        if !closed {
            tracing::warn!("CoTracker worker {}: its last messages:\n{}", self.name, self.last_words(20, "\n"));
        }
    }
}

/// A job's stream through the shared worker: its number, and its replies.
struct Process {
    proc: Arc<Proc>,
    /// Its job (its cancel flag ends a wait for room in the worker's queue).
    job: Arc<Shared>,
    id: u32,
    replies: Receiver<Reply>,
    /// "Tracker 2, Forward, 61 frames", for the log.
    name: String,
    /// It ended as it should (`done`).
    finished: bool,
    /// It counts in the worker's `owed` (waiting for its last results).
    owing: bool,
}

impl Process {
    /// A stream for the job sharing `job` on the shared worker (started if
    /// none runs); `what` says which, for the log.
    fn start(job: Arc<Shared>, what: &str) -> Result<Self> {
        let proc = Proc::current()?;
        let id = proc.next.fetch_add(1, Ordering::Relaxed);
        let (tx, replies) = channel();
        {
            let mut routes = proc.routes.lock().unwrap_or_else(PoisonError::into_inner);
            if proc.ready.load(Ordering::Relaxed) {
                let _ = tx.send(Reply::Ready);
            }
            routes.insert(id, tx);
        }
        proc.open.fetch_add(1, Ordering::Relaxed);
        tracing::info!("CoTracker worker {}: stream {id} for {what}", proc.name);
        Ok(Self { proc, job, id, replies, name: what.to_string(), finished: false, owing: false })
    }

    /// Send a message (`tag`, the stream's number, `payload`), whole: into
    /// the worker's queue, waiting for room while it tracks (and counted as
    /// waiting on it then). A cancel ends the wait (the message is dropped).
    fn send(&mut self, tag: u8, payload: &[u8]) -> Result<()> {
        let mut msg = Vec::with_capacity(5 + payload.len());
        msg.push(tag);
        msg.extend(self.id.to_le_bytes());
        msg.extend_from_slice(payload);
        let Some(queue) = self.proc.queue.lock().unwrap_or_else(PoisonError::into_inner).clone() else { return Err(self.stopped(None)) };
        let mut waiting = false;
        let sent = loop {
            if self.proc.dead.load(Ordering::Relaxed) {
                break Err(self.stopped(None));
            }
            match queue.try_send(msg) {
                Ok(()) => break Ok(()),
                Err(std::sync::mpsc::TrySendError::Full(back)) => {
                    if self.job.cancel.load(Ordering::Relaxed) {
                        break Ok(());
                    }
                    if !waiting {
                        waiting = true;
                        self.proc.owed.fetch_add(1, Ordering::Relaxed);
                    }
                    msg = back;
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => break Err(self.stopped(None)),
            }
        };
        if waiting {
            self.proc.owed.fetch_sub(1, Ordering::Relaxed);
        }
        sent
    }

    /// Its frames end: from now on it waits for its last results (the watchdog counts it).
    fn end(&mut self) -> Result<()> {
        self.send(b'E', &[])?;
        if !self.owing {
            self.owing = true;
            self.proc.owed.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Why the worker can't be talked to any more: it was stuck (its watchdog
    /// stopped it), or it stopped by itself (and what it said last). `why`:
    /// what failed here.
    fn stopped(&self, why: Option<&dyn std::fmt::Display>) -> anyhow::Error {
        if self.proc.hung.load(Ordering::Relaxed) {
            return anyhow!("CoTracker stopped answering.");
        }
        // (A moment for its last words to arrive.)
        std::thread::sleep(Duration::from_millis(100));
        let why = why.map_or(String::new(), |e| format!(" ({e})"));
        anyhow!("the CoTracker worker stopped{why}: {}", self.last_words(3, " | "))
    }

    fn last_words(&self, n: usize, sep: &str) -> String {
        self.proc.last_words(n, sep)
    }
}

impl Drop for Process {
    /// The stream closes: dropped by the worker if it hadn't finished (its
    /// job was cancelled or failed), and taken off the worker's streams.
    fn drop(&mut self) {
        if !self.finished && !self.proc.dead.load(Ordering::Relaxed) {
            // (Best effort, never waiting: a worker that reads again drops it; one that doesn't is stopped by its watchdog.)
            let mut msg = vec![b'X'];
            msg.extend(self.id.to_le_bytes());
            if let Some(q) = self.proc.queue.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
                let _ = q.try_send(msg);
            }
        }
        if self.owing {
            self.proc.owed.fetch_sub(1, Ordering::Relaxed);
        }
        self.proc.routes.lock().unwrap_or_else(PoisonError::into_inner).remove(&self.id);
        self.proc.open.fetch_sub(1, Ordering::Relaxed);
        let how = if self.finished { "done" } else { "dropped" };
        tracing::info!("CoTracker worker {}: stream {} for {} {how}", self.proc.name, self.id, self.name);
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
        worker.send(b'O', format!("{header}\n").as_bytes())?;

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
                    if i > 0 && !self.wait_taking(f, || decoder = None, &mut worker, &stream, &queries)? {
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
                        if f != start && !self.wait_taking(f, || {}, &mut worker, &stream, &queries)? {
                            return Ok(());
                        }
                        self.send_frame(&mut worker, &mut stream, grid, &rgb, &queries)?;
                    }
                    hi = lo - 1;
                }
            }
        }
        worker.end()?;
        // (A worker that stops answering is stopped by its watchdog: the replies end.)
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
        worker.send(b'F', rgb)?;
        self.take_replies(worker, stream, queries)
    }

    /// Wait for frame `f` to be within the catch-up limit ([`Self::wait_for`]),
    /// taking the results that come in meanwhile (so a job waiting at the
    /// playhead shows all it has tracked). Ok(false): cancelled.
    fn wait_taking(&mut self, f: FrameIndex, park: impl FnMut(), worker: &mut Process, stream: &Stream, queries: &[usize]) -> Result<bool> {
        let mut failed = None;
        let go = self.wait_while(f, park, |me| match me.take_replies(worker, stream, queries) {
            Ok(()) => true,
            Err(e) => {
                failed = Some(e);
                false
            }
        });
        failed.map_or(Ok(go), Err)
    }

    /// Take whatever results have come back.
    fn take_replies(&mut self, worker: &mut Process, stream: &Stream, queries: &[usize]) -> Result<()> {
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
