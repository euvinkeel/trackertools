//! The CoTracker3 method end to end: the tracker's jobs decode the sprite
//! fixture, resample it through the view into the model's crops, and a
//! Python worker (editor/cotracker_worker.py, v1's online engine) tracks the
//! look's point forward and backward. On a CPU a short stretch is enough to
//! prove the plumbing.
//!
//! Skipped unless Python with torch (and av, opencv: v1's engine imports
//! them) and the CoTracker3 weights (`scaled_online.pth`: in torch hub's
//! cache, or `TT_COTRACKER_WEIGHTS`) are there, and the fixture is
//! generated (`cargo xtask fixtures`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bevy_ecs::name::Name;
use tt_core::op::{OpError, Output};
use tt_core::signal::{FrameState, SignalStore};
use tt_core::sketch::BOX_CHANNELS;
use tt_core::span::{Span, set_span};
use tt_core::transport::Transport;
use tt_core::view::SourceSize;
use tt_core::{AppBuilder, CoreModules};
use tt_media::{DecodeOptions, VideoIndex};
use tt_track::look::Look;
use tt_track::runner::{Footage, TrackStatus, coverage, settled};
use tt_track::{Method, TrackModule, Tracker};

fn fixture() -> Option<PathBuf> {
    let name = "sprite_1080p60.mp4";
    if let Some(dir) = std::env::var_os("TT_FIXTURES") {
        return Some(PathBuf::from(dir).join(name)).filter(|p| p.exists());
    }
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().map(|d| d.join("fixtures").join(name)).find(|p| p.exists())
}

/// Whether the worker can run here: Python with torch, av and opencv, and the weights.
fn worker_ready() -> Result<(), String> {
    let (python, _) = tt_track::job::worker_command();
    let check = "import os, torch, av, cv2; p = os.environ.get('TT_COTRACKER_WEIGHTS') or os.path.join(torch.hub.get_dir(), 'checkpoints', 'scaled_online.pth'); print(os.path.exists(p))";
    let out = std::process::Command::new(&python).args(["-c", check]).output().map_err(|e| format!("no Python ({}): {e}", python.display()))?;
    match String::from_utf8_lossy(&out.stdout).trim() {
        "True" => Ok(()),
        "False" => Err("no CoTracker3 weights (scaled_online.pth in torch hub's cache, or TT_COTRACKER_WEIGHTS)".into()),
        _ => Err(format!("Python lacks torch, av or opencv: {}", String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or_default())),
    }
}

fn truth(f: i64) -> [f64; 2] {
    let t = f as f64 / 60.0;
    let p = [950.0 + 500.0 * (0.9 * t).sin() + 60.0 * (5.3 * t).sin(), 530.0 + 300.0 * (1.3 * t + 0.7).sin() + 40.0 * (4.1 * t).sin()];
    p.map(|v| 2.0 * (v.floor() / 2.0).floor() + 10.5)
}

#[test]
fn cotracker_follows_the_sprite_both_ways_through_the_view() {
    let Some(fixture) = fixture() else {
        eprintln!("skipped: sprite_1080p60.mp4 not found (cargo xtask fixtures)");
        return;
    };
    if let Err(why) = worker_ready() {
        eprintln!("skipped: {why}");
        return;
    }
    let mut app = AppBuilder::new();
    app.add_module(CoreModules).add_module(TrackModule);
    let mut core = app.build();
    let index = Arc::new(VideoIndex::open(&fixture).expect("fixture opens"));
    let w = &mut core.world;
    {
        let mut t = w.resource_mut::<Transport>();
        t.fps = index.fps;
        t.frame_count = index.frame_count();
    }
    w.insert_resource(SourceSize { width: index.width as f64, height: index.height as f64 });
    w.insert_resource(Footage { original: index, proxy: None, decode: DecodeOptions::default() });
    // A rough pass ~5 px off the sprite in a 70 px box, alive on frames 590–650 only.
    let sig = w.resource_mut::<SignalStore>().create(BOX_CHANNELS);
    {
        let mut store = w.resource_mut::<SignalStore>();
        let s = store.get_mut(sig).expect("created");
        for f in 0..1200 {
            let c = truth(f);
            let (x, y) = (c[0] + 4.0 * (f as f64 * 0.05).sin(), c[1] - 3.0);
            s.set(f, &[x, y, x - 35.0, y - 35.0, x + 35.0, y + 35.0].map(|v| v as f32));
        }
    }
    let guide = w.spawn((Name::new("Guide"), Output(sig))).id();
    set_span(w, guide, Span::new(590, 650));
    let op = tt_track::add_tracker_with_look(w, guide, Look::new(600, truth(600), [10.5, 10.5])).expect("tracker");
    let params = Tracker { method: Method::CoTracker, ..w.get::<Tracker>(op).expect("tracker").clone() };
    w.entity_mut(op).insert(params);

    let start = Instant::now();
    for _ in 0..3 {
        core.run_pre_ui();
    }
    let mut both = false;
    while !settled(&core.world, op) {
        assert!(start.elapsed() < Duration::from_secs(900), "still tracking");
        let st = core.world.get::<TrackStatus>(op).cloned().unwrap_or_default();
        both |= st.forward.is_some() && st.backward.is_some();
        core.run_pre_ui();
        std::thread::sleep(Duration::from_millis(20));
    }
    let secs = start.elapsed().as_secs_f64();
    let w = &core.world;
    assert!(w.get::<OpError>(op).is_none(), "{:?}", w.get::<OpError>(op));
    assert_eq!(coverage(w, op), Some(590..651), "both ways over the guide's span");
    assert!(both, "a forward and a backward job ran");
    let out = w.resource::<SignalStore>().get(w.get::<Output>(op).expect("output").0).expect("signal");
    let mut e: Vec<f64> = (590..651).map(|f| out.get(f).map_or(f64::INFINITY, |v| (v[0] as f64 - truth(f)[0]).hypot(v[1] as f64 - truth(f)[1]))).collect();
    let flagged = (590..651).filter(|f| out.get(*f).is_some_and(|v| tt_track::flags(v) != 0)).count();
    assert!((590..651).all(|f| out.state(f) == FrameState::Valid));
    e.sort_by(f64::total_cmp);
    eprintln!("CoTracker3: 61 frames in {secs:.1} s; error median {:.2} px, p95 {:.2}, max {:.2}; {flagged} flagged", e[e.len() / 2], e[e.len() * 95 / 100], e[e.len() - 1]);
    assert!(e[e.len() / 2] < 1.5, "median {:.2} px", e[e.len() / 2]);
    assert!(e[e.len() - 1] < 4.0, "max {:.2} px", e[e.len() - 1]);
    assert!(out.get(600).is_some_and(|v| (v[0] as f64 - truth(600)[0]).abs() < 1e-3), "the look's frame is pinned");
}
