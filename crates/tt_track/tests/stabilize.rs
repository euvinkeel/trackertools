//! Two trackers → a Fusion stabilizer (tt_track::export), end to end: a clip
//! of a still, textured scene filmed by a camera that drifts and rolls is
//! made with ffmpeg, two points are tracked by the real tracker, and the
//! exported keys, applied as Fusion's Transform applies them, must hold
//! every point of the scene still: the two tracked and a third one that
//! wasn't. Skipped without ffmpeg.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bevy_ecs::name::Name;
use bevy_ecs::prelude::*;
use tt_core::op::Output;
use tt_core::signal::SignalStore;
use tt_core::sketch::BOX_CHANNELS;
use tt_core::transport::Transport;
use tt_core::view::SourceSize;
use tt_core::{AppBuilder, Core, CoreModules};
use tt_media::{DecodeOptions, VideoIndex};
use tt_track::export::{Steady, fusion_setting, good_points, steady};
use tt_track::runner::{Footage, settled};
use tt_track::{TrackModule, add_tracker};

const FPS: f64 = 30.0;
const FRAMES: i64 = 150;
const SIZE: [f64; 2] = [960.0, 540.0];
/// The rotated scene's size, before the crop that drifts.
const ROT: [f64; 2] = [1100.0, 760.0];
const SCENE: [f64; 2] = [1400.0, 1000.0];

/// The camera at frame f: roll (radians, clockwise on screen, as ffmpeg's
/// `rotate`) and the crop's corner. Kept in step with `make_clip`'s filters.
fn camera(f: i64) -> (f64, [f64; 2]) {
    let t = f as f64 / FPS;
    (0.1 * (0.8 * t).sin(), [(70.0 + 40.0 * (1.1 * t).sin()).floor(), (110.0 + 30.0 * (0.7 * t).sin()).floor()])
}

/// Where scene point `s` (scene pixels) is on frame f (frame pixels, y down).
fn seen(s: [f64; 2], f: i64) -> [f64; 2] {
    let (a, crop) = camera(f);
    let (sin, cos) = a.sin_cos();
    let d = [s[0] - SCENE[0] / 2.0, s[1] - SCENE[1] / 2.0];
    [cos * d[0] - sin * d[1] + ROT[0] / 2.0 - crop[0], sin * d[0] + cos * d[1] + ROT[1] / 2.0 - crop[1]]
}

fn ffmpeg() -> String {
    std::env::var("FFMPEG").unwrap_or_else(|_| "ffmpeg".into())
}

fn make_clip(dir: &Path) -> Option<PathBuf> {
    let clip = dir.join("stabilize_540p30.mp4");
    if clip.exists() {
        return Some(clip);
    }
    let still = dir.join("stabilize_scene.png");
    let run = |args: &[&str]| Command::new(ffmpeg()).args(["-hide_banner", "-loglevel", "error", "-y"]).args(args).status().is_ok_and(|s| s.success());
    // A still scene: blurred noise (texture everywhere) over a few large shapes.
    let scene = format!(
        "nullsrc=s={}x{}:d=1,geq=lum='128+70*sin(X/90)*cos(Y/70)+50*(random(1)-0.5)':cb=128:cr=128,gblur=sigma=1.5",
        SCENE[0], SCENE[1]
    );
    if !run(&["-f", "lavfi", "-i", &scene, "-frames:v", "1", still.to_str()?]) {
        eprintln!("skipped: ffmpeg could not make the scene");
        return None;
    }
    let graph = format!(
        "rotate=a='0.1*sin(0.8*t)':ow={}:oh={}:c=black,crop=w={}:h={}:x='floor(70+40*sin(1.1*t))':y='floor(110+30*sin(0.7*t))':exact=1,format=yuv420p",
        ROT[0], ROT[1], SIZE[0], SIZE[1]
    );
    let frames = FRAMES.to_string();
    let ok = run(&[
        "-loop", "1", "-framerate", "30", "-i", still.to_str()?, "-vf", &graph, "-frames:v", &frames, "-c:v", "libx264", "-crf", "12",
        "-x264-params", "keyint=30:min-keyint=30:scenecut=0", clip.to_str()?,
    ]);
    ok.then_some(clip)
}

/// A guide that follows scene point `s` exactly, in a 64 px box.
fn guide(core: &mut Core, name: &str, s: [f64; 2]) -> Entity {
    let w = &mut core.world;
    let sig = w.resource_mut::<SignalStore>().create(BOX_CHANNELS);
    {
        let mut store = w.resource_mut::<SignalStore>();
        let out = store.get_mut(sig).expect("created");
        for f in 0..FRAMES {
            let [x, y] = seen(s, f);
            out.set(f, &[x, y, x - 32.0, y - 32.0, x + 32.0, y + 32.0].map(|v| v as f32));
        }
    }
    w.spawn((Name::new(name.to_string()), Output(sig))).id()
}

/// Fusion's Transform with its pivot at the centre, on a point in frame
/// pixels (y down): turn about the centre by the key's angle
/// (counter-clockwise on screen), then put the centre at the key's Center.
fn apply(k: &Steady, p: [f64; 2]) -> [f64; 2] {
    let [w, h] = SIZE;
    let up = [p[0], h - p[1]];
    let (s, c) = k.angle.to_radians().sin_cos();
    let d = [up[0] - w / 2.0, up[1] - h / 2.0];
    let out = [c * d[0] - s * d[1] + k.center[0] * w, s * d[0] + c * d[1] + k.center[1] * h];
    [out[0], h - out[1]]
}

#[test]
fn two_tracked_points_hold_the_whole_scene_still() {
    let Some(clip) = make_clip(Path::new(env!("CARGO_TARGET_TMPDIR"))) else {
        eprintln!("skipped: no ffmpeg");
        return;
    };
    let mut app = AppBuilder::new();
    app.add_module(CoreModules).add_module(TrackModule);
    let mut core = app.build();
    let index = Arc::new(VideoIndex::open(&clip).expect("clip opens"));
    {
        let w = &mut core.world;
        let mut t = w.resource_mut::<Transport>();
        t.fps = index.fps;
        t.frame_count = index.frame_count();
        w.insert_resource(SourceSize { width: index.width as f64, height: index.height as f64 });
        w.insert_resource(Footage { original: index, proxy: None, decode: DecodeOptions::default() });
    }
    // Two points ~380 px apart, and a third (never tracked) off their line.
    let (sa, sb, sc) = ([520.0, 430.0], [880.0, 560.0], [640.0, 640.0]);
    let (ga, gb) = (guide(&mut core, "Guide A", sa), guide(&mut core, "Guide B", sb));
    let ta = add_tracker(&mut core.world, ga, 75, None).expect("tracker A");
    let tb = add_tracker(&mut core.world, gb, 75, None).expect("tracker B");
    let start = Instant::now();
    for _ in 0..3 {
        core.run_pre_ui();
    }
    while !(settled(&core.world, ta) && settled(&core.world, tb)) {
        assert!(start.elapsed() < Duration::from_secs(300), "still tracking");
        core.run_pre_ui();
        std::thread::sleep(Duration::from_millis(5));
    }
    let w = &core.world;
    let (pa, pb) = (good_points(w, ta), good_points(w, tb));
    let keys = steady(&pa, &pb, SIZE, 40);
    assert!(keys.len() as i64 >= FRAMES - 5, "a key on nearly every frame: {} of {FRAMES}", keys.len());
    let setting = fusion_setting("Stabilize", &keys, 0);
    std::fs::write(Path::new(env!("CARGO_TARGET_TMPDIR")).join("stabilize.setting"), &setting).expect("write");

    // Every scene point, stabilized, against where it is on the reference frame.
    let mut report = Vec::new();
    for (name, s) in [("tracked A", sa), ("tracked B", sb), ("untracked", sc)] {
        let r = seen(s, 40);
        let mut e: Vec<f64> = keys.iter().map(|k| {
            let p = apply(k, seen(s, k.frame));
            (p[0] - r[0]).hypot(p[1] - r[1])
        }).collect();
        e.sort_by(f64::total_cmp);
        let (median, max) = (e[e.len() / 2], e[e.len() - 1]);
        eprintln!("{name:9}: stabilized drift median {median:.2} px, max {max:.2} px");
        report.push((median, max));
    }
    // Unstabilized, for scale: how far the untracked point moves.
    let raw = (0..FRAMES).map(|f| { let (p, r) = (seen(sc, f), seen(sc, 40)); (p[0] - r[0]).hypot(p[1] - r[1]) }).fold(0.0, f64::max);
    let roll = (0..FRAMES).map(|f| camera(f).0.to_degrees().abs()).fold(0.0, f64::max);
    eprintln!("unstabilized: the untracked point moves up to {raw:.0} px; the camera rolls up to {roll:.1}°");
    for (median, max) in report {
        assert!(median < 0.5 && max < 1.5, "median {median:.2}, max {max:.2}");
    }
}
