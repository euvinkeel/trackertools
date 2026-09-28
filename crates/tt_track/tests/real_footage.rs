//! Replays a tracker from a saved project on its video, headless, and
//! writes every frame's result as CSV (for checking the tracker on real
//! footage). Ignored by default: it needs a project and its video.
//!
//! ```text
//! TT_REAL_PROJECT=<copy of a .ttproj> TT_REAL_VIDEO=<video> TT_REAL_TRACKER="Tracker 1" \
//! TT_REAL_OUT=<out.csv> [TT_REAL_DROP="Look (frame"] cargo test -p tt_track --release --test real_footage -- --ignored --nocapture
//! ```
//! `TT_REAL_DROP` deletes the tracker's looks whose names start with it first.
//! Point it at a copy: the project file is only read, never written.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bevy_ecs::name::Name;
use bevy_ecs::prelude::*;
use tt_core::op::Output;
use tt_core::signal::SignalStore;
use tt_core::transport::Transport;
use tt_core::view::SourceSize;
use tt_core::{AppBuilder, CoreModules};
use tt_media::{DecodeOptions, VideoIndex};
use tt_track::look::{Look, looks_of};
use tt_track::runner::{Footage, settled};
use tt_track::{TrackModule, is_tracker};

#[test]
#[ignore]
fn replay_a_saved_tracker() {
    let var = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("set {k}"));
    let (project, video, name, out) = (PathBuf::from(var("TT_REAL_PROJECT")), PathBuf::from(var("TT_REAL_VIDEO")), var("TT_REAL_TRACKER"), PathBuf::from(var("TT_REAL_OUT")));
    let mut app = AppBuilder::new();
    app.add_module(CoreModules).add_module(TrackModule);
    let mut core = app.build();
    let index = Arc::new(VideoIndex::open(&video).expect("video opens"));
    {
        let w = &mut core.world;
        let mut t = w.resource_mut::<Transport>();
        t.fps = index.fps;
        t.frame_count = index.frame_count();
        w.insert_resource(SourceSize { width: index.width as f64, height: index.height as f64 });
        w.insert_resource(Footage { original: index, proxy: None, decode: DecodeOptions::default() });
    }
    tt_core::persist::load(&mut core.world, &project).expect("project loads");
    let op = {
        let w = &mut core.world;
        let mut q = w.query::<(Entity, &Name)>();
        let found: Vec<Entity> = q.iter(w).filter(|(_, n)| n.as_str() == name).map(|(e, _)| e).collect();
        found.into_iter().find(|e| is_tracker(w, *e)).expect("the tracker")
    };
    // What was saved (the previous run's results) and the guide, for comparison.
    let dump = |w: &World, e: Entity, path: PathBuf| {
        let Some(sig) = w.get::<Output>(e).and_then(|o| w.resource::<SignalStore>().get(o.0)) else { return };
        let Some((lo, hi)) = sig.present_hull() else { return };
        let mut csv = String::from("frame,x,y,score,flags
");
        for f in lo..=hi {
            if let Some(v) = sig.get(f) {
                let (score, flags) = if v.len() > 7 { (v[6], tt_track::flags(v)) } else { (1.0, 0) };
                csv.push_str(&format!("{f},{:.3},{:.3},{score:.4},{flags}
", v[0], v[1]));
            }
        }
        std::fs::write(path, csv).expect("write csv");
    };
    dump(&core.world, op, out.with_extension("saved.csv"));
    if let Some(g) = tt_track::guide_of(&core.world, op) {
        dump(&core.world, g, out.with_extension("guide.csv"));
    }
    if let Ok(prefix) = std::env::var("TT_REAL_DROP") {
        let drop: Vec<Entity> = looks_of(&core.world, op).into_iter().filter(|l| core.world.get::<Name>(*l).is_some_and(|n| n.as_str().starts_with(&prefix))).collect();
        println!("dropping {} looks named {prefix:?}…", drop.len());
        tt_core::commands::delete(&mut core.world, &drop);
    }
    for l in looks_of(&core.world, op) {
        let look = core.world.get::<Look>(l).expect("look");
        println!("look {:?}: frame {} at ({:.1}, {:.1}), {:.0}×{:.0}, {}", core.world.get::<Name>(l).map(|n| n.to_string()), look.frame, look.x, look.y, 2.0 * look.half_w, 2.0 * look.half_h, if look.painted().is_some() { "masked" } else { "unpainted" });
    }
    let start = Instant::now();
    for _ in 0..3 {
        core.run_pre_ui();
    }
    while !settled(&core.world, op) {
        assert!(start.elapsed() < Duration::from_secs(1200), "still tracking");
        core.run_pre_ui();
        std::thread::sleep(Duration::from_millis(5));
    }
    let w = &core.world;
    let sig = w.resource::<SignalStore>().get(w.get::<Output>(op).expect("output").0).expect("signal");
    let (lo, hi) = sig.present_hull().expect("results");
    let mut csv = String::from("frame,x,y,score,flags\n");
    for f in lo..=hi {
        if let Some(v) = sig.get(f) {
            csv.push_str(&format!("{f},{:.3},{:.3},{:.4},{}\n", v[0], v[1], v[6], tt_track::flags(v)));
        }
    }
    std::fs::write(&out, csv).expect("write csv");
    println!("tracked {lo}–{hi} in {:.1} s → {}", start.elapsed().as_secs_f64(), out.display());
}
