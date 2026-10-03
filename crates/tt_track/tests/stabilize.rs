//! Trackers → a Fusion stabilizer (tt_track::export), end to end: a clip of a
//! still, textured scene filmed by a camera that drifts and rolls is made
//! with ffmpeg, points are tracked by the real tracker, and the exported
//! keys, applied as Fusion's Transform applies them, must hold every point of
//! the scene still: the tracked ones and one that wasn't. Two points at
//! 30 fps, held exactly; four spread out at 50 fps (the rate of the footage
//! this was asked for), through the app's default spring. Skipped without
//! ffmpeg.
//!
//! Each run leaves its clip and `.setting` in the target's tmp folder
//! (`stabilize_540p{fps}.mp4`, `stabilize_{fps}.setting`):
//! `scripts/resolve_stabilizer_proof.py` pastes them into DaVinci Resolve,
//! renders, and measures what Resolve made of them.
//!
//! `export_a_saved_projects_stabilizer` (ignored) does what the app's menu
//! does to a saved project's two trackers, headless, for the same check on
//! real footage:
//!
//! ```text
//! TT_REAL_PROJECT=<copy of a .ttproj> TT_REAL_VIDEO=<video> TT_REAL_TRACKERS="Tracker 2,Tracker 3" \
//! TT_REAL_REFERENCE=<frame> TT_REAL_OUT=<out.setting> [TT_REAL_SMOOTH=<position>,<rotation>] \
//! cargo test -p tt_track --release --test stabilize -- --ignored --nocapture
//! ```
//! With `TT_REAL_FOLLOW=1` it makes the follower (text that moves with them)
//! instead. With `TT_REAL_RENDER=<out>` it also renders the video, as the
//! app's export window does (`TT_REAL_CODEC`: prores, dnxhr, or h264, the default). Any number of trackers or sketches (by name); the smoothing (seconds)
//! defaults to the app's. It writes the `.setting` and, beside it (`.json`),
//! the reference frame, the points on it, the keys' frames and the source's
//! size and rate. Point it at a copy: the project file is only read, never
//! written.

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
use tt_track::export::{Smoothing, StabilizerDefaults, Steady, follow, fusion_follow_setting, fusion_setting, good_points, key_at, stabilize, stabilize_path, stabilizer_map, subject_path};
use tt_track::runner::{Footage, settled};
use tt_track::{TrackModule, add_tracker, is_tracker};

const SECONDS: f64 = 5.0;
const SIZE: [f64; 2] = [960.0, 540.0];
/// The rotated scene's size, before the crop that drifts.
const ROT: [f64; 2] = [1100.0, 760.0];
const SCENE: [f64; 2] = [1400.0, 1000.0];
/// The reference: the frame 4/3 s in, held still.
const REFERENCE_SECONDS: f64 = 4.0 / 3.0;

/// The camera at time t (seconds): roll (radians, clockwise on screen, as
/// ffmpeg's `rotate`) and the crop's corner. Kept in step with `make_clip`'s filters.
fn camera(t: f64) -> (f64, [f64; 2]) {
    (0.1 * (0.8 * t).sin(), [(70.0 + 40.0 * (1.1 * t).sin()).floor(), (110.0 + 30.0 * (0.7 * t).sin()).floor()])
}

/// Where scene point `s` (scene pixels) is at time t (frame pixels, y down).
fn seen(s: [f64; 2], t: f64) -> [f64; 2] {
    let (a, crop) = camera(t);
    let (sin, cos) = a.sin_cos();
    let d = [s[0] - SCENE[0] / 2.0, s[1] - SCENE[1] / 2.0];
    [cos * d[0] - sin * d[1] + ROT[0] / 2.0 - crop[0], sin * d[0] + cos * d[1] + ROT[1] / 2.0 - crop[1]]
}

/// Every frame of `path`, grey, SIZE.
fn grey_frames(path: &Path) -> Vec<Vec<u8>> {
    let out = Command::new(ffmpeg()).args(["-hide_banner", "-loglevel", "error", "-i"]).arg(path).args(["-f", "rawvideo", "-pix_fmt", "gray", "pipe:1"]).output().expect("ffmpeg runs");
    out.stdout.chunks((SIZE[0] * SIZE[1]) as usize).map(|c| c.to_vec()).collect()
}

/// The mean absolute difference of two grey frames over their middle half (no edges).
fn middle_difference(a: &[u8], b: &[u8]) -> f64 {
    let (w, h) = (SIZE[0] as usize, SIZE[1] as usize);
    let (mut sum, mut n) = (0.0, 0.0);
    for y in h / 4..3 * h / 4 {
        for x in w / 4..3 * w / 4 {
            sum += (a[y * w + x] as f64 - b[y * w + x] as f64).abs();
            n += 1.0;
        }
    }
    sum / n
}

fn ffmpeg() -> String {
    std::env::var("FFMPEG").unwrap_or_else(|_| "ffmpeg".into())
}

fn make_clip(dir: &Path, fps: u32) -> Option<PathBuf> {
    let clip = dir.join(format!("stabilize_540p{fps}.mp4"));
    if clip.exists() {
        return Some(clip);
    }
    // One still per rate: the two tests run at once.
    let still = dir.join(format!("stabilize_scene_{fps}.png"));
    let run = |args: &[&str]| Command::new(ffmpeg()).args(["-hide_banner", "-loglevel", "error", "-y"]).args(args).status().is_ok_and(|s| s.success());
    // A still scene: blurred noise (texture everywhere) over a few large shapes.
    let scene = format!(
        "nullsrc=s={}x{}:d=1,geq=lum='128+70*sin(X/90)*cos(Y/70)+50*(random(1)-0.5)':cb=128:cr=128,gblur=sigma=1.5",
        SCENE[0], SCENE[1]
    );
    if !still.exists() && !run(&["-f", "lavfi", "-i", &scene, "-frames:v", "1", still.to_str()?]) {
        eprintln!("skipped: ffmpeg could not make the scene");
        return None;
    }
    let graph = format!(
        "rotate=a='0.1*sin(0.8*t)':ow={}:oh={}:c=black,crop=w={}:h={}:x='floor(70+40*sin(1.1*t))':y='floor(110+30*sin(0.7*t))':exact=1,format=yuv420p",
        ROT[0], ROT[1], SIZE[0], SIZE[1]
    );
    let (rate, frames) = (fps.to_string(), (SECONDS * fps as f64).round().to_string());
    let gop = format!("keyint={fps}:min-keyint={fps}:scenecut=0");
    let ok = run(&[
        "-loop", "1", "-framerate", &rate, "-i", still.to_str()?, "-vf", &graph, "-frames:v", &frames, "-c:v", "libx264", "-crf", "12",
        "-x264-params", &gop, clip.to_str()?,
    ]);
    ok.then_some(clip)
}

/// A guide that follows scene point `s` exactly, in a 64 px box.
fn guide(core: &mut Core, name: &str, s: [f64; 2], fps: f64, frames: i64) -> Entity {
    let w = &mut core.world;
    let sig = w.resource_mut::<SignalStore>().create(BOX_CHANNELS);
    {
        let mut store = w.resource_mut::<SignalStore>();
        let out = store.get_mut(sig).expect("created");
        for f in 0..frames {
            let [x, y] = seen(s, f as f64 / fps);
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

fn tracked_points_hold_the_whole_scene_still(fps: u32, tracked: &[[f64; 2]], smoothing: Smoothing) {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let Some(clip) = make_clip(dir, fps) else {
        eprintln!("skipped: no ffmpeg");
        return;
    };
    let (rate, frames) = (fps as f64, (SECONDS * fps as f64).round() as i64);
    let reference = (REFERENCE_SECONDS * rate).round() as i64;
    let mut app = AppBuilder::new();
    app.add_module(CoreModules).add_module(TrackModule);
    let mut core = app.build();
    // (New trackers wait for a button in the app; these start at once.)
    core.world.resource_mut::<tt_track::NewTrackers>().run = tt_track::TrackRun::Both;
    let index = Arc::new(VideoIndex::open(&clip).expect("clip opens"));
    assert_eq!(index.frame_count(), frames, "the clip has every frame");
    let source = index.clone();
    {
        let w = &mut core.world;
        let mut t = w.resource_mut::<Transport>();
        t.fps = index.fps;
        t.frame_count = index.frame_count();
        w.insert_resource(SourceSize { width: index.width as f64, height: index.height as f64 });
        w.insert_resource(Footage { original: index, proxy: None, decode: DecodeOptions::default() });
    }
    // A point that is never tracked, off the tracked ones' lines.
    let untracked = [640.0, 640.0];
    let (guides, trackers): (Vec<Entity>, Vec<Entity>) = tracked
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let g = guide(&mut core, &format!("Guide {i}"), *s, rate, frames);
            (g, add_tracker(&mut core.world, g, frames / 2, None).expect("a tracker"))
        })
        .unzip();
    let start = Instant::now();
    for _ in 0..3 {
        core.run_pre_ui();
    }
    while !trackers.iter().all(|t| settled(&core.world, *t)) {
        assert!(start.elapsed() < Duration::from_secs(300), "still tracking");
        core.run_pre_ui();
        std::thread::sleep(Duration::from_millis(5));
    }
    let w = &core.world;
    let tracks: Vec<_> = trackers.iter().map(|t| good_points(w, *t)).collect();
    let st = stabilize(&tracks, SIZE, reference, smoothing, rate).expect("a stabilization");
    assert_eq!((st.reference, st.used), (reference, tracked.len()));
    let keys = st.keys;
    assert!(keys.len() as i64 >= frames - 5, "a key on nearly every frame: {} of {frames}", keys.len());
    let setting = fusion_setting("Stabilize", &keys, w.resource::<Transport>().fps.as_f64());
    assert!(setting.contains(&format!("SourceFPS = Input {{ Value = {fps}, }}")), "the source's rate goes with the keys");
    std::fs::write(dir.join(format!("stabilize_{fps}.setting")), &setting).expect("write");
    eprintln!("{fps} fps, {} trackers, {:.0} px spread: rotation jitter ±{:.3}° a frame before smoothing", st.used, st.spread, st.jitter);

    // Every scene point, stabilized, against where it is on the reference frame.
    let mut report = Vec::new();
    let names = (0..tracked.len()).map(|i| format!("tracked {i}")).chain(["untracked".to_string()]);
    for (name, s) in names.zip(tracked.iter().copied().chain([untracked])) {
        let r = seen(s, reference as f64 / rate);
        let mut e: Vec<f64> = keys
            .iter()
            .map(|k| {
                let p = apply(k, seen(s, k.frame as f64 / rate));
                (p[0] - r[0]).hypot(p[1] - r[1])
            })
            .collect();
        e.sort_by(f64::total_cmp);
        let (median, max) = (e[e.len() / 2], e[e.len() - 1]);
        eprintln!("{fps} fps, {name:9}: stabilized drift median {median:.2} px, max {max:.2} px");
        report.push((median, max));
    }
    // Unstabilized, for scale: how far the untracked point moves.
    let at = |f: i64| f as f64 / rate;
    let raw = (0..frames)
        .map(|f| (seen(untracked, at(f))[0] - seen(untracked, at(reference))[0]).hypot(seen(untracked, at(f))[1] - seen(untracked, at(reference))[1]))
        .fold(0.0, f64::max);
    let roll = (0..frames).map(|f| camera(at(f)).0.to_degrees().abs()).fold(0.0, f64::max);
    eprintln!("{fps} fps, unstabilized: the untracked point moves up to {raw:.0} px; the camera rolls up to {roll:.1}°");
    for (median, max) in report {
        assert!(median < 0.5 && max < 1.5, "median {median:.2}, max {max:.2}");
    }

    // A subject of the same trackers (tt_core::subject: their motion, pushed
    // frame by frame), copied from its final data, holds the scene still too.
    let subject = tt_core::subject::make_subject(&mut core.world, &trackers, reference).expect("a subject");
    for _ in 0..3 {
        core.run_pre_ui();
    }
    let w = &core.world;
    let path = subject_path(w, subject);
    assert!(path.len() as i64 >= frames - 5, "the subject has nearly every frame: {}", path.len());
    let held = stabilize_path(&path, SIZE, reference, smoothing, rate).expect("a stabilizer from the subject");
    let r = seen(untracked, reference as f64 / rate);
    let worst = held.keys.iter().map(|k| (apply(k, seen(untracked, k.frame as f64 / rate))[0] - r[0]).hypot(apply(k, seen(untracked, k.frame as f64 / rate))[1] - r[1])).fold(0.0, f64::max);
    eprintln!("{fps} fps, from a subject of the trackers: the untracked point within {worst:.2} px");
    assert!(worst < 1.5, "{worst}");

    // Rendered here (tt_media::render) instead of in an editor: a still scene,
    // stabilized, looks the same on every frame. The middle of each frame
    // against the reference frame's, in grey levels (the unstabilized clip for scale).
    let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("stabilize_{fps}_rendered.mov"));
    let map = |g: i64| key_at(&held.keys, g).map_or(tt_media::render::IDENTITY, |k| stabilizer_map(&k, SIZE, 1.0));
    let (done, stop) = (std::sync::atomic::AtomicUsize::new(0), std::sync::atomic::AtomicBool::new(false));
    tt_media::render::render_warped(&source, &out, tt_media::render::Codec::ProRes422Hq, 0..source.frame_count(), &map, &done, &stop).expect("renders");
    let (raw, rendered) = (grey_frames(&clip), grey_frames(&out));
    assert_eq!(rendered.len() as i64, frames, "one frame out per frame in");
    let r = reference as usize;
    let worst = |v: &[Vec<u8>]| (0..v.len()).map(|f| middle_difference(&v[f], &v[r])).fold(0.0, f64::max);
    let (still, moving) = (worst(&rendered), worst(&raw));
    eprintln!("{fps} fps, rendered here: every frame's middle within {still:.2} grey levels of the reference frame's (unstabilized: {moving:.1})");
    assert!(still < 4.0 && still < moving / 5.0, "{still} vs {moving}");

    // A sketch's point counts as a tracker's: the guides (box producers, as a
    // sketch is, here drawn perfectly) hold the whole scene still by themselves,
    // and one alone holds its own point still.
    let drift = |keys: &[Steady], s: [f64; 2]| {
        let r = seen(s, reference as f64 / rate);
        keys.iter().map(|k| (apply(k, seen(s, k.frame as f64 / rate))[0] - r[0]).hypot(apply(k, seen(s, k.frame as f64 / rate))[1] - r[1])).fold(0.0, f64::max)
    };
    let paths: Vec<_> = guides.iter().map(|g| good_points(w, *g)).collect();
    let all = stabilize(&paths, SIZE, reference, Smoothing::default(), rate).expect("from the guides");
    let one = stabilize(&paths[..1], SIZE, reference, Smoothing::default(), rate).expect("from one guide");
    let (whole, own) = (drift(&all.keys, untracked), drift(&one.keys, tracked[0]));
    eprintln!("{fps} fps, from the guides alone: the untracked point within {whole:.3} px; one guide holds its own point within {own:.3} px");
    assert!(whole < 0.01 && own < 0.01 && one.keys.iter().all(|k| k.angle == 0.0));
}

#[test]
fn two_tracked_points_hold_the_whole_scene_still_30fps() {
    // ~380 px apart, held exactly.
    tracked_points_hold_the_whole_scene_still(30, &[[520.0, 430.0], [880.0, 560.0]], Smoothing::default());
}

#[test]
fn four_tracked_points_hold_the_whole_scene_still_50fps() {
    // Spread over the frame, through the app's default spring.
    let spread = [[520.0, 430.0], [880.0, 560.0], [430.0, 600.0], [960.0, 380.0]];
    tracked_points_hold_the_whole_scene_still(50, &spread, StabilizerDefaults::default().into());
}

#[test]
#[ignore]
fn export_a_saved_projects_stabilizer() {
    let var = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("set {k}"));
    let (project, video, out) = (PathBuf::from(var("TT_REAL_PROJECT")), PathBuf::from(var("TT_REAL_VIDEO")), PathBuf::from(var("TT_REAL_OUT")));
    let names: Vec<String> = var("TT_REAL_TRACKERS").split(',').map(|n| n.trim().to_string()).collect();
    let reference: i64 = var("TT_REAL_REFERENCE").parse().expect("TT_REAL_REFERENCE: a frame number");
    let mut app = AppBuilder::new();
    app.add_module(CoreModules).add_module(TrackModule);
    let mut core = app.build();
    // (New trackers wait for a button in the app; these start at once.)
    core.world.resource_mut::<tt_track::NewTrackers>().run = tt_track::TrackRun::Both;
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
    let w = &mut core.world;
    let mut q = w.query::<(Entity, &Name)>();
    let named: Vec<(Entity, String)> = q.iter(w).map(|(e, n)| (e, n.to_string())).collect();
    // Trackers or sketches, by name (a sketch's point is the hand's path).
    let tracker = |name: &str| {
        named.iter().find(|(e, n)| n == name && (is_tracker(w, *e) || tt_core::sketch::is_sketch(w, *e))).map(|(e, _)| *e).unwrap_or_else(|| panic!("no tracker or sketch {name:?}"))
    };
    let trackers: Vec<Entity> = names.iter().map(|n| tracker(n)).collect();
    let smoothing: Smoothing = match std::env::var("TT_REAL_SMOOTH") {
        Ok(v) => {
            let s: Vec<f64> = v.split(',').map(|x| x.trim().parse().expect("TT_REAL_SMOOTH: <position>,<rotation> seconds")).collect();
            Smoothing { position: s[0], rotation: s[1] }
        }
        Err(_) => StabilizerDefaults::default().into(),
    };
    // As the menu does (tt_app panels/menu.rs, copy_stabilizer), with the playhead on `reference`.
    let size = w.resource::<SourceSize>();
    let size = [size.width, size.height];
    let fps = w.resource::<Transport>().fps.as_f64();
    let tracks: Vec<_> = trackers.iter().map(|t| good_points(w, *t)).collect();
    let follower = std::env::var_os("TT_REAL_FOLLOW").is_some();
    let made = if follower { follow(&tracks, size, reference, smoothing, fps) } else { stabilize(&tracks, size, reference, smoothing, fps) };
    let st = made.expect("a frame where two trackers are good");
    let keys = &st.keys;
    let (first, last, held) = (keys[0].frame, keys[keys.len() - 1].frame, st.reference);
    let text = if follower { fusion_follow_setting("Follow", keys, fps) } else { fusion_setting("Stabilize", keys, fps) };
    std::fs::write(&out, text).expect("write the setting");
    let points: Vec<String> = tracks.iter().filter_map(|p| p.iter().find(|(f, _)| *f == held)).map(|(_, [x, y])| format!("[{x:.3}, {y:.3}]")).collect();
    let info = format!(
        "{{\n  \"reference\": {held},\n  \"points\": [{}],\n  \"first\": {first},\n  \"last\": {last},\n  \"keys\": {},\n  \"fps\": {fps},\n  \"size\": [{}, {}],\n  \"smooth\": [{}, {}],\n  \"spread\": {:.1},\n  \"jitter\": {:.4}\n}}\n",
        points.join(", "),
        keys.len(),
        size[0],
        size[1],
        smoothing.position,
        smoothing.rotation,
        st.spread,
        st.jitter
    );
    std::fs::write(out.with_extension("json"), &info).expect("write the info");
    if let Ok(render) = std::env::var("TT_REAL_RENDER") {
        use tt_media::render::{Codec, IDENTITY, render_marker, render_warped};
        let codec = match std::env::var("TT_REAL_CODEC").as_deref() {
            Ok("prores") => Codec::ProRes422Hq,
            Ok("dnxhr") => Codec::DnxhrHqx,
            _ => Codec::H264,
        };
        let footage = w.resource::<Footage>().original.clone();
        // TT_REAL_RANGE=in-out (frames, both included): just those.
        let frames = match std::env::var("TT_REAL_RANGE").ok().and_then(|r| r.split_once('-').map(|(a, b)| (a.trim().parse::<i64>(), b.trim().parse::<i64>()))) {
            Some((Ok(a), Ok(b))) => a..b + 1,
            _ => 0..footage.frame_count(),
        };
        let (done, stop) = (std::sync::atomic::AtomicUsize::new(0), std::sync::atomic::AtomicBool::new(false));
        let start = Instant::now();
        let made = if follower {
            let half = (size[0].min(size[1]) / 24.0).clamp(16.0, 120.0);
            render_marker(&footage, Path::new(&render), codec, frames.clone(), &|g| tt_track::export::follower_at(keys, size, g), half, &done, &stop)
        } else {
            render_warped(&footage, Path::new(&render), codec, frames.clone(), &|g| tt_track::export::key_at(keys, g).map_or(IDENTITY, |k| tt_track::export::stabilizer_map(&k, size, 1.0)), &done, &stop)
        };
        made.expect("renders");
        let secs = start.elapsed().as_secs_f64();
        let n = done.into_inner();
        println!("rendered {n} frames ({}–{}) in {secs:.1} s ({:.1} fps) → {render}", frames.start, frames.end - 1, n as f64 / secs);
    }
    println!("{} keys on frames {first}–{last}, held as on frame {held} → {}\n{info}", keys.len(), out.display());
}
