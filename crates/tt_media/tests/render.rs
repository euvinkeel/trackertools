//! tt_media::render end to end: a clip made with ffmpeg, rendered moved by
//! a sub-pixel amount (and as a tracking target), decoded again and measured.
//! Skipped without ffmpeg.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize};

use tt_media::VideoIndex;
use tt_media::render::{Codec, IDENTITY, render_marker, render_warped};

const W: usize = 160;
const H: usize = 96;
const FRAMES: usize = 20;

fn run(args: &[&str]) -> bool {
    Command::new("ffmpeg").args(["-hide_banner", "-loglevel", "error", "-y"]).args(args).status().is_ok_and(|s| s.success())
}

/// Black, with a light 10 × 10 square moving 2 px right a frame (an ffmpeg source).
fn square() -> String {
    format!("nullsrc=s={W}x{H}:r=25:d=1,format=gray,geq=lum='if(between(X\\,40+2*N\\,49+2*N)*between(Y\\,40\\,49)\\,235\\,16)'")
}

/// [`square`] as a clip (`name`: each test its own, as they run at once).
fn make_clip(dir: &Path, name: &str) -> Option<PathBuf> {
    let clip = dir.join(format!("{name}.mp4"));
    run(&["-f", "lavfi", "-i", &square(), "-frames:v", &FRAMES.to_string(), "-c:v", "libx264", "-qp", "0", "-pix_fmt", "yuv420p", clip.to_str()?]).then_some(clip)
}

/// Every frame of `path` as gray samples.
fn frames(path: &Path) -> Vec<Vec<u8>> {
    let out = Command::new("ffmpeg").args(["-hide_banner", "-loglevel", "error", "-i"]).arg(path).args(["-f", "rawvideo", "-pix_fmt", "gray", "pipe:1"]).output().expect("ffmpeg runs");
    out.stdout.chunks(W * H).map(|c| c.to_vec()).collect()
}

/// The brightness-weighted centre of what stands out from black.
fn centre(f: &[u8]) -> [f64; 2] {
    let (mut sx, mut sy, mut sw) = (0.0, 0.0, 0.0);
    for (i, v) in f.iter().enumerate() {
        let wgt = (*v as f64 - 40.0).max(0.0);
        sx += wgt * ((i % W) as f64 + 0.5);
        sy += wgt * ((i / W) as f64 + 0.5);
        sw += wgt;
    }
    [sx / sw, sy / sw]
}

#[test]
fn a_render_keeps_every_frame_and_moves_the_picture_sub_pixel() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let Some(clip) = make_clip(&dir, "render_square") else {
        eprintln!("skipped: no ffmpeg");
        return;
    };
    let index = VideoIndex::open(&clip).expect("the clip opens");
    let out = dir.join("render_square_moved.mov");
    let (done, cancel) = (AtomicUsize::new(0), AtomicBool::new(false));
    // Output pixel (x, y) shows source (x − 3.5, y + 2): the picture moves 3.5 right and 2 up.
    render_warped(&index, &out, Codec::ProRes422Hq, 0..index.frame_count(), &|_| [1.0, 0.0, -3.5, 0.0, 1.0, 2.0], &done, &cancel).expect("renders");
    assert_eq!(done.into_inner(), FRAMES);
    let (before, after) = (frames(&clip), frames(&out));
    assert_eq!(after.len(), FRAMES, "one frame out per frame in");
    for (f, (a, b)) in before.iter().zip(&after).enumerate() {
        let (ca, cb) = (centre(a), centre(b));
        let moved = [cb[0] - ca[0], cb[1] - ca[1]];
        assert!((moved[0] - 3.5).abs() < 0.1 && (moved[1] + 2.0).abs() < 0.1, "frame {f}: moved {moved:?}");
    }
    assert!(!dir.join("render_square_moved.mov.part").exists(), "no partial file left");
}

#[test]
fn a_tracking_target_is_a_marker_where_it_is_told() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let Some(clip) = make_clip(&dir, "render_target_source") else {
        eprintln!("skipped: no ffmpeg");
        return;
    };
    let index = VideoIndex::open(&clip).expect("the clip opens");
    let out = dir.join("render_target.mp4");
    let (done, cancel) = (AtomicUsize::new(0), AtomicBool::new(false));
    let at = |g: i64| Some(([50.0 + g as f64, 48.0], 0.0));
    render_marker(&index, &out, Codec::H264, 0..index.frame_count(), &at, 12.0, &done, &cancel).expect("renders");
    let after = frames(&out);
    assert_eq!(after.len(), FRAMES);
    for (g, f) in after.iter().enumerate() {
        // Its top-left quadrant (light) is up and left of the given centre, the top-right (dark) up and right.
        let x = 50 + g;
        assert!(f[42 * W + x - 6] > 200 && f[42 * W + x + 6] < 60, "frame {g}");
    }
}

/// [`square`] with sound: silent for the first 0.2 s (5 frames), then a tone.
fn make_clip_with_sound(dir: &Path, name: &str) -> Option<PathBuf> {
    let clip = dir.join(format!("{name}.mov"));
    let sound = "aevalsrc='if(lt(t\\,0.2)\\,0\\,0.5*sin(2*PI*440*t))':s=48000:d=0.8";
    run(&["-f", "lavfi", "-i", &square(), "-f", "lavfi", "-i", sound, "-frames:v", &FRAMES.to_string(), "-c:v", "libx264", "-qp", "0", "-pix_fmt", "yuv420p", "-c:a", "pcm_s16le", clip.to_str()?])
        .then_some(clip)
}

/// The first audio stream of `path`, mono 48 kHz samples.
fn sound(path: &Path) -> Vec<f32> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-ac", "1", "-ar", "48000", "-f", "f32le", "pipe:1"])
        .output()
        .expect("ffmpeg runs");
    out.stdout.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect()
}

fn loudness(s: &[f32]) -> f32 {
    (s.iter().map(|v| v * v).sum::<f32>() / s.len().max(1) as f32).sqrt()
}

#[test]
fn a_range_renders_just_those_frames_and_their_sound() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let Some(clip) = make_clip_with_sound(&dir, "render_range_source") else {
        eprintln!("skipped: no ffmpeg");
        return;
    };
    let index = VideoIndex::open(&clip).expect("the clip opens");
    let out = dir.join("render_range.mov");
    let (done, cancel) = (AtomicUsize::new(0), AtomicBool::new(false));
    // Frames 5 to 14: the picture as it is; the map is asked about source frames.
    let asked = std::sync::Mutex::new(Vec::new());
    let map = |g: i64| {
        asked.lock().unwrap().push(g);
        IDENTITY
    };
    render_warped(&index, &out, Codec::ProRes422Hq, 5..15, &map, &done, &cancel).expect("renders");
    assert_eq!(done.into_inner(), 10);
    assert_eq!(asked.into_inner().unwrap(), (5..15).collect::<Vec<_>>());
    let (before, after) = (frames(&clip), frames(&out));
    assert_eq!(after.len(), 10, "just the frames asked for");
    for (k, b) in after.iter().enumerate() {
        let (want, got) = (centre(&before[5 + k]), centre(b));
        assert!((want[0] - got[0]).abs() < 0.1 && (want[1] - got[1]).abs() < 0.1, "output frame {k} is source frame {}: {got:?} vs {want:?}", 5 + k);
    }
    // The sound under those frames: 0.4 s, the tone from the start (the silence before frame 5 is left out).
    let s = sound(&out);
    let seconds = s.len() as f64 / 48000.0;
    assert!((seconds - 0.4).abs() < 0.01, "{seconds} s of sound");
    assert!(loudness(&s[..480]) > 0.2, "the tone from the first 10 ms: {}", loudness(&s[..480]));

    // A tracking target over a range: as many frames, the marker where it is on those frames.
    let target = dir.join("render_range_target.mp4");
    let done = AtomicUsize::new(0);
    render_marker(&index, &target, Codec::H264, 12..20, &|g: i64| Some(([50.0 + g as f64, 48.0], 0.0)), 12.0, &done, &cancel).expect("renders");
    let after = frames(&target);
    assert_eq!(after.len(), 8);
    for (k, f) in after.iter().enumerate() {
        let x = 50 + 12 + k;
        assert!(f[42 * W + x - 6] > 200 && f[42 * W + x + 6] < 60, "output frame {k}");
    }
}
