//! Rendering a new video from the source: ffmpeg decodes the source to raw
//! frames, each one is drawn here, and ffmpeg encodes the result. The same
//! size and frame rate, one frame per frame of the grid over the frames asked
//! for (output frame k is grid frame `frames.start + k` of the source; VFR
//! gaps repeat), so it lines up with everything measured on the source, in
//! any editor, with nothing to keep working there (no plug-in, no
//! expression). The sound comes along, cut to the same frames.
//!
//! - [`render_warped`]: every frame moved and turned (sub-pixel, bilinear),
//!   e.g. stabilized. 4:4:4 in between, 16 bits a sample for sources deeper
//!   than 8, the colour tags and the first audio stream carried over.
//! - [`render_marker`]: a tracking target: a high-contrast marker on black
//!   that goes (and turns) where something does, for any editor's tracker to
//!   follow.

use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use tt_core::time::FrameIndex;

use crate::VideoIndex;

/// What to encode to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Codec {
    /// Apple ProRes 422 HQ, 10-bit (.mov): what editors handle best; large.
    #[default]
    ProRes422Hq,
    /// Avid DNxHR HQX, 10-bit (.mov).
    DnxhrHqx,
    /// H.264, 8-bit, near lossless (.mp4): smaller, plays anywhere.
    H264,
}

impl Codec {
    pub const ALL: [Codec; 3] = [Codec::ProRes422Hq, Codec::DnxhrHqx, Codec::H264];

    pub fn label(self) -> &'static str {
        match self {
            Codec::ProRes422Hq => "ProRes 422 HQ (.mov, 10-bit, best for editing, large)",
            Codec::DnxhrHqx => "DNxHR HQX (.mov, 10-bit, for Avid/Resolve)",
            Codec::H264 => "H.264 (.mp4, 8-bit, near lossless, smaller)",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Codec::H264 => "mp4",
            _ => "mov",
        }
    }

    pub(crate) fn video_args(self) -> &'static [&'static str] {
        match self {
            Codec::ProRes422Hq => &["-c:v", "prores_ks", "-profile:v", "3", "-vendor", "apl0", "-pix_fmt", "yuv422p10le"],
            Codec::DnxhrHqx => &["-c:v", "dnxhd", "-profile:v", "dnxhr_hqx", "-pix_fmt", "yuv422p10le"],
            Codec::H264 => &["-c:v", "libx264", "-preset", "slow", "-crf", "12", "-pix_fmt", "yuv420p", "-movflags", "+faststart"],
        }
    }

    fn audio_args(self) -> &'static [&'static str] {
        match self {
            Codec::H264 => &["-c:a", "aac", "-b:a", "320k"],
            _ => &["-c:a", "pcm_s16le"],
        }
    }
}

/// A map from output pixels to source pixels: `[a, b, c, d, e, f]` takes
/// (x, y) to (a·x + b·y + c, d·x + e·y + f). Continuous pixels: pixel (i, j)
/// covers [i, i+1) × [j, j+1), so its centre is (i + ½, j + ½).
pub type Affine = [f64; 6];

pub const IDENTITY: Affine = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0];

/// The map the other way (source pixels → output pixels); None if `m`
/// flattens the picture.
pub fn invert(m: &Affine) -> Option<Affine> {
    let det = m[0] * m[4] - m[1] * m[3];
    if det.abs() < 1e-12 || !det.is_finite() {
        return None;
    }
    let (a, b, d, e) = (m[4] / det, -m[1] / det, -m[3] / det, m[0] / det);
    Some([a, b, -(a * m[2] + b * m[5]), d, e, -(d * m[2] + e * m[5])])
}

/// A small RGB picture (`out` = width, height; 3 bytes a pixel, rows top
/// down) of an NV12 frame of `w` × `h` (as the decoder gives it), drawn
/// through `map` (picture pixels → frame pixels, as [`Affine`]): four
/// nearest samples a pixel, averaged; outside the frame, black. For a
/// preview: quick, not exact.
pub fn nv12_preview(nv12: &[u8], w: usize, h: usize, color: crate::ColorInfo, map: &Affine, out: [usize; 2]) -> Vec<u8> {
    let [ow, oh] = out;
    let mut rgb = vec![0u8; ow * oh * 3];
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    if w == 0 || h == 0 || nv12.len() < w * h + cw * ch * 2 {
        return rgb;
    }
    let (luma, chroma) = nv12.split_at(w * h);
    // As the viewport's shader: the range expanded, then BT.709 or BT.601.
    let (bt601, full) = (color.matrix == crate::Matrix::Bt601, color.full_range);
    let to_rgb = |y: u8, u: u8, v: u8| -> [f32; 3] {
        let (y, u, v) = (y as f32 / 255.0, u as f32 / 255.0, v as f32 / 255.0);
        let (y, cb, cr) = if full { (y, u - 0.5, v - 0.5) } else { ((y - 16.0 / 255.0) * (255.0 / 219.0), (u - 128.0 / 255.0) * (255.0 / 224.0), (v - 128.0 / 255.0) * (255.0 / 224.0)) };
        let c = if bt601 { [y + 1.402 * cr, y - 0.344136 * cb - 0.714136 * cr, y + 1.772 * cb] } else { [y + 1.5748 * cr, y - 0.187324 * cb - 0.468124 * cr, y + 1.8556 * cb] };
        c.map(|v| v.clamp(0.0, 1.0))
    };
    for (j, row) in rgb.chunks_mut(ow * 3).enumerate() {
        for (i, px) in row.chunks_mut(3).enumerate() {
            let mut sum = [0.0f32; 3];
            for (dx, dy) in [(0.25, 0.25), (0.75, 0.25), (0.25, 0.75), (0.75, 0.75)] {
                let (x, y) = (i as f64 + dx, j as f64 + dy);
                let (sx, sy) = (map[0] * x + map[1] * y + map[2], map[3] * x + map[4] * y + map[5]);
                if !(sx >= 0.0 && sy >= 0.0 && sx < w as f64 && sy < h as f64) {
                    continue;
                }
                let (sx, sy) = (sx as usize, sy as usize);
                let c = (sy / 2) * cw * 2 + (sx / 2) * 2;
                let s = to_rgb(luma[sy * w + sx], chroma[c], chroma[c + 1]);
                sum = [sum[0] + s[0], sum[1] + s[1], sum[2] + s[2]];
            }
            for (p, s) in px.iter_mut().zip(sum) {
                *p = (s / 4.0 * 255.0).round() as u8;
            }
        }
    }
    rgb
}

/// What the render needs to know about the source's video and audio.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StreamInfo {
    /// More than 8 bits a sample.
    pub deep: bool,
    pub full_range: bool,
    /// ffmpeg's colour options describing it (`-colorspace bt709`, …).
    pub tags: Vec<(String, String)>,
    pub audio: bool,
}

pub fn probe_stream(path: &Path) -> StreamInfo {
    let mut cmd = command(&crate::ffmpeg::tool("ffprobe", "FFPROBE"));
    cmd.args(["-v", "error", "-show_entries", "stream=codec_type,pix_fmt,color_space,color_range,color_primaries,color_transfer", "-of", "default=nw=1"]).arg(path);
    let Ok(out) = cmd.output() else { return StreamInfo::default() };
    parse_probe(&String::from_utf8_lossy(&out.stdout))
}

fn parse_probe(text: &str) -> StreamInfo {
    let mut info = StreamInfo::default();
    let (mut kind, mut seen_video) = (String::new(), false);
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        if k == "codec_type" {
            if kind == "video" {
                seen_video = true;
            }
            kind = v.to_string();
            info.audio |= v == "audio";
            continue;
        }
        if kind != "video" || seen_video || v.is_empty() || v == "unknown" {
            continue;
        }
        match k {
            "pix_fmt" => info.deep = ["10", "12", "14", "16", "p010", "p016"].iter().any(|d| v.contains(d)),
            "color_range" => {
                info.full_range = v == "pc";
                info.tags.push(("-color_range".into(), v.into()));
            }
            "color_space" => info.tags.push(("-colorspace".into(), v.into())),
            "color_primaries" => info.tags.push(("-color_primaries".into(), v.into())),
            "color_transfer" => info.tags.push(("-color_trc".into(), v.into())),
            _ => {}
        }
    }
    info
}

pub(crate) fn command(program: &Path) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
}

/// A child's stderr, kept (the last lines) for error reports.
pub(crate) fn keep_log(child: &mut Child) -> Arc<Mutex<Vec<String>>> {
    let log = Arc::new(Mutex::new(Vec::new()));
    if let Some(stderr) = child.stderr.take() {
        let sink = log.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut l = sink.lock().expect("log");
                l.push(line);
                if l.len() > 30 {
                    l.remove(0);
                }
            }
        });
    }
    log
}

pub(crate) fn last_lines(log: &Arc<Mutex<Vec<String>>>) -> String {
    log.lock().map(|l| l.join("\n")).unwrap_or_default()
}

/// The file written while rendering: renamed to `out` when complete.
pub(crate) fn partial(out: &Path) -> PathBuf {
    let mut name = out.file_name().unwrap_or_default().to_owned();
    name.push(".part");
    out.with_file_name(name)
}

/// The sound to carry over: `from`'s first audio stream, `seconds` of it from `start`.
pub(crate) struct Sound<'a> {
    pub(crate) from: &'a Path,
    pub(crate) start: f64,
    pub(crate) seconds: f64,
}

impl<'a> Sound<'a> {
    /// The source's sound under grid frames `frames`.
    pub(crate) fn under(index: &'a VideoIndex, frames: &Range<FrameIndex>) -> Self {
        let fps = index.fps.as_f64();
        Self { from: &index.path, start: frames.start as f64 / fps, seconds: (frames.end - frames.start) as f64 / fps }
    }
}

/// `frames` within the video (grid frames); an error if none are.
pub(crate) fn within(index: &VideoIndex, frames: Range<FrameIndex>) -> Result<Range<FrameIndex>> {
    let frames = frames.start.max(0)..frames.end.min(index.frame_count());
    if frames.is_empty() {
        bail!("no frames to render");
    }
    Ok(frames)
}

/// Everything a frame of the encode needs: the encoder, its input and its log.
pub(crate) struct Encoder {
    pub(crate) child: Child,
    pub(crate) input: BufWriter<std::process::ChildStdin>,
    pub(crate) log: Arc<Mutex<Vec<String>>>,
    /// The file being written, renamed to the output when complete (None: written in place, a picture sequence).
    pub(crate) part: Option<PathBuf>,
}

impl Encoder {
    /// ffmpeg reading raw `pix` frames of `w` × `h` at the grid's rate from its stdin, encoding them with `codec` to `out`'s partial file (and `sound` along).
    pub(crate) fn start(index: &VideoIndex, out: &Path, codec: Codec, pix: &str, tags: &[(String, String)], sound: Option<Sound>) -> Result<Self> {
        let part = partial(out);
        let mut cmd = command(&crate::ffmpeg::tool("ffmpeg", "FFMPEG"));
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-f", "rawvideo", "-pix_fmt", pix])
            .args(["-s", &format!("{}x{}", index.width, index.height), "-r", &format!("{}/{}", index.fps.num, index.fps.den)]);
        for (k, v) in tags {
            cmd.arg(k).arg(v);
        }
        cmd.args(["-i", "pipe:0"]);
        match sound {
            Some(Sound { from, start, seconds }) => {
                if start > 0.0 {
                    cmd.args(["-ss", &format!("{start:.6}")]);
                }
                cmd.args(["-t", &format!("{seconds:.6}")]).arg("-i").arg(from).args(["-map", "0:v:0", "-map", "1:a:0"]).args(codec.audio_args());
            }
            None => {
                cmd.args(["-map", "0:v:0"]);
            }
        }
        cmd.args(codec.video_args());
        for (k, v) in tags {
            cmd.arg(k).arg(v);
        }
        // The container: the extension of `out`, not of the partial file.
        cmd.args(["-f", if codec.extension() == "mp4" { "mp4" } else { "mov" }]).arg(&part);
        cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
        let mut child = cmd.spawn().context("starting ffmpeg to encode")?;
        let log = keep_log(&mut child);
        let input = BufWriter::with_capacity(16 << 20, child.stdin.take().context("ffmpeg stdin")?);
        Ok(Self { child, input, log, part: Some(part) })
    }

    pub(crate) fn write(&mut self, frame: &[u8]) -> Result<()> {
        self.input.write_all(frame).map_err(|e| anyhow!("ffmpeg stopped taking frames ({e}): {}", last_lines(&self.log)))
    }

    pub(crate) fn finish(self, out: &Path) -> Result<()> {
        let Encoder { mut child, input, log, part } = self;
        drop(input.into_inner().map_err(|e| anyhow!("ffmpeg stopped taking frames: {e}"))?);
        let status = child.wait()?;
        if !status.success() {
            if let Some(part) = &part {
                let _ = std::fs::remove_file(part);
            }
            bail!("ffmpeg could not encode: {}", last_lines(&log));
        }
        let Some(part) = part else { return Ok(()) };
        if out.exists() {
            std::fs::remove_file(out).with_context(|| format!("replacing {}", out.display()))?;
        }
        std::fs::rename(&part, out).with_context(|| format!("writing {}", out.display()))
    }

    pub(crate) fn abandon(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(part) = &self.part {
            let _ = std::fs::remove_file(part);
        }
    }
}

/// Renders grid frames `frames` of `index`'s video to `out`, grid frame g
/// drawn from the source through `map(g)` (output pixels → source pixels);
/// outside the source, black. `progress` counts frames done; `cancel` stops
/// it (and removes the file).
pub fn render_warped(
    index: &VideoIndex,
    out: &Path,
    codec: Codec,
    frames: Range<FrameIndex>,
    map: &(dyn Fn(FrameIndex) -> Affine + Sync),
    progress: &AtomicUsize,
    cancel: &AtomicBool,
) -> Result<()> {
    let frames = within(index, frames)?;
    let info = probe_stream(&index.path);
    let (w, h) = (index.width as usize, index.height as usize);
    let (pix, bytes) = if info.deep { ("yuv444p16le", 2) } else { ("yuv444p", 1) };
    let plane = w * h * bytes;
    // Decoding starts exactly at the first frame asked for.
    let first = index.presented_at(frames.start);
    let (before, after) = crate::ffmpeg::seek_args(index, first);
    let mut dec = command(&crate::ffmpeg::tool("ffmpeg", "FFMPEG"));
    dec.args(["-hide_banner", "-loglevel", "error", "-nostdin"])
        .args(before)
        .arg("-i")
        .arg(&index.path)
        .args(after)
        .args(["-map", "0:v:0", "-an", "-sn", "-dn", "-fps_mode", "passthrough", "-pix_fmt", pix, "-f", "rawvideo", "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut decoder = dec.spawn().context("starting ffmpeg to decode")?;
    let dec_log = keep_log(&mut decoder);
    let mut decoded = BufReader::with_capacity(16 << 20, decoder.stdout.take().context("ffmpeg stdout")?);
    let mut encoder = Encoder::start(index, out, codec, pix, &info.tags, info.audio.then(|| Sound::under(index, &frames)))?;
    // Black, as the source codes it: limited range starts at 16 (16-bit: 16 << 8); chroma in the middle.
    let (black, mid) = match (info.deep, info.full_range) {
        (true, false) => (16u32 << 8, 1u32 << 15),
        (true, true) => (0, 1 << 15),
        (false, false) => (16, 128),
        (false, true) => (0, 128),
    };
    let mut src = vec![0u8; plane * 3];
    let mut dst = vec![0u8; plane * 3];
    // The presented frame last read.
    let mut have: Option<usize> = None;
    let result = (|| -> Result<()> {
        for (k, g) in frames.clone().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                bail!("cancelled");
            }
            let want = index.presented_at(g);
            while have.is_none_or(|p| p < want) {
                match decoded.read_exact(&mut src) {
                    Ok(()) => have = Some(have.map_or(first, |p| p + 1)),
                    // Fewer frames than the index says (a damaged end): the last one repeats.
                    Err(_) if have.is_some() => break,
                    Err(e) => bail!("ffmpeg gave no frames ({e}): {}", last_lines(&dec_log)),
                }
            }
            let m = map(g);
            for (k, fill) in [black, mid, mid].into_iter().enumerate() {
                let (s, d) = (&src[k * plane..(k + 1) * plane], &mut dst[k * plane..(k + 1) * plane]);
                if bytes == 2 { warp::<2>(s, d, w, h, &m, fill) } else { warp::<1>(s, d, w, h, &m, fill) }
            }
            encoder.write(&dst)?;
            progress.store(k + 1, Ordering::Relaxed);
        }
        Ok(())
    })();
    let _ = decoder.kill();
    let _ = decoder.wait();
    match result {
        Ok(()) => encoder.finish(out),
        Err(e) => {
            encoder.abandon();
            Err(e)
        }
    }
}

/// Renders a tracking target the size of `index`'s video over its grid
/// frames `frames`: on black, a marker where `at(g)` says (its centre in
/// source px, its turn in radians, clockwise on screen), `half` px from its
/// centre to its side: a white-bordered checker (its crossing is what
/// trackers lock onto) and a smaller one beside it on its own x axis (for
/// rotation). No `at`: black.
#[allow(clippy::too_many_arguments)]
pub fn render_marker(
    index: &VideoIndex,
    out: &Path,
    codec: Codec,
    frames: Range<FrameIndex>,
    at: &(dyn Fn(FrameIndex) -> Option<([f64; 2], f64)> + Sync),
    half: f64,
    progress: &AtomicUsize,
    cancel: &AtomicBool,
) -> Result<()> {
    let frames = within(index, frames)?;
    let (w, h) = (index.width as usize, index.height as usize);
    let tags = [("-color_range".to_string(), "pc".to_string())];
    let mut encoder = Encoder::start(index, out, codec, "gray", &tags, None)?;
    let mut frame = vec![0u8; w * h];
    let result = (|| -> Result<()> {
        for (k, g) in frames.clone().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                bail!("cancelled");
            }
            frame.fill(0);
            if let Some((c, angle)) = at(g) {
                draw_marker(&mut frame, w, h, c, angle, half);
            }
            encoder.write(&frame)?;
            progress.store(k + 1, Ordering::Relaxed);
        }
        Ok(())
    })();
    match result {
        Ok(()) => encoder.finish(out),
        Err(e) => {
            encoder.abandon();
            Err(e)
        }
    }
}

/// One plane of `w` × `h` samples of `B` bytes (little-endian): `dst` drawn
/// from `src` through `m` (bilinear; outside the source, `fill`). Rows are
/// shared out among the CPU's threads.
fn warp<const B: usize>(src: &[u8], dst: &mut [u8], w: usize, h: usize, m: &Affine, fill: u32) {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 32);
    let rows = h.div_ceil(threads).max(1);
    let get = |i: usize| -> f32 {
        if B == 2 { u16::from_le_bytes([src[2 * i], src[2 * i + 1]]) as f32 } else { src[i] as f32 }
    };
    std::thread::scope(|scope| {
        for (n, chunk) in dst.chunks_mut(rows * w * B).enumerate() {
            scope.spawn(move || {
                for (r, row) in chunk.chunks_mut(w * B).enumerate() {
                    let y = (n * rows + r) as f64 + 0.5;
                    // The source position of this row's first pixel centre, then a step a pixel.
                    let (mut sx, mut sy) = (m[0] * 0.5 + m[1] * y + m[2], m[3] * 0.5 + m[4] * y + m[5]);
                    for x in 0..w {
                        let v = sample(&get, w, h, sx - 0.5, sy - 0.5).map_or(fill, |v| v.round() as u32);
                        if B == 2 {
                            row[2 * x..2 * x + 2].copy_from_slice(&(v as u16).to_le_bytes());
                        } else {
                            row[x] = v as u8;
                        }
                        sx += m[0];
                        sy += m[3];
                    }
                }
            });
        }
    });
}

/// Bilinear at (x, y) in sample indices (sample (i, j)'s centre is at (i, j));
/// None outside the plane's extent (half a sample past the edge samples).
fn sample(get: &impl Fn(usize) -> f32, w: usize, h: usize, x: f64, y: f64) -> Option<f32> {
    if !(x >= -0.5 && y >= -0.5 && x <= w as f64 - 0.5 && y <= h as f64 - 0.5) {
        return None;
    }
    let (x0, y0) = (x.floor(), y.floor());
    let (fx, fy) = ((x - x0) as f32, (y - y0) as f32);
    let clamp = |v: f64, n: usize| (v.max(0.0) as usize).min(n - 1);
    let (xa, xb, ya, yb) = (clamp(x0, w), clamp(x0 + 1.0, w), clamp(y0, h), clamp(y0 + 1.0, h));
    let top = get(ya * w + xa) * (1.0 - fx) + get(ya * w + xb) * fx;
    let bottom = get(yb * w + xa) * (1.0 - fx) + get(yb * w + xb) * fx;
    Some(top * (1.0 - fy) + bottom * fy)
}

/// The marker (see [`render_marker`]), anti-aliased (4 × 4 samples a pixel),
/// drawn into an 8-bit luma plane.
fn draw_marker(y: &mut [u8], w: usize, h: usize, centre: [f64; 2], angle: f64, half: f64) {
    let (s, c) = angle.sin_cos();
    let beside = [centre[0] + c * 3.0 * half, centre[1] + s * 3.0 * half];
    for (at, size) in [(centre, half), (beside, half / 2.0)] {
        let border = (size / 6.0).max(1.5);
        // Inside: the checker's light quadrants (top left and bottom right, in its own turn) and the border.
        let lit = |px: f64, py: f64| {
            let (dx, dy) = (px - at[0], py - at[1]);
            let (u, v) = (c * dx + s * dy, -s * dx + c * dy);
            if u.abs() > size || v.abs() > size {
                return false;
            }
            u.abs() > size - border || v.abs() > size - border || (u < 0.0) == (v < 0.0)
        };
        let reach = size * std::f64::consts::SQRT_2 + 1.0;
        let (x0, x1) = (((at[0] - reach).floor().max(0.0)) as usize, ((at[0] + reach).ceil().max(0.0) as usize).min(w));
        let (y0, y1) = (((at[1] - reach).floor().max(0.0)) as usize, ((at[1] + reach).ceil().max(0.0) as usize).min(h));
        for py in y0..y1 {
            for px in x0..x1 {
                let mut n = 0;
                for sy in 0..4 {
                    for sx in 0..4 {
                        n += lit(px as f64 + (sx as f64 + 0.5) / 4.0, py as f64 + (sy as f64 + 0.5) / 4.0) as u32;
                    }
                }
                let v = (n * 255 / 16) as u8;
                let p = &mut y[py * w + px];
                *p = (*p).max(v);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_warp_moves_and_turns_the_picture_sub_pixel() {
        let (w, h) = (40usize, 30usize);
        // A plane whose value is its x (so a sample says where it came from).
        let src: Vec<u8> = (0..w * h).map(|i| (i % w) as u8 * 4).collect();
        let mut dst = vec![0u8; w * h];
        // Moved 2.5 px right: output x takes source x − 2.5.
        warp::<1>(&src, &mut dst, w, h, &[1.0, 0.0, -2.5, 0.0, 1.0, 0.0], 255);
        assert_eq!(dst[10 * w], 255, "the left edge, uncovered, is filled");
        assert_eq!(dst[10 * w + 20], ((20.0 - 2.5) * 4.0f64).round() as u8, "sub-pixel: half way between two samples");
        // 16-bit: the same, little-endian.
        let src16: Vec<u8> = (0..w * h).flat_map(|i| (((i % w) * 1000) as u16).to_le_bytes()).collect();
        let mut dst16 = vec![0u8; w * h * 2];
        warp::<2>(&src16, &mut dst16, w, h, &IDENTITY, 0);
        assert_eq!(dst16, src16, "the identity leaves it as it is");
    }

    #[test]
    fn a_preview_is_the_frame_through_the_map_in_colour_and_black_outside() {
        use crate::{ColorInfo, Matrix};
        // 8 × 4, limited range: the left half black, the right half white; neutral chroma.
        let (w, h) = (8usize, 4usize);
        let mut frame: Vec<u8> = (0..w * h).map(|i| if i % w < 4 { 16 } else { 235 }).collect();
        frame.extend(std::iter::repeat_n(128u8, w.div_ceil(2) * h.div_ceil(2) * 2));
        let limited = ColorInfo { matrix: Matrix::Bt709, full_range: false };
        // Half size: a picture pixel is two frame pixels.
        let half = nv12_preview(&frame, w, h, limited, &[2.0, 0.0, 0.0, 0.0, 2.0, 0.0], [4, 2]);
        assert_eq!(half.len(), 4 * 2 * 3);
        assert_eq!(&half[..3], &[0, 0, 0], "black stays black");
        assert_eq!(&half[3 * 3..4 * 3], &[255, 255, 255], "white, the range expanded");
        // Moved 6 frame pixels left: the right of the picture is outside the frame, black.
        let moved = nv12_preview(&frame, w, h, limited, &[2.0, 0.0, 6.0, 0.0, 2.0, 0.0], [4, 2]);
        assert_eq!(&moved[..3], &[255, 255, 255]);
        assert_eq!(&moved[3..6], &[0, 0, 0], "outside: black");
        // Half a picture pixel over the edge: two samples of four inside, half as bright.
        let edge = nv12_preview(&frame, w, h, limited, &[2.0, 0.0, 7.0, 0.0, 2.0, 0.0], [1, 1]);
        assert_eq!(edge, vec![128, 128, 128]);
        // Colour: BT.709 limited red, and full range BT.601 grey.
        let mut red = vec![63u8; 4];
        red.extend([102, 240]);
        let r = nv12_preview(&red, 2, 2, limited, &IDENTITY, [1, 1]);
        assert!(r[0] == 255 && r[1] < 3 && r[2] < 3, "{r:?}");
        let grey = nv12_preview(&[100, 100, 100, 100, 128, 128], 2, 2, ColorInfo { matrix: Matrix::Bt601, full_range: true }, &IDENTITY, [1, 1]);
        assert!(grey.iter().all(|&v| v.abs_diff(100) <= 1), "{grey:?}");
        // Too little data: all black, no panic.
        assert_eq!(nv12_preview(&[1, 2, 3], w, h, limited, &IDENTITY, [2, 2]), vec![0; 12]);
    }

    #[test]
    fn an_inverted_map_takes_its_points_back() {
        let m = [1.3, -0.4, 20.0, 0.25, 0.9, -7.0];
        let back = invert(&m).expect("invertible");
        for p in [[0.0, 0.0], [100.0, 50.0], [-3.0, 400.0]] {
            let q = [m[0] * p[0] + m[1] * p[1] + m[2], m[3] * p[0] + m[4] * p[1] + m[5]];
            let r = [back[0] * q[0] + back[1] * q[1] + back[2], back[3] * q[0] + back[4] * q[1] + back[5]];
            assert!((r[0] - p[0]).abs() < 1e-9 && (r[1] - p[1]).abs() < 1e-9, "{p:?} {r:?}");
        }
        assert_eq!(invert(&IDENTITY), Some(IDENTITY));
        assert_eq!(invert(&[1.0, 2.0, 0.0, 2.0, 4.0, 0.0]), None, "flat");
    }

    #[test]
    fn the_marker_is_a_checker_where_it_is_told() {
        let (w, h) = (200usize, 120usize);
        let mut y = vec![0u8; w * h];
        draw_marker(&mut y, w, h, [80.0, 60.0], 0.0, 20.0);
        let at = |x: usize, yy: usize| y[yy * w + x];
        assert!(at(74, 54) > 250 && at(86, 66) > 250, "top left and bottom right are light");
        assert!(at(86, 54) < 5 && at(74, 66) < 5, "the other two dark");
        assert!(at(140, 60) > 250 || at(135, 55) > 250, "its partner sits on its x axis");
        assert!(at(20, 20) == 0, "the rest is black");
        // Turned a quarter: its partner goes below it (clockwise on screen, y down).
        let mut z = vec![0u8; w * h];
        draw_marker(&mut z, w, h, [80.0, 40.0], std::f64::consts::FRAC_PI_2, 12.0);
        // (Its partner's centre is (80, 76); a light quadrant, turned, is up and right of it.)
        assert!(z[73 * w + 83] > 250, "partner below, turned with it");
        assert_eq!(z[40 * w + 116], 0, "not on the right any more");
    }

    #[test]
    fn probing_reads_depth_colour_and_audio() {
        let text = "codec_type=video\npix_fmt=yuv420p10le\ncolor_space=bt709\ncolor_range=tv\ncolor_primaries=bt709\ncolor_transfer=unknown\ncodec_type=audio\n";
        let info = parse_probe(text);
        assert!(info.deep && info.audio && !info.full_range);
        assert_eq!(info.tags.len(), 3, "unknown values are left out: {:?}", info.tags);
    }
}
