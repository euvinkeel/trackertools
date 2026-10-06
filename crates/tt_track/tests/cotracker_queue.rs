//! CoTracker's workers, run by a fake (tests/fake_cotracker_worker.py: the
//! worker's protocol in standard-library Python, no model), so nothing here
//! touches the graphics card. On the sprite fixture (`cargo xtask fixtures`)
//! with a Python on PATH; skipped without either.
//!
//! - Two CoTracker trackers asked both ways: one worker is alive at a time
//!   (a tracker's two sides one after the other too); the other tracker
//!   says it waits its turn, and both finish.
//! - Asked only backward, a tracker's anchor needs no worker.
//! - A paused tracker's job lets its worker go within seconds, even while
//!   the worker loads its model or is stuck.
//! - A worker that fails stops its tracker with an error, which doesn't
//!   start again by itself; asked again (the same way), it tracks.
//! - (Any job) a job thread that panics ends with a failure, as any error.
//! - Catch-up mode: a side that can only wait at the playhead keeps nothing
//!   busy; the playhead moving to it takes the worker from the job parked on
//!   the other side; a parked job that a new limit frees keeps its worker.
//! - Results arriving count as a change to save, at most every 5 s.
//!
//! The fake is chosen through the environment (`TT_PYTHON`,
//! `TT_COTRACKER_WORKER`), which the whole process shares: these tests take
//! turns, and change it only while no job runs. They test the default of
//! one worker at a time (`TT_COTRACKER_JOBS` is cleared).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bevy_ecs::name::Name;
use bevy_ecs::prelude::*;
use tt_core::history::History;
use tt_core::op::{OpError, Output};
use tt_core::signal::SignalStore;
use tt_core::sketch::BOX_CHANNELS;
use tt_core::span::{Span, set_span};
use tt_core::time::WallClock;
use tt_core::transport::Transport;
use tt_core::view::SourceSize;
use tt_core::{AppBuilder, Core, CoreModules};
use tt_media::{DecodeOptions, VideoIndex};
use tt_track::job::Phase;
use tt_track::look::Look;
use tt_track::runner::{Footage, RESULTS_SAVED_EVERY, TrackJobs, TrackStatus, coverage, settled};
use tt_track::{Method, NewTrackers, TrackModule, TrackRun, Tracker, set_run};

const NAME: &str = "sprite_1080p60.mp4";

/// The environment is the process's: one test at a time.
static TURN: Mutex<()> = Mutex::new(());

fn fixture() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("TT_FIXTURES") {
        return Some(PathBuf::from(dir).join(NAME)).filter(|p| p.exists());
    }
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().map(|d| d.join("fixtures").join(NAME)).find(|p| p.exists())
}

fn truth(f: i64) -> [f64; 2] {
    let t = f as f64 / 60.0;
    let p = [950.0 + 500.0 * (0.9 * t).sin() + 60.0 * (5.3 * t).sin(), 530.0 + 300.0 * (1.3 * t + 0.7).sin() + 40.0 * (4.1 * t).sin()];
    p.map(|v| 2.0 * (v.floor() / 2.0).floor() + 10.5)
}

/// A Python 3 to run the fake with: `python` on PATH, or the usual install.
fn python() -> Option<PathBuf> {
    let candidates: &[&str] = if cfg!(windows) { &["python", r"C:\Python313\python.exe"] } else { &["python3", "python"] };
    candidates.iter().map(PathBuf::from).find(|p| {
        std::process::Command::new(p).args(["-c", "import sys; print(sys.version_info[0])"]).output().is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "3")
    })
}

/// Set (or with None, remove) an environment variable.
fn set_env(key: &str, value: Option<&std::ffi::OsStr>) {
    // SAFETY: the tests here take turns (TURN) and change the environment
    // only while none of their jobs run; nothing else in this process reads it then.
    unsafe {
        match value {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }
}

/// The fixture open with a rough guide over it, new trackers CoTracker ones
/// asked `run`, and the fake worker in place of the real one. None (skipped)
/// without the fixture or a Python.
fn setup(run: TrackRun) -> Option<(Core, Entity)> {
    let Some(fixture) = fixture() else {
        eprintln!("skipped: {NAME} not found (cargo xtask fixtures, or set TT_FIXTURES)");
        return None;
    };
    let Some(python) = python() else {
        eprintln!("skipped: no Python 3 here (python on PATH) to run the fake CoTracker worker");
        return None;
    };
    set_env("TT_PYTHON", Some(python.as_os_str()));
    set_env("TT_COTRACKER_WORKER", Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("fake_cotracker_worker.py").as_os_str()));
    set_env("TT_FAKE_COTRACKER_DELAY", Some("0.005".as_ref()));
    set_env("TT_FAKE_COTRACKER_FAIL", None);
    set_env("TT_FAKE_COTRACKER_HANG", None);
    // (Read once, by the first runner pass: the default, one worker at a time.)
    set_env("TT_COTRACKER_JOBS", None);
    let mut app = AppBuilder::new();
    app.add_module(CoreModules).add_module(TrackModule);
    let mut core = app.build();
    *core.world.resource_mut::<NewTrackers>() = NewTrackers { method: Method::CoTracker, run };
    let index = Arc::new(VideoIndex::open(&fixture).expect("fixture opens"));
    let w = &mut core.world;
    {
        let mut t = w.resource_mut::<Transport>();
        t.fps = index.fps;
        t.frame_count = index.frame_count();
    }
    w.insert_resource(SourceSize { width: index.width as f64, height: index.height as f64 });
    w.insert_resource(Footage { original: index, proxy: None, decode: DecodeOptions::default() });
    let sig = w.resource_mut::<SignalStore>().create(BOX_CHANNELS);
    {
        let mut store = w.resource_mut::<SignalStore>();
        let s = store.get_mut(sig).expect("created");
        for f in 0..1200 {
            let c = truth(f);
            s.set(f, &[c[0], c[1], c[0] - 35.0, c[1] - 35.0, c[0] + 35.0, c[1] + 35.0].map(|v| v as f32));
        }
    }
    let guide = w.spawn((Name::new("Guide"), Output(sig))).id();
    Some((core, guide))
}

/// A tracker on the sprite from frame 600, living on frames 570–630.
fn add(core: &mut Core, guide: Entity) -> Entity {
    let op = tt_track::add_tracker_with_look(&mut core.world, guide, Look::new(600, truth(600), [10.5, 10.5])).expect("tracker");
    set_span(&mut core.world, op, Span::new(570, 630));
    op
}

fn status(w: &World, op: Entity) -> TrackStatus {
    w.get::<TrackStatus>(op).cloned().unwrap_or_default()
}

/// Run frames until every one of `ops` has nothing left to do; `look` sees each frame.
fn run(core: &mut Core, ops: &[Entity], mut look: impl FnMut(&World)) {
    let start = Instant::now();
    for _ in 0..3 {
        core.run_pre_ui();
        look(&core.world);
    }
    while !ops.iter().all(|op| settled(&core.world, *op)) {
        assert!(start.elapsed() < Duration::from_secs(300), "still tracking");
        core.run_pre_ui();
        look(&core.world);
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Wait for every job thread to end (and with it its worker).
fn quiet(core: &mut Core) {
    let start = Instant::now();
    while core.world.resource::<TrackJobs>().threads() > 0 {
        assert!(start.elapsed() < Duration::from_secs(20), "job threads still alive");
        core.run_pre_ui();
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn one_cotracker_worker_at_a_time() {
    let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
    let Some((mut core, guide)) = setup(TrackRun::Both) else { return };
    let a = add(&mut core, guide);
    let b = add(&mut core, guide);
    let (mut most, mut b_waited, mut a_waited) = (0, false, false);
    run(&mut core, &[a, b], |w| {
        let workers = w.resource::<TrackJobs>().cotracker_workers();
        assert!(workers <= 1, "{workers} CoTracker workers at once");
        most = most.max(workers);
        for t in [a, b] {
            let s = status(w, t);
            assert!(!(s.forward.is_some() && s.backward.is_some()), "a tracker's two sides one after the other");
        }
        b_waited |= status(w, b).waits_for_cotracker;
        a_waited |= status(w, a).waits_for_cotracker;
    });
    let w = &core.world;
    for t in [a, b] {
        assert!(w.get::<OpError>(t).is_none(), "{:?}", w.get::<OpError>(t));
        assert_eq!(coverage(w, t), Some(570..631), "both ways over its span");
    }
    assert_eq!(most, 1, "a worker ran");
    assert!(b_waited, "the second tracker said it waits for the first");
    assert!(a_waited, "the first tracker's backward side waited for its forward side");
    quiet(&mut core);
    assert_eq!(core.world.resource::<TrackJobs>().cotracker_workers(), 0);
}

#[test]
fn an_anchor_alone_needs_no_worker() {
    let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
    let Some((mut core, guide)) = setup(TrackRun::Backward) else { return };
    let op = add(&mut core, guide);
    let mut together = false;
    run(&mut core, &[op], |w| {
        let s = status(w, op);
        // (The anchor's job holds no worker, so the backward side's starts beside it.)
        together |= s.forward.is_some() && s.backward.is_some();
        assert!(w.resource::<TrackJobs>().cotracker_workers() <= 1);
    });
    let w = &core.world;
    assert!(w.get::<OpError>(op).is_none(), "{:?}", w.get::<OpError>(op));
    assert_eq!(coverage(w, op), Some(570..601), "backward, and its anchor");
    assert!(together, "the anchor's job and the backward one started together");
    let out = w.resource::<SignalStore>().get(w.get::<Output>(op).expect("output").0).expect("signal");
    assert!(out.get(600).is_some_and(|v| (v[0] as f64 - truth(600)[0]).abs() < 1e-3), "the anchor is the reset point");
    quiet(&mut core);
}

/// A paused tracker's job lets its worker go within seconds, while the
/// worker still loads its model and when it is stuck (not reading the frames
/// it is sent): the next CoTracker job doesn't wait for it.
#[test]
fn a_cancelled_job_lets_its_worker_go() {
    let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
    let Some((mut core, guide)) = setup(TrackRun::Paused) else { return };
    for (hang, phase) in [("load", Phase::Loading), ("frames", Phase::Tracking)] {
        set_env("TT_FAKE_COTRACKER_HANG", Some(hang.as_ref()));
        let op = add(&mut core, guide);
        set_run(&mut core.world, op, TrackRun::Forward);
        let start = Instant::now();
        while status(&core.world, op).forward.is_none_or(|s| s.phase != phase) {
            assert!(start.elapsed() < Duration::from_secs(20), "{hang}: the job never got to {phase:?}");
            core.run_pre_ui();
            std::thread::sleep(Duration::from_millis(5));
        }
        // (Stuck: loading, or writing a frame the worker doesn't read.)
        std::thread::sleep(Duration::from_millis(500));
        core.run_pre_ui();
        assert_eq!(core.world.resource::<TrackJobs>().cotracker_workers(), 1, "{hang}: its worker is alive");
        set_run(&mut core.world, op, TrackRun::Paused);
        let paused = Instant::now();
        while core.world.resource::<TrackJobs>().cotracker_workers() > 0 {
            assert!(paused.elapsed() < Duration::from_secs(6), "{hang}: the worker is still held");
            core.run_pre_ui();
            std::thread::sleep(Duration::from_millis(5));
        }
        eprintln!("{hang}: the worker was let go {:.1} s after the pause", paused.elapsed().as_secs_f64());
        assert!(core.world.get::<OpError>(op).is_none(), "a cancelled job is not an error");
        set_env("TT_FAKE_COTRACKER_HANG", None);
        quiet(&mut core);
    }
}

#[test]
fn a_failed_cotracker_tracks_again_when_asked() {
    let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
    let Some((mut core, guide)) = setup(TrackRun::Paused) else { return };
    set_env("TT_FAKE_COTRACKER_FAIL", Some("1".as_ref()));
    let op = add(&mut core, guide);
    set_run(&mut core.world, op, TrackRun::Forward);
    run(&mut core, &[op], |_| {});
    let error = core.world.get::<OpError>(op).map(|e| e.0.clone());
    assert!(error.as_deref().is_some_and(|e| e.contains("fake CoTracker failure")), "{error:?}");
    quiet(&mut core);
    // It doesn't start again by itself.
    for _ in 0..20 {
        core.run_pre_ui();
        assert_eq!(core.world.resource::<TrackJobs>().busy(), 0, "a failed tracker waits");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(core.world.get::<OpError>(op).is_some());

    // Asked again the same way, it plans again and tracks (the ask is not a change to save).
    set_env("TT_FAKE_COTRACKER_FAIL", None);
    let revision = core.world.resource::<History>().revision();
    set_run(&mut core.world, op, TrackRun::Forward);
    assert_eq!(core.world.resource::<History>().revision(), revision);
    run(&mut core, &[op], |_| {});
    let w = &core.world;
    assert!(w.get::<OpError>(op).is_none(), "{:?}", w.get::<OpError>(op));
    assert_eq!(coverage(w, op), Some(600..631), "forward over its span");
    quiet(&mut core);
}

/// A job thread that panics says so (a failure, as any error), and isn't counted any more.
#[test]
fn a_job_that_panics_reports_a_failure() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tt_track::job::{JobSpec, Msg, Shared, Side, spawn};
    let Some(fixture) = fixture() else {
        eprintln!("skipped: {NAME} not found (cargo xtask fixtures, or set TT_FIXTURES)");
        return;
    };
    let video = Arc::new(VideoIndex::open(&fixture).expect("fixture opens"));
    let spec = JobSpec {
        side: Side::Forward,
        anchor: 600,
        from: 600,
        to: 610,
        resume: None,
        lo: 0,
        // (No guide boxes: reading the anchor's panics.)
        guide: Arc::new(Vec::new()),
        maps: Arc::new(Vec::new()),
        scale: 1.0,
        search: 1.0,
        settings: tt_track::template::Settings { adapt: 0.25, min_score: 0.6, tolerance: tt_track::ncc::Tolerance::default() },
        video: video.clone(),
        k: [1.0, 1.0],
        grid: video,
        decode: DecodeOptions::default(),
        looks: Arc::new(Vec::new()),
        seed: None,
        fuse: true,
        method: Method::Template,
        root: false,
        label: "a test job".into(),
    };
    let (threads, workers) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (tx, rx) = std::sync::mpsc::channel();
    spawn(spec, Arc::new(Shared::new(i64::MAX, 600)), tx, &threads, &workers).join().expect("the thread ends normally");
    let last = rx.try_iter().last();
    match last {
        Some(Msg::Failed(e)) => assert!(e.starts_with("the tracker stopped on an error: "), "{e}"),
        Some(Msg::Finished) => panic!("finished"),
        Some(Msg::Frames(_)) | None => panic!("no last word"),
    }
    assert_eq!((threads.load(Ordering::Relaxed), workers.load(Ordering::Relaxed)), (0, 0));
}

/// Run frames until `done` says so (within 20 s).
fn run_until(core: &mut Core, what: &str, mut done: impl FnMut(&World) -> bool) {
    let start = Instant::now();
    loop {
        core.run_pre_ui();
        if done(&core.world) {
            return;
        }
        assert!(start.elapsed() < Duration::from_secs(20), "{what}");
        assert!(core.world.resource::<TrackJobs>().cotracker_workers() <= 1);
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Catch-up mode: a side that could only wait at the playhead, the worker
/// held by the other side's job (waiting at the playhead too), keeps nothing
/// busy and says it waits for the playhead. The playhead moving to it takes
/// the worker from the parked job. A parked job that a new limit frees keeps
/// its worker, and both sides finish.
#[test]
fn catch_up_waits_quietly_at_the_playhead() {
    let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
    let Some((mut core, guide)) = setup(TrackRun::Paused) else { return };
    let op = add(&mut core, guide);
    core.world.get_mut::<Tracker>(op).expect("tracker").follow_playhead = true;
    core.world.resource_mut::<Transport>().seek(615);
    set_run(&mut core.world, op, TrackRun::Both);

    // Forward tracks to the playhead and waits there; backward's frames are behind it.
    run_until(&mut core, "the forward job never got to the playhead", |w| status(w, op).forward.is_some_and(|s| s.waiting));
    let rest = Instant::now();
    while rest.elapsed() < Duration::from_millis(1500) {
        core.run_pre_ui();
        let (w, jobs) = (&core.world, core.world.resource::<TrackJobs>());
        let s = status(w, op);
        assert!(!jobs.active(), "only waiting at the playhead: nothing keeps the app busy ({s:?})");
        assert!(s.waits_at_playhead && !s.queued && !s.waits_for_cotracker, "{s:?}");
        assert_eq!(jobs.cotracker_workers(), 1, "the waiting forward job keeps its worker: nothing else could use it");
        std::thread::sleep(Duration::from_millis(10));
    }

    // The playhead goes before the anchor: backward can track, and takes the worker from the parked forward job.
    core.world.resource_mut::<Transport>().seek(580);
    run_until(&mut core, "backward never tracked to the playhead", |w| status(w, op).backward.is_some_and(|s| s.waiting));
    let backward = status(&core.world, op).backward.expect("a backward job");
    assert!(status(&core.world, op).forward.is_none(), "the forward job let its worker go");
    // Parked there (after a second) ...
    let rest = Instant::now();
    while rest.elapsed() < Duration::from_millis(1500) {
        core.run_pre_ui();
        assert!(!core.world.resource::<TrackJobs>().active(), "{:?}", status(&core.world, op));
        std::thread::sleep(Duration::from_millis(10));
    }
    // ... then freed by a new limit (not following the playhead any more) as forward wants the worker too: it keeps it.
    core.world.get_mut::<Tracker>(op).expect("tracker").follow_playhead = false;
    run(&mut core, &[op], |w| {
        if let Some(b) = status(w, op).backward {
            assert_eq!(b.from, backward.from, "the freed backward job went on (it was not stopped and started again)");
        }
    });
    let w = &core.world;
    assert!(w.get::<OpError>(op).is_none(), "{:?}", w.get::<OpError>(op));
    assert_eq!(coverage(w, op), Some(570..631), "both ways over its span");
    quiet(&mut core);
}

/// Results arriving count as a change to save (autosave waits for 1.5 s
/// without changes), at most every [`RESULTS_SAVED_EVERY`]: a running
/// tracker's results reach the disk while it runs.
#[test]
fn running_results_are_saved_now_and_then() {
    let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
    let Some((mut core, guide)) = setup(TrackRun::Paused) else { return };
    set_env("TT_FAKE_COTRACKER_DELAY", Some("0.02".as_ref()));
    let op = add(&mut core, guide);
    set_run(&mut core.world, op, TrackRun::Forward);
    // (A clock a second ahead every frame: the job lasts many "seconds".)
    let (mut clock, mut last, mut touched) = (0.0, core.world.resource::<History>().revision(), Vec::new());
    let start = Instant::now();
    while !settled(&core.world, op) || clock < 3.0 {
        assert!(start.elapsed() < Duration::from_secs(60), "still tracking");
        clock += 1.0;
        core.world.resource_mut::<WallClock>().tick(clock);
        core.run_pre_ui();
        let revision = core.world.resource::<History>().revision();
        if revision != last && status(&core.world, op).forward.is_some() {
            touched.push(clock);
        }
        last = revision;
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(touched.len() >= 2, "saved while it ran: {touched:?}");
    assert!(touched.windows(2).all(|t| t[1] - t[0] >= RESULTS_SAVED_EVERY), "at most every {RESULTS_SAVED_EVERY} s: {touched:?}");
    set_env("TT_FAKE_COTRACKER_DELAY", Some("0.005".as_ref()));
    quiet(&mut core);
}
