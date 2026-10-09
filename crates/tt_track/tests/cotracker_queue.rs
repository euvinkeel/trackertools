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
    // (Read once, by the first runner pass: the default, three jobs at once.)
    set_env("TT_COTRACKER_JOBS", None);
    // A worker left from another test had other switches: it goes (and none is kept warm).
    tt_track::job::keep_cotracker_warm(false);
    tt_track::job::close_cotracker_worker();
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

/// Several CoTracker trackers track at once, as streams through one worker
/// process (the model on the graphics card once).
#[test]
fn cotracker_jobs_share_one_worker() {
    let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
    let Some((mut core, guide)) = setup(TrackRun::Both) else { return };
    let started = tt_track::job::cotracker_processes_started();
    let a = add(&mut core, guide);
    let b = add(&mut core, guide);
    let c = add(&mut core, guide);
    let mut most = 0;
    run(&mut core, &[a, b, c], |w| {
        let jobs = w.resource::<TrackJobs>().cotracker_workers();
        assert!(jobs <= tt_track::runner::cotracker_jobs(), "{jobs} CoTracker jobs at once");
        most = most.max(jobs);
    });
    let w = &core.world;
    for t in [a, b, c] {
        assert!(w.get::<OpError>(t).is_none(), "{:?}", w.get::<OpError>(t));
        assert_eq!(coverage(w, t), Some(570..631), "both ways over its span");
    }
    assert!(most >= 2, "trackers tracked at once: at most {most} together");
    assert_eq!(tt_track::job::cotracker_processes_started() - started, 1, "one worker process for all of them");
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
        // (The shared worker still hangs: the next one is a new one.)
        tt_track::job::close_cotracker_worker();
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
        paint_resume: None,
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
        Some(Msg::Frames(_) | Msg::Points(_) | Msg::PaintStates(_) | Msg::CursorShapes(_)) | None => panic!("no last word"),
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
        assert!(core.world.resource::<TrackJobs>().cotracker_workers() <= tt_track::runner::cotracker_jobs());
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Catch-up mode, with room for both sides (jobs share one worker): each
/// side tracks up to the playhead and parks there, keeping nothing busy and
/// saying so. The playhead moving on lets them go on; a parked job freed by
/// a new limit goes on (not started again), and both sides finish.
#[test]
fn catch_up_waits_quietly_at_the_playhead() {
    let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
    let Some((mut core, guide)) = setup(TrackRun::Paused) else { return };
    let op = add(&mut core, guide);
    core.world.get_mut::<Tracker>(op).expect("tracker").follow_playhead = true;
    core.world.resource_mut::<Transport>().seek(615);
    set_run(&mut core.world, op, TrackRun::Both);

    // Forward tracks to the playhead and parks there; backward's frames are behind it: it parks at once.
    let parked = |w: &World| {
        let s = status(w, op);
        s.forward.is_some_and(|f| f.waiting) && s.backward.is_none_or(|b| b.waiting)
    };
    run_until(&mut core, "the sides never parked at the playhead", parked);
    let rest = Instant::now();
    while rest.elapsed() < Duration::from_millis(1500) {
        core.run_pre_ui();
        let (w, jobs) = (&core.world, core.world.resource::<TrackJobs>());
        let s = status(w, op);
        assert!(!jobs.active(), "only waiting at the playhead: nothing keeps the app busy ({s:?})");
        assert!(!s.queued && !s.waits_for_cotracker, "{s:?}");
        std::thread::sleep(Duration::from_millis(10));
    }

    // The playhead goes before the anchor: backward tracks to it and parks there.
    core.world.resource_mut::<Transport>().seek(580);
    run_until(&mut core, "backward never tracked to the playhead", |w| status(w, op).backward.is_some_and(|s| s.waiting && s.at <= 581));
    let backward = status(&core.world, op).backward.expect("a backward job");
    let rest = Instant::now();
    while rest.elapsed() < Duration::from_millis(1500) {
        core.run_pre_ui();
        assert!(!core.world.resource::<TrackJobs>().active(), "{:?}", status(&core.world, op));
        std::thread::sleep(Duration::from_millis(10));
    }
    // ... then freed by a new limit (not following the playhead any more): it goes on, and so does forward.
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

/// Wait (up to 20 s) until the engine is as `done` says; its last state.
fn engine_until(what: &str, done: impl Fn(&tt_track::job::CoTrackerEngine) -> bool) -> tt_track::job::CoTrackerEngine {
    let start = Instant::now();
    loop {
        let e = tt_track::job::cotracker_engine();
        if done(&e) {
            return e;
        }
        assert!(start.elapsed() < Duration::from_secs(20), "{what}: still {e:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Started early, the engine is ready before the first CoTracker tracker
/// needs it, and that tracker uses it: no other worker starts, and the job
/// never waits for a model to load.
#[test]
fn the_engine_started_early_is_the_one_trackers_use() {
    use tt_track::job::CoTrackerEngine;
    let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
    let Some((mut core, guide)) = setup(TrackRun::Both) else { return };
    assert_eq!(tt_track::job::cotracker_engine(), CoTrackerEngine::Off);
    let started = tt_track::job::cotracker_processes_started();
    tt_track::job::warm_up_cotracker().expect("the worker starts");
    assert_eq!(engine_until("ready", |e| matches!(e, CoTrackerEngine::Ready(_))), CoTrackerEngine::Ready("fake".into()));
    let op = add(&mut core, guide);
    let mut loaded = false;
    run(&mut core, &[op], |w| loaded |= status(w, op).forward.is_some_and(|s| s.phase == Phase::Loading));
    assert!(!loaded, "the tracker never waited for the model to load");
    assert_eq!(coverage(&core.world, op), Some(570..631));
    assert_eq!(tt_track::job::cotracker_processes_started() - started, 1, "one worker: the one started early");
    // Kept loaded with nothing using it.
    assert!(matches!(tt_track::job::cotracker_engine(), CoTrackerEngine::Ready(_)));
    quiet(&mut core);
    tt_track::job::keep_cotracker_warm(false);
    tt_track::job::close_cotracker_worker();
}

/// An engine that can't start says why (for the app to show), and nothing
/// panics: a worker that fails as it loads, and a Python that isn't there.
#[test]
fn an_engine_that_cannot_start_says_why() {
    use tt_track::job::CoTrackerEngine;
    let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
    let Some((_core, _guide)) = setup(TrackRun::Both) else { return };
    set_env("TT_FAKE_COTRACKER_FAIL", Some("1".as_ref()));
    tt_track::job::warm_up_cotracker().expect("the process starts");
    let e = engine_until("failed", |e| matches!(e, CoTrackerEngine::Failed(_)));
    assert!(matches!(&e, CoTrackerEngine::Failed(why) if why.contains("fake CoTracker failure")), "{e:?}");
    set_env("TT_FAKE_COTRACKER_FAIL", None);
    tt_track::job::close_cotracker_worker();
    assert_eq!(tt_track::job::cotracker_engine(), CoTrackerEngine::Off, "closed anew: no failure kept");

    let python = std::env::var_os("TT_PYTHON");
    set_env("TT_PYTHON", Some(r"C:\nonexistent\python.exe".as_ref()));
    assert!(tt_track::job::warm_up_cotracker().is_err());
    assert!(matches!(tt_track::job::cotracker_engine(), CoTrackerEngine::Failed(why) if why.contains("starting the CoTracker worker")));
    set_env("TT_PYTHON", python.as_deref());
    tt_track::job::keep_cotracker_warm(false);
    tt_track::job::close_cotracker_worker();
}

/// A paint tracker end to end. The fake answers each point where it was
/// asked in the crop, and the crop follows the guide (the sprite), so the
/// points move with the sprite. Painted on 600 and again on 612 on the
/// sprite: the points make it, and it follows the sprite. A third paint on
/// 622, 40 px off the sprite: none of the points get there, so 612–622 is
/// lost, and from 622 it follows the sprite from that paint. Its points
/// come out for the app to draw.
#[test]
fn a_paint_tracker_follows_its_paints() {
    let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
    let Some((mut core, guide)) = setup(TrackRun::Both) else { return };
    core.world.resource_mut::<NewTrackers>().method = Method::Paint;
    let paint = |f: i64, at: [f64; 2]| {
        let (c, h, mask) = tt_track::tool::paint_look(&[at, [at[0] + 12.0, at[1] + 4.0]], 9.0);
        let mut look = Look::new(f, c, h);
        look.mask = mask;
        look
    };
    let first = truth(600);
    let op = tt_track::add_tracker_with_look(&mut core.world, guide, paint(600, first)).expect("tracker");
    set_span(&mut core.world, op, Span::new(570, 630));
    tt_track::add_look(&mut core.world, op, paint(612, truth(612))).expect("a reset paint");
    let off = [truth(622)[0] + 40.0, truth(622)[1]];
    tt_track::add_look(&mut core.world, op, paint(622, off)).expect("a paint off the sprite");
    run(&mut core, &[op], |_| {});
    let w = &core.world;
    assert!(w.get::<OpError>(op).is_none(), "{:?}", w.get::<OpError>(op));
    assert_eq!(coverage(w, op), Some(570..631), "both ways over its span");
    let sig = w.resource::<SignalStore>().get(w.get::<Output>(op).expect("output").0).expect("signal");
    let at = |f: i64| sig.get(f).map(|v| ([v[0] as f64, v[1] as f64], tt_track::flags(v) != 0)).expect("a result");
    // A paint's centre, moved as the sprite moved since frame `from`.
    let centre = |c: [f64; 2], from: i64, f: i64| [c[0] + 6.0 + truth(f)[0] - truth(from)[0], c[1] + 2.0 + truth(f)[1] - truth(from)[1]];
    let near = |a: [f64; 2], b: [f64; 2]| (a[0] - b[0]).hypot(a[1] - b[1]) < 1.5;
    for f in [570, 585, 600, 606, 612] {
        let want = centre(first, 600, f);
        assert!(near(at(f).0, want) && !at(f).1, "frame {f}: with the sprite: {:?} vs {want:?}", at(f));
    }
    assert!((613..622).all(|f| at(f).1), "613–621: no point reaches the paint on 622: lost");
    for f in [622, 626, 630] {
        let want = centre(off, 622, f);
        assert!(near(at(f).0, want), "frame {f}: from the paint on 622, with the sprite: {:?} vs {want:?}", at(f));
    }
    // Its points, for the app: on 606 the cohort of the first stretch, all kept.
    let points = w.get::<tt_track::runner::PaintPoints>(op).expect("its points");
    let marks = points.0.get(&606).expect("points on 606");
    assert!(!marks.is_empty() && marks.iter().all(|m| m.kept), "{marks:?}");
    assert!(points.0.get(&616).expect("points on 616").iter().all(|m| !m.kept), "613–621: none of them make it");
    quiet(&mut core);
}


/// A reset point added between two others re-tracks only from the one
/// before it to the one after it (on request: "limit recomputation from
/// when paints/reset points have actually changed, and their immediate
/// neighbors only"): the rest stays valid, and no job runs on the other side.
#[test]
fn a_new_reset_point_retracks_only_between_its_neighbours() {
    let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
    let Some((mut core, guide)) = setup(TrackRun::Both) else { return };
    let op = add(&mut core, guide);
    set_span(&mut core.world, op, Span::new(570, 680));
    for f in [620, 660] {
        tt_track::set_reset_point(&mut core.world, op, Look::new(f, truth(f), [6.0, 6.0]));
    }
    run(&mut core, &[op], |_| {});
    assert_eq!(coverage(&core.world, op), Some(570..681));
    tt_track::set_reset_point(&mut core.world, op, Look::new(640, truth(640), [6.0, 6.0]));
    let mut sides = Vec::new();
    run(&mut core, &[op], |w| {
        let s = status(w, op);
        if let Some(f) = s.forward {
            sides.push((f.from, f.to));
        }
        assert!(s.backward.is_none(), "nothing to redo before the anchor");
        // Outside 620–660 the results stay valid while it re-tracks.
        let sig = w.resource::<SignalStore>().get(w.get::<Output>(op).expect("output").0).expect("signal");
        for f in [575, 600, 619, 661, 680] {
            assert_eq!(sig.state(f), tt_core::signal::FrameState::Valid, "frame {f} kept");
        }
    });
    sides.dedup();
    assert_eq!(sides, vec![(620, 660)], "one job, from the reset point before to the one after");
    assert_eq!(coverage(&core.world, op), Some(570..681));
    quiet(&mut core);
}

/// A paint changed: the paint tracker resumes on the paint before it (its
/// cohort and motion there), not from its first paint; the frames before
/// that stay; and it ends where a fresh tracker with the same paints does.
#[test]
fn a_changed_paint_resumes_on_the_paint_before() {
    let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
    let Some((mut core, guide)) = setup(TrackRun::Both) else { return };
    core.world.resource_mut::<NewTrackers>().method = Method::Paint;
    let paint = |f: i64, at: [f64; 2]| {
        let (c, h, mask) = tt_track::tool::paint_look(&[at, [at[0] + 12.0, at[1] + 4.0]], 9.0);
        let mut look = Look::new(f, c, h);
        look.mask = mask;
        look
    };
    let make = |core: &mut Core, last: [f64; 2]| {
        let op = tt_track::add_tracker_with_look(&mut core.world, guide, paint(600, truth(600))).expect("tracker");
        set_span(&mut core.world, op, Span::new(570, 640));
        tt_track::add_look(&mut core.world, op, paint(612, truth(612))).expect("paint");
        let l = tt_track::add_look(&mut core.world, op, paint(622, last)).expect("paint");
        (op, l)
    };
    let (op, last) = make(&mut core, [truth(622)[0] + 40.0, truth(622)[1]]);
    run(&mut core, &[op], |_| {});
    assert_eq!(coverage(&core.world, op), Some(570..641));
    // The paint on 622 moved onto the sprite.
    let moved = paint(622, truth(622));
    tt_core::history::edit(&mut core.world, "move", |tx| tx.modify::<Look>(last, |l| *l = moved.clone()));
    let mut froms = Vec::new();
    run(&mut core, &[op], |w| {
        let s = status(w, op);
        if let Some(f) = s.forward {
            froms.push(f.from);
        }
        assert!(s.backward.is_none(), "nothing to redo before the anchor");
        let sig = w.resource::<SignalStore>().get(w.get::<Output>(op).expect("output").0).expect("signal");
        for f in [575, 600, 612] {
            assert_eq!(sig.state(f), tt_core::signal::FrameState::Valid, "frame {f} kept");
        }
    });
    froms.dedup();
    assert_eq!(froms, vec![612], "resumed on the paint before the changed one");
    // A fresh tracker with the same paints, tracked from its first paint.
    let (fresh, _) = make(&mut core, truth(622));
    run(&mut core, &[fresh], |_| {});
    let w = &core.world;
    let at = |e: Entity, f: i64| w.resource::<SignalStore>().get(w.get::<Output>(e).expect("output").0).and_then(|s| s.get(f).map(|v| [v[0] as f64, v[1] as f64, tt_track::flags(v) as f64])).expect("a value");
    for f in 570..641 {
        let (a, b) = (at(op, f), at(fresh, f));
        assert!((a[0] - b[0]).hypot(a[1] - b[1]) < 0.5 && a[2] == b[2], "frame {f}: resumed {a:?} vs fresh {b:?}");
    }
    quiet(&mut core);
}
