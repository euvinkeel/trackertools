//! Media for layers (tt_core::layer): a picture (PNG, JPEG, WebP, BMP…), an
//! animated GIF or a video clip, read through ffmpeg as RGBA with its
//! transparency (PNG, GIF, WebM's alpha, ProRes 4444).
//!
//! - [`probe`]: its size, length and frame rate (ffprobe).
//! - [`decode`]: every frame, evenly timed at its frame rate, scaled down to
//!   fit a pixel budget (the preview's is small; an export's is the media's
//!   own size, up to a memory limit), straight (not premultiplied) RGBA.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

/// What a media file is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MediaInfo {
    pub width: u32,
    pub height: u32,
    /// Seconds (0: a still picture).
    pub duration: f64,
    /// Frames per second (0: a still picture).
    pub fps: f64,
    /// The decoder to read it with, when ffmpeg's own wouldn't keep its
    /// transparency (VP8/VP9 in WebM: only libvpx reads the alpha).
    pub decoder: Option<&'static str>,
}

impl MediaInfo {
    pub fn still(&self) -> bool {
        self.duration <= 0.0 || self.fps <= 0.0
    }
}

/// A media file's frames, decoded.
#[derive(Clone, Debug)]
pub struct Frames {
    /// Each frame's size (all the same).
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// Straight RGBA, row by row.
    pub frames: Vec<Vec<u8>>,
}

impl Frames {
    /// The frame showing at `t` seconds into the clip.
    pub fn at(&self, t: f64) -> Option<&[u8]> {
        if self.frames.is_empty() {
            return None;
        }
        let i = if self.fps > 0.0 { (t * self.fps + 1e-6).floor().max(0.0) as usize } else { 0 };
        self.frames.get(i.min(self.frames.len() - 1)).map(Vec::as_slice)
    }

    pub fn index_at(&self, t: f64) -> usize {
        if self.fps > 0.0 { ((t * self.fps + 1e-6).floor().max(0.0) as usize).min(self.frames.len().saturating_sub(1)) } else { 0 }
    }
}

fn quiet(mut cmd: Command) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd.stdin(Stdio::null());
    cmd
}

/// A ratio like `30000/1001`, or a number.
fn rate(s: &str) -> Option<f64> {
    match s.split_once('/') {
        Some((a, b)) => {
            let (a, b): (f64, f64) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
            (b > 0.0).then_some(a / b)
        }
        None => s.trim().parse().ok(),
    }
}

/// Containers that hold one picture (ffmpeg's image readers): what's read
/// through them is a still. (Not by codec: PNG and Motion-JPEG are clips
/// in a MOV or AVI.)
fn is_picture_format(format: &str) -> bool {
    format.split(',').any(|f| f == "image2" || f.ends_with("_pipe"))
}

/// What `path` is (ffprobe).
pub fn probe(path: &Path) -> Result<MediaInfo> {
    let mut cmd = quiet(Command::new(crate::ffmpeg::tool("ffprobe", "FFPROBE")));
    cmd.args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=codec_name,width,height,avg_frame_rate,r_frame_rate,nb_frames,duration:format=duration,format_name", "-of", "default=nw=1"]).arg(path);
    let out = cmd.output().context("ffprobe didn't start")?;
    if !out.status.success() {
        bail!("ffprobe can't read {}: {}", path.display(), String::from_utf8_lossy(&out.stderr).trim());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let get = |k: &str| text.lines().filter_map(|l| l.split_once('=')).filter(|(key, _)| *key == k).map(|(_, v)| v.trim().to_string()).find(|v| v != "N/A" && !v.is_empty());
    let width: u32 = get("width").and_then(|v| v.parse().ok()).context("no picture in it")?;
    let height: u32 = get("height").and_then(|v| v.parse().ok()).context("no picture in it")?;
    let codec = get("codec_name").unwrap_or_default();
    let fps = get("avg_frame_rate").and_then(|v| rate(&v)).filter(|r| *r > 0.0).or_else(|| get("r_frame_rate").and_then(|v| rate(&v))).unwrap_or(0.0);
    let duration = get("duration").and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    let frames: Option<u64> = get("nb_frames").and_then(|v| v.parse().ok());
    let format = get("format_name").unwrap_or_default();
    let still = is_picture_format(&format) || frames == Some(1) || duration <= 0.0 || fps <= 0.0;
    let decoder = match codec.as_str() {
        "vp9" => Some("libvpx-vp9"),
        "vp8" => Some("libvpx"),
        _ => None,
    };
    Ok(if still { MediaInfo { width, height, duration: 0.0, fps: 0.0, decoder } } else { MediaInfo { width, height, duration, fps, decoder } })
}

/// The size `info` decodes at: scaled down (never up) so that one frame has
/// at most `max_side` on its longer side, and all of them together at most
/// `max_bytes` (RGBA). Even sizes are not needed (RGBA).
pub fn fit(info: &MediaInfo, max_side: u32, max_bytes: usize) -> (u32, u32) {
    let (w, h) = (info.width.max(1) as f64, info.height.max(1) as f64);
    let n = if info.still() { 1.0 } else { (info.duration * info.fps).ceil().max(1.0) };
    let by_side = (max_side as f64 / w.max(h)).min(1.0);
    let by_bytes = (max_bytes as f64 / (n * w * h * 4.0)).sqrt().min(1.0);
    let s = by_side.min(by_bytes);
    (((w * s).round() as u32).max(1), ((h * s).round() as u32).max(1))
}

/// Every frame of `path`, evenly timed at its frame rate, at `size` (see [`fit`]).
pub fn decode(path: &Path, info: &MediaInfo, size: (u32, u32)) -> Result<Frames> {
    let (w, h) = size;
    let mut cmd = quiet(Command::new(crate::ffmpeg::tool("ffmpeg", "FFMPEG")));
    cmd.args(["-v", "error", "-nostdin"]);
    if let Some(d) = info.decoder {
        cmd.args(["-c:v", d]);
    }
    cmd.arg("-i").arg(path);
    let filter = format!("scale={w}:{h}:flags=area,format=rgba");
    cmd.args(["-an", "-vf", &filter]);
    if info.still() {
        cmd.args(["-frames:v", "1"]);
    } else {
        // Evenly timed frames (a GIF's own delays vary), as many as its length holds.
        cmd.args(["-fps_mode", "cfr", "-r", &format!("{}", info.fps)]);
    }
    cmd.args(["-f", "rawvideo", "-pix_fmt", "rgba", "-"]);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().context("ffmpeg didn't start")?;
    let mut stdout = child.stdout.take().expect("piped");
    let mut err = child.stderr.take().expect("piped");
    let errors = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    let bytes = w as usize * h as usize * 4;
    let mut frames = Vec::new();
    loop {
        let mut buf = vec![0u8; bytes];
        match stdout.read_exact(&mut buf) {
            Ok(()) => frames.push(buf),
            Err(_) => break,
        }
    }
    let status = child.wait()?;
    let errors = errors.join().unwrap_or_default();
    if frames.is_empty() {
        bail!("ffmpeg read no frames from {} ({status}): {}", path.display(), errors.trim());
    }
    Ok(Frames { width: w, height: h, fps: if info.still() { 0.0 } else { info.fps }, frames })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_fit_the_side_and_the_memory() {
        let pic = MediaInfo { width: 2000, height: 1000, duration: 0.0, fps: 0.0, decoder: None };
        assert_eq!(fit(&pic, 500, usize::MAX), (500, 250));
        assert_eq!(fit(&pic, 4000, usize::MAX), (2000, 1000), "never up");
        let clip = MediaInfo { width: 1000, height: 1000, duration: 10.0, fps: 10.0, decoder: None };
        let (w, h) = fit(&clip, 4000, 100 * 100 * 4 * 100);
        assert_eq!((w, h), (100, 100), "100 frames in the memory of 100 frames of 100 × 100");
    }

    /// A GIF and a PNG made by ffmpeg itself, probed and decoded (skipped without ffmpeg).
    #[test]
    fn a_gif_and_a_picture_are_read() {
        let ffmpeg = crate::ffmpeg::tool("ffmpeg", "FFMPEG");
        let dir = std::env::temp_dir().join(format!("tt-overlay-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let gif = dir.join("a.gif");
        let png = dir.join("a.png");
        let made = Command::new(&ffmpeg)
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=64x32:rate=10:duration=1"])
            .arg(&gif)
            .status()
            .is_ok_and(|s| s.success())
            && Command::new(&ffmpeg).args(["-v", "error", "-y", "-f", "lavfi", "-i", "color=c=red@0.5:size=40x20,format=rgba", "-frames:v", "1"]).arg(&png).status().is_ok_and(|s| s.success());
        if !made {
            eprintln!("no ffmpeg: skipped");
            return;
        }
        let info = probe(&gif).expect("probes the GIF");
        assert_eq!((info.width, info.height), (64, 32));
        assert!(!info.still() && (info.fps - 10.0).abs() < 0.5, "{info:?}");
        let frames = decode(&gif, &info, fit(&info, 32, usize::MAX)).expect("decodes the GIF");
        assert_eq!((frames.width, frames.height), (32, 16));
        assert!((9..=11).contains(&frames.frames.len()), "{} frames", frames.frames.len());
        let pinfo = probe(&png).expect("probes the PNG");
        assert!(pinfo.still());
        let p = decode(&png, &pinfo, (40, 20)).expect("decodes the PNG");
        assert_eq!(p.frames.len(), 1);
        let px = &p.frames[0][..4];
        assert!(px[0] > 200 && (100..160).contains(&px[3]), "red, half transparent: {px:?}");
        // A video clip.
        let mp4 = dir.join("a.mp4");
        if Command::new(&ffmpeg).args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=96x64:rate=25:duration=0.8", "-pix_fmt", "yuv420p"]).arg(&mp4).status().is_ok_and(|s| s.success()) {
            let info = probe(&mp4).expect("probes the clip");
            assert!(!info.still() && (info.fps - 25.0).abs() < 0.1 && (info.duration - 0.8).abs() < 0.05, "{info:?}");
            let f = decode(&mp4, &info, fit(&info, 48, usize::MAX)).expect("decodes the clip");
            assert_eq!((f.width, f.height, f.frames.len()), (48, 32, 20));
            assert_eq!(f.index_at(0.39), 9);
        }
        // PNG in a MOV (an overlay with alpha): a clip, not a picture.
        let mov = dir.join("a.mov");
        if Command::new(&ffmpeg).args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=32x32:rate=10:duration=0.5", "-c:v", "png"]).arg(&mov).status().is_ok_and(|s| s.success()) {
            let info = probe(&mov).expect("probes PNG in MOV");
            assert!(!info.still(), "PNG frames in a MOV are a clip: {info:?}");
        }
        // WebM with alpha: read through libvpx, its transparency kept.
        let webm = dir.join("a.webm");
        if Command::new(&ffmpeg).args(["-v", "error", "-y", "-f", "lavfi", "-i", "color=c=red@0.5:size=32x32:rate=10:duration=0.5,format=yuva420p", "-c:v", "libvpx-vp9", "-pix_fmt", "yuva420p", "-auto-alt-ref", "0"]).arg(&webm).status().is_ok_and(|s| s.success()) {
            let info = probe(&webm).expect("probes the WebM");
            assert_eq!(info.decoder, Some("libvpx-vp9"));
            let f = decode(&webm, &info, (32, 32)).expect("decodes the WebM");
            let a = f.frames[0][3];
            assert!((100..160).contains(&a), "half transparent: {a}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
