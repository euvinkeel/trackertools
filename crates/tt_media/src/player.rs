//! Background decode service (DESIGN §13 step 3): keeps the frames around the
//! playhead in the [`FrameCache`] so stepping, scrubbing and playback read
//! from memory.
//!
//! The app posts a [`Want`] every frame (latest wins). The worker does one
//! frame of work per iteration, re-reading the newest `Want` in between, in
//! priority order:
//! 1. the wanted frame and a read-ahead window (larger while playing, scaled by rate);
//! 2. while paused, the frames just behind the playhead, filled one decode
//!    group (keyframe → target) at a time so backward steps hit the cache;
//! 3. otherwise it sleeps until the next request.
//!
//! Spawning an ffmpeg stream costs ~100–300 ms (spike S1), so a running stream
//! is reused whenever it will reach the next missing frame soon.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::cache::{FrameCache, FrameData};
use crate::ffmpeg::{DecodeOptions, FrameStream};
use crate::index::VideoIndex;

/// Frames a running stream may read (and cache) to reach a target, rather
/// than respawning. ~0.15 s of decoding at ~650 fps: cheaper than a spawn.
const REUSE_GAP: usize = 90;
/// Frames kept decoded ahead of the playhead while paused.
const AHEAD_PAUSED: usize = 12;
/// Frames kept decoded behind the playhead while paused (backward stepping).
const BEHIND_PAUSED: usize = 120;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Want {
    /// Presented frame (index into `VideoIndex::frames`) to show.
    pub frame: usize,
    pub playing: bool,
    /// Playback rate (fraction of real time).
    pub rate: f64,
}

#[derive(Clone, Debug, Default)]
pub struct PlayerStats {
    pub decoded: u64,
    pub spawns: u64,
    pub last_spawn_ms: f64,
    pub cached_frames: usize,
    pub cached_bytes: usize,
    pub error: Option<String>,
}

struct Shared {
    cache: Mutex<FrameCache>,
    stats: Mutex<PlayerStats>,
}

pub struct Player {
    shared: Arc<Shared>,
    tx: Option<Sender<Want>>,
    thread: Option<JoinHandle<()>>,
    last_want: Option<Want>,
}

impl Player {
    pub fn new(index: Arc<VideoIndex>, opts: DecodeOptions, budget_bytes: usize) -> Self {
        let shared = Arc::new(Shared { cache: Mutex::new(FrameCache::new(budget_bytes)), stats: Default::default() });
        let (tx, rx) = channel();
        let worker = Worker { index, opts, shared: shared.clone(), stream: None, scratch: Vec::new() };
        let thread = std::thread::Builder::new().name("decode".into()).spawn(move || worker.run(rx)).expect("spawn decode thread");
        Self { shared, tx: Some(tx), thread: Some(thread), last_want: None }
    }

    /// Tell the worker what the app shows now. Cheap; call every frame.
    pub fn want(&mut self, want: Want) {
        if self.last_want != Some(want) {
            self.last_want = Some(want);
            if let Some(tx) = &self.tx {
                let _ = tx.send(want);
            }
        }
    }

    pub fn frame(&self, p: usize) -> Option<FrameData> {
        self.shared.cache.lock().unwrap().get(p)
    }

    /// The exact frame if cached, else the nearest cached one.
    pub fn frame_or_nearest(&self, p: usize) -> Option<(usize, FrameData)> {
        let cache = self.shared.cache.lock().unwrap();
        cache.get(p).map(|d| (p, d)).or_else(|| cache.nearest(p))
    }

    /// Cached frames as inclusive runs of presented indices.
    pub fn cached_ranges(&self) -> Vec<(usize, usize)> {
        self.shared.cache.lock().unwrap().ranges()
    }

    pub fn stats(&self) -> PlayerStats {
        let mut s = self.shared.stats.lock().unwrap().clone();
        let cache = self.shared.cache.lock().unwrap();
        s.cached_frames = cache.len();
        s.cached_bytes = cache.bytes();
        s
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.tx.take(); // closes the channel; the worker exits
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Worker {
    index: Arc<VideoIndex>,
    opts: DecodeOptions,
    shared: Arc<Shared>,
    stream: Option<FrameStream>,
    scratch: Vec<u8>,
}

impl Worker {
    fn run(mut self, rx: Receiver<Want>) {
        let mut want = Want { frame: 0, playing: false, rate: 1.0 };
        loop {
            // Newest request wins.
            loop {
                match rx.try_recv() {
                    Ok(w) => want = w,
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                }
            }
            match self.step(want) {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => {
                    self.stream = None;
                    self.shared.stats.lock().unwrap().error = Some(format!("{e:#}"));
                }
            }
            // Idle (or after an error): wait for the next request.
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(w) => want = w,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    /// Do one frame of work. Returns whether there was work to do.
    fn step(&mut self, want: Want) -> anyhow::Result<bool> {
        let n = self.index.frames.len();
        let fps = self.index.fps.as_f64();
        let ahead = if want.playing { ((fps * want.rate * 0.75) as usize).max(30) } else { AHEAD_PAUSED };
        let last = (want.frame + ahead).min(n - 1);

        // 1. The wanted frame and the read-ahead window.
        let missing_ahead = {
            let cache = self.shared.cache.lock().unwrap();
            (want.frame..=last).find(|p| !cache.contains(*p))
        };
        if let Some(m) = missing_ahead {
            self.read_toward(m, m, want.frame)?;
            return Ok(true);
        }

        // 2. Paused: the frames just behind the playhead, a decode group at a time.
        if !want.playing && want.frame > 0 {
            let missing_behind = {
                let cache = self.shared.cache.lock().unwrap();
                (want.frame.saturating_sub(BEHIND_PAUSED)..want.frame).rev().find(|p| !cache.contains(*p))
            };
            if let Some(m) = missing_behind {
                let start = self.index.group_start(m);
                self.read_toward(start, m, want.frame)?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Make progress toward caching frame `target`: reuse the running stream
    /// if it will get there soon, otherwise (re)start one at `start`
    /// (≤ target). Reads exactly one frame.
    fn read_toward(&mut self, start: usize, target: usize, playhead: usize) -> anyhow::Result<()> {
        let reusable = self.stream.as_ref().is_some_and(|s| s.position() <= target && target - s.position() <= REUSE_GAP);
        if !reusable {
            let t = Instant::now();
            self.stream = Some(FrameStream::start(&self.index, start, &self.opts)?);
            let mut stats = self.shared.stats.lock().unwrap();
            stats.spawns += 1;
            stats.last_spawn_ms = t.elapsed().as_secs_f64() * 1e3; // spawn only; first frame arrives later
        }
        let stream = self.stream.as_mut().unwrap();
        match stream.read(&mut self.scratch)? {
            Some(p) => {
                let data: FrameData = Arc::from(self.scratch.as_slice());
                self.shared.cache.lock().unwrap().insert(p, data, playhead);
                self.shared.stats.lock().unwrap().decoded += 1;
            }
            None => self.stream = None, // end of video
        }
        Ok(())
    }
}
