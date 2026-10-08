//! tt_media::layers end to end: a grey clip made with ffmpeg, a red square
//! layer moving across it, rendered over the video and alone on
//! transparency (ProRes 4444, a PNG sequence, WebM), decoded again and
//! measured. Skipped without ffmpeg.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};

use tt_core::layer::Placed;
use tt_core::time::FrameIndex;
use tt_media::VideoIndex;
use tt_media::layers::{AlphaFormat, Overlay, render_alpha, render_over};
use tt_media::overlay::Frames;
use tt_media::render::Codec;

const W: usize = 160;
const H: usize = 96;
const FRAMES: usize = 12;

fn run(args: &[&str]) -> bool {
    Command::new("ffmpeg").args(["-hide_banner", "-loglevel", "error", "-y"]).args(args).status().is_ok_and(|s| s.success())
}

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tt-layers-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A mid-grey clip.
fn grey(d: &Path) -> Option<PathBuf> {
    let clip = d.join("grey.mp4");
    run(&["-f", "lavfi", "-i", &format!("color=c=0x808080:s={W}x{H}:r=25:d=1"), "-frames:v", &FRAMES.to_string(), "-c:v", "libx264", "-qp", "0", "-pix_fmt", "yuv420p", clip.to_str()?]).then_some(clip)
}

/// Every frame of `path` as RGBA.
fn rgba(path: &Path) -> Vec<Vec<u8>> {
    let out = Command::new("ffmpeg").args(["-hide_banner", "-loglevel", "error", "-i"]).arg(path).args(["-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"]).output().expect("ffmpeg runs");
    out.stdout.chunks(W * H * 4).map(|c| c.to_vec()).collect()
}

fn px(f: &[u8], x: usize, y: usize) -> [u8; 4] {
    let i = (y * W + x) * 4;
    [f[i], f[i + 1], f[i + 2], f[i + 3]]
}

/// A 10 × 10 opaque red picture, drawn twice its size, its centre 4 px further right each frame from (40, 48).
fn red() -> (Arc<Frames>, impl Fn(FrameIndex) -> Option<Placed> + Sync) {
    let frames = Arc::new(Frames { width: 10, height: 10, fps: 0.0, frames: vec![[255u8, 0, 0, 255].repeat(100)] });
    let at = |g: FrameIndex| Some(Placed { at: [40.0 + 4.0 * g as f64, 48.0], angle: 0.0, scale: [2.0, 2.0], opacity: 1.0, clip_time: 0.0, anchor: [0.5, 0.5] });
    (frames, at)
}

#[test]
fn a_layer_rendered_over_the_video_and_alone() {
    let d = dir("all");
    let Some(clip) = grey(&d) else {
        eprintln!("no ffmpeg: skipped");
        return;
    };
    let index = VideoIndex::open(&clip).expect("indexes the clip");
    let (frames, at) = red();
    let overlays = [Overlay { frames, size: [10.0, 10.0], placed: &at, blend: tt_core::layer::BlendMode::Normal }];
    let (progress, cancel) = (AtomicUsize::new(0), AtomicBool::new(false));

    // Over the video (H.264: what's measured is the picture, near lossless).
    let over = d.join("over.mp4");
    render_over(&index, &over, Codec::H264, 0..FRAMES as FrameIndex, &overlays, &progress, &cancel).expect("renders over the video");
    let got = rgba(&over);
    assert_eq!(got.len(), FRAMES);
    for (g, f) in got.iter().enumerate() {
        let x = 40 + 4 * g;
        let inside = px(f, x, 48);
        assert!(inside[0] > 220 && inside[1] < 40 && inside[2] < 40, "frame {g}: red at its centre: {inside:?}");
        let outside = px(f, x + 20, 48);
        assert!(outside.iter().take(3).all(|c| c.abs_diff(128) <= 4), "frame {g}: the grey is untouched beside it: {outside:?}");
    }

    // Alone, on transparency, in each format.
    for format in AlphaFormat::ALL {
        let out = d.join(match format {
            AlphaFormat::ProRes4444 => "alpha.mov",
            AlphaFormat::PngSequence => "pngs",
            AlphaFormat::WebM => "alpha.webm",
        });
        let made = render_alpha(&index, &out, format, 0..FRAMES as FrameIndex, &overlays, &progress, &cancel);
        if format == AlphaFormat::WebM && made.as_ref().is_err_and(|e| e.to_string().contains("libvpx")) {
            eprintln!("this ffmpeg has no VP9 encoder: WebM skipped");
            continue;
        }
        made.unwrap_or_else(|e| panic!("{format:?}: {e:#}"));
        let pictures: Vec<Vec<u8>> = if format == AlphaFormat::PngSequence {
            let mut files: Vec<PathBuf> = std::fs::read_dir(&out).unwrap().map(|e| e.unwrap().path()).collect();
            files.sort();
            assert_eq!(files.len(), FRAMES, "a picture a frame");
            assert!(files[0].file_name().unwrap().to_string_lossy().ends_with("_000000.png"), "numbered by frame: {:?}", files[0]);
            files.iter().flat_map(|f| rgba(f)).collect()
        } else {
            // (WebM's alpha is in a side stream: ffmpeg's libvpx decoder reads it.)
            let args = if format == AlphaFormat::WebM { vec!["-c:v", "libvpx-vp9"] } else { vec![] };
            let out_rgba = Command::new("ffmpeg").args(["-hide_banner", "-loglevel", "error"]).args(&args).arg("-i").arg(&out).args(["-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"]).output().unwrap();
            out_rgba.stdout.chunks(W * H * 4).map(|c| c.to_vec()).collect()
        };
        assert_eq!(pictures.len(), FRAMES, "{format:?}");
        for (g, f) in pictures.iter().enumerate() {
            let x = 40 + 4 * g;
            let inside = px(f, x, 48);
            assert!(inside[3] > 240 && inside[0] > 200 && inside[1] < 60, "{format:?} frame {g}: opaque red: {inside:?}");
            assert!(px(f, x + 20, 48)[3] < 16, "{format:?} frame {g}: transparent beside it");
            assert!(px(f, 2, 2)[3] < 16, "{format:?} frame {g}: transparent in the corner");
        }
    }
    let _ = std::fs::remove_dir_all(&d);
}

/// A stabilized render with the layer on it (the export window's): the
/// layer is drawn on the source frame, then moved with the picture. The map
/// follows the square, so it stays at the same place in the output on every frame.
#[test]
fn a_layer_moves_with_the_stabilized_picture() {
    let d = dir("warped");
    let Some(clip) = grey(&d) else {
        eprintln!("no ffmpeg: skipped");
        return;
    };
    let index = VideoIndex::open(&clip).expect("indexes the clip");
    let (frames, at) = red();
    let overlays = [Overlay { frames, size: [10.0, 10.0], placed: &at, blend: tt_core::layer::BlendMode::Normal }];
    let (progress, cancel) = (AtomicUsize::new(0), AtomicBool::new(false));
    // Output x 80 is source x 40 + 4g: where the square is on frame g.
    let map = |g: FrameIndex| [1.0, 0.0, 4.0 * g as f64 - 40.0, 0.0, 1.0, 0.0];
    let out = d.join("stabilized.mp4");
    tt_media::render::render_warped_with(&index, &out, Codec::H264, 0..FRAMES as FrameIndex, &map, &overlays, &progress, &cancel).expect("renders");
    let got = rgba(&out);
    assert_eq!(got.len(), FRAMES);
    for (g, f) in got.iter().enumerate() {
        let inside = px(f, 80, 48);
        assert!(inside[0] > 220 && inside[1] < 40 && inside[2] < 40, "frame {g}: the square holds still at (80, 48): {inside:?}");
        let beside = px(f, 110, 48);
        assert!(beside.iter().take(3).all(|c| c.abs_diff(128) <= 4), "frame {g}: the grey beside it: {beside:?}");
        // (Output x 4 is source x 4g - 36: outside the picture until frame 9.)
        if g < 9 {
            let left = px(f, 4, 48);
            assert!(left.iter().take(3).all(|c| *c < 24), "frame {g}: black where the picture moved away: {left:?}");
        }
    }
    let _ = std::fs::remove_dir_all(&d);
}
