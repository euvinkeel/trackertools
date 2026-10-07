//! Rendering layers (tt_core::layer): the media attached to what's tracked,
//! drawn where each layer's output places it on every frame.
//!
//! - [`render_over`]: the video with the layers on top. The source is read
//!   as 4:4:4 (16 bits a sample for sources deeper than 8) and each layer is
//!   blended straight into those planes (its colours converted with the
//!   source's own matrix and range), so a pixel no layer covers comes out
//!   exactly as it was; then encoded as the stabilized export is, with the
//!   sound and colour tags carried over.
//! - [`render_alpha`]: the layers alone on transparency, the size, frame
//!   rate and frames of the video, to line up on a track above it: ProRes
//!   4444 (.mov), a PNG sequence (a folder), or WebM (VP9 with alpha).
//!
//! A layer's picture is sampled bilinearly from its media's frames (read at
//! their own size, up to a memory limit: `overlay::fit`), its alpha times
//! its opacity, composited in the order given (bottom first).

use std::io::{BufReader, Read};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use anyhow::{Context, Result, bail};
use tt_core::layer::Placed;
use tt_core::time::FrameIndex;

use crate::VideoIndex;
use crate::overlay::Frames;
use crate::render::{Codec, Encoder, Sound, command, keep_log, last_lines, probe_stream, within};

/// One layer to draw: its media's frames, its media's own size (what its
/// placement is in), and where it is on each frame (`placed(g)`).
pub struct Overlay<'a> {
    pub frames: Arc<Frames>,
    pub size: [f32; 2],
    pub placed: &'a (dyn Fn(FrameIndex) -> Option<Placed> + Sync),
}

/// The transparent formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AlphaFormat {
    /// Apple ProRes 4444 with alpha (.mov): what editors read best.
    #[default]
    ProRes4444,
    /// One PNG a frame, in a folder.
    PngSequence,
    /// VP9 with alpha (.webm): small, for the web.
    WebM,
}

impl AlphaFormat {
    pub const ALL: [AlphaFormat; 3] = [AlphaFormat::ProRes4444, AlphaFormat::PngSequence, AlphaFormat::WebM];

    pub fn label(self) -> &'static str {
        match self {
            AlphaFormat::ProRes4444 => "ProRes 4444 with transparency (.mov, for editors)",
            AlphaFormat::PngSequence => "PNG pictures, one a frame (a folder)",
            AlphaFormat::WebM => "WebM with transparency (.webm, small)",
        }
    }

    /// The file's extension (a PNG sequence: the folder has none).
    pub fn extension(self) -> &'static str {
        match self {
            AlphaFormat::ProRes4444 => "mov",
            AlphaFormat::PngSequence => "",
            AlphaFormat::WebM => "webm",
        }
    }
}

/// The source's colour conversion: R'G'B' (0–1) to its Y'CbCr codes.
#[derive(Clone, Copy, Debug)]
pub struct ToYuv {
    kr: f64,
    kb: f64,
    full: bool,
    /// The codes' scale: 1 (8 bits) or 256 (16 bits).
    scale: f64,
}

impl ToYuv {
    /// For a source with `colorspace` (ffprobe's word; None: by height), `full` range, 16-bit codes or not.
    pub fn new(colorspace: Option<&str>, height: u32, full: bool, deep: bool) -> Self {
        let (kr, kb) = match colorspace {
            Some("bt470bg" | "smpte170m" | "fcc" | "bt601") => (0.299, 0.114),
            Some("bt2020nc" | "bt2020c") => (0.2627, 0.0593),
            Some(_) => (0.2126, 0.0722),
            None if height < 720 => (0.299, 0.114),
            None => (0.2126, 0.0722),
        };
        Self { kr, kb, full, scale: if deep { 256.0 } else { 1.0 } }
    }

    /// `[Y, Cb, Cr]` codes for R'G'B' in 0–1.
    pub fn yuv(&self, rgb: [f64; 3]) -> [f64; 3] {
        let [r, g, b] = rgb;
        let y = self.kr * r + (1.0 - self.kr - self.kb) * g + self.kb * b;
        let cb = (b - y) / (2.0 * (1.0 - self.kb));
        let cr = (r - y) / (2.0 * (1.0 - self.kr));
        let [y, cb, cr] = if self.full { [255.0 * y, 128.0 + 255.0 * cb, 128.0 + 255.0 * cr] } else { [16.0 + 219.0 * y, 128.0 + 224.0 * cb, 128.0 + 224.0 * cr] };
        [y * self.scale, cb * self.scale, cr * self.scale]
    }
}

/// The straight RGBA (0–1) of `f` at `(x, y)` in its own pixels (bilinear;
/// outside it, transparent).
fn sample(f: &[u8], fw: usize, fh: usize, x: f64, y: f64) -> [f64; 4] {
    let (x, y) = (x - 0.5, y - 0.5);
    let (x0, y0) = (x.floor(), y.floor());
    let (tx, ty) = (x - x0, y - y0);
    let mut acc = [0.0; 4];
    let mut weight = 0.0;
    for (dx, dy, w) in [(0, 0, (1.0 - tx) * (1.0 - ty)), (1, 0, tx * (1.0 - ty)), (0, 1, (1.0 - tx) * ty), (1, 1, tx * ty)] {
        let (px, py) = (x0 as i64 + dx, y0 as i64 + dy);
        // Clamped at the edges, so its border isn't half transparent from outside.
        let (cx, cy) = (px.clamp(0, fw as i64 - 1) as usize, py.clamp(0, fh as i64 - 1) as usize);
        let i = (cy * fw + cx) * 4;
        let a = f[i + 3] as f64 / 255.0;
        // Premultiplied while averaging (no dark fringes), straight after.
        for c in 0..3 {
            acc[c] += w * a * f[i + c] as f64 / 255.0;
        }
        acc[3] += w * a;
        weight += w;
    }
    let a = acc[3] / weight.max(1e-12);
    if a <= 1e-9 {
        return [0.0; 4];
    }
    [acc[0] / weight / a, acc[1] / weight / a, acc[2] / weight / a, a]
}

/// The output pixels a placed layer may cover: `(x0, y0, x1, y1)`, clipped to `w` × `h`.
fn bounds(pl: &Placed, size: [f32; 2], w: usize, h: usize) -> Option<(usize, usize, usize, usize)> {
    let c = pl.corners(size);
    let (xs, ys) = (c.map(|p| p[0]), c.map(|p| p[1]));
    let (x0, x1) = (xs.iter().copied().fold(f64::INFINITY, f64::min).floor().max(0.0), xs.iter().copied().fold(f64::NEG_INFINITY, f64::max).ceil().min(w as f64));
    let (y0, y1) = (ys.iter().copied().fold(f64::INFINITY, f64::min).floor().max(0.0), ys.iter().copied().fold(f64::NEG_INFINITY, f64::max).ceil().min(h as f64));
    (x1 > x0 && y1 > y0).then_some((x0 as usize, y0 as usize, x1 as usize, y1 as usize))
}

/// For every output pixel of rows `rows` a layer covers: `f(row, x, rgba)`,
/// rgba straight with the layer's opacity in its alpha.
fn cover(ov: &Overlay, pl: &Placed, frame: &[u8], w: usize, rows: Range<usize>, mut f: impl FnMut(usize, usize, [f64; 4])) {
    let (fw, fh) = (ov.frames.width as usize, ov.frames.height as usize);
    let Some((x0, y0, x1, y1)) = bounds(pl, ov.size, w, usize::MAX) else { return };
    // Media px → the frames' own px (they may be smaller: read to fit memory).
    let (kx, ky) = (fw as f64 / ov.size[0].max(1.0) as f64, fh as f64 / ov.size[1].max(1.0) as f64);
    for y in rows.start.max(y0)..rows.end.min(y1) {
        for x in x0..x1 {
            let m = pl.from_source(ov.size, [x as f64 + 0.5, y as f64 + 0.5]);
            if m[0] < 0.0 || m[1] < 0.0 || m[0] >= ov.size[0] as f64 || m[1] >= ov.size[1] as f64 {
                continue;
            }
            let mut c = sample(frame, fw, fh, m[0] * kx, m[1] * ky);
            c[3] *= pl.opacity;
            if c[3] > 0.0 {
                f(y, x, c);
            }
        }
    }
}

/// The overlays on grid frame `g`, each with the media frame it shows.
fn on_frame<'o, 'a>(overlays: &'o [Overlay<'a>], g: FrameIndex) -> Vec<(&'o Overlay<'a>, Placed, &'o [u8])> {
    overlays.iter().filter_map(|ov| (ov.placed)(g).filter(|p| p.opacity > 0.0).and_then(|p| Some((ov, p, ov.frames.at(p.clip_time)?)))).collect()
}

/// Rows a thread takes: the frame's height shared out among the CPU's threads.
fn rows_each(h: usize) -> usize {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 32);
    h.div_ceil(threads).max(1)
}

/// Blends the colour `yuv` at alpha `a` into sample `i` of a plane (`B` bytes a sample).
fn blend<const B: usize>(plane: &mut [u8], i: usize, v: f64, a: f64) {
    if B == 2 {
        let old = u16::from_le_bytes([plane[2 * i], plane[2 * i + 1]]) as f64;
        let n = (old + (v - old) * a).round().clamp(0.0, 65535.0) as u16;
        plane[2 * i..2 * i + 2].copy_from_slice(&n.to_le_bytes());
    } else {
        let old = plane[i] as f64;
        plane[i] = (old + (v - old) * a).round().clamp(0.0, 255.0) as u8;
    }
}

/// The layers over 4:4:4 planes `buf` (`w` × `h`, `B` bytes a sample), a band of rows per thread.
fn composite<const B: usize>(buf: &mut [u8], w: usize, h: usize, on: &[(&Overlay, Placed, &[u8])], conv: &ToYuv) {
    if on.is_empty() {
        return;
    }
    let plane = w * h * B;
    let (y, rest) = buf.split_at_mut(plane);
    let (u, v) = rest.split_at_mut(plane);
    let step = rows_each(h);
    let band = step * w * B;
    std::thread::scope(|s| {
        for (n, ((yb, ub), vb)) in y.chunks_mut(band).zip(u.chunks_mut(band)).zip(v.chunks_mut(band)).enumerate() {
            s.spawn(move || {
                let first = n * step;
                let rows = first..(first + step).min(h);
                for (ov, pl, frame) in on {
                    cover(ov, pl, frame, w, rows.clone(), |yy, x, c| {
                        let i = (yy - first) * w + x;
                        let [cy, cb, cr] = conv.yuv([c[0], c[1], c[2]]);
                        blend::<B>(yb, i, cy, c[3]);
                        blend::<B>(ub, i, cb, c[3]);
                        blend::<B>(vb, i, cr, c[3]);
                    });
                }
            });
        }
    });
}

/// Renders grid frames `frames` of `index`'s video with `overlays` on top
/// (bottom first), encoded with `codec` (module docs). `progress` counts
/// frames done; `cancel` stops it (and removes the file).
pub fn render_over(index: &VideoIndex, out: &Path, codec: Codec, frames: Range<FrameIndex>, overlays: &[Overlay], progress: &AtomicUsize, cancel: &AtomicBool) -> Result<()> {
    let frames = within(index, frames)?;
    let info = probe_stream(&index.path);
    let (w, h) = (index.width as usize, index.height as usize);
    let (pix, bytes) = if info.deep { ("yuv444p16le", 2) } else { ("yuv444p", 1) };
    let colorspace = info.tags.iter().find(|(k, _)| k == "-colorspace").map(|(_, v)| v.as_str());
    let conv = ToYuv::new(colorspace, index.height, info.full_range, info.deep);
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
    let mut buf = vec![0u8; w * h * bytes * 3];
    let mut out_frame = vec![0u8; w * h * bytes * 3];
    let mut have: Option<usize> = None;
    let result = (|| -> Result<()> {
        for (k, g) in frames.clone().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                bail!("cancelled");
            }
            let want = index.presented_at(g);
            while have.is_none_or(|p| p < want) {
                match decoded.read_exact(&mut buf) {
                    Ok(()) => have = Some(have.map_or(first, |p| p + 1)),
                    Err(_) if have.is_some() => break,
                    Err(e) => bail!("ffmpeg gave no frames ({e}): {}", last_lines(&dec_log)),
                }
            }
            // (A repeated frame — VFR — must start from the frame as decoded, not as drawn on.)
            out_frame.copy_from_slice(&buf);
            let on = on_frame(overlays, g);
            if bytes == 2 { composite::<2>(&mut out_frame, w, h, &on, &conv) } else { composite::<1>(&mut out_frame, w, h, &on, &conv) }
            encoder.write(&out_frame)?;
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

/// The layers over transparency on one frame: straight RGBA, 8 bits.
pub fn draw_alpha(w: usize, h: usize, on: &[(&Overlay, Placed, &[u8])]) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 4];
    if on.is_empty() {
        return out;
    }
    let step = rows_each(h);
    std::thread::scope(|s| {
        for (n, band) in out.chunks_mut(step * w * 4).enumerate() {
            s.spawn(move || {
                let first = n * step;
                let rows = first..(first + step).min(h);
                // Premultiplied, in floats, while compositing; straight at the end.
                let mut acc = vec![[0f32; 4]; band.len() / 4];
                for (ov, pl, frame) in on {
                    cover(ov, pl, frame, w, rows.clone(), |yy, x, c| {
                        let px = &mut acc[(yy - first) * w + x];
                        let a = c[3] as f32;
                        for k in 0..3 {
                            px[k] = c[k] as f32 * a + px[k] * (1.0 - a);
                        }
                        px[3] = a + px[3] * (1.0 - a);
                    });
                }
                for (i, px) in acc.iter().enumerate() {
                    let a = px[3];
                    if a > 0.0 {
                        for k in 0..3 {
                            band[4 * i + k] = (px[k] / a * 255.0).round().clamp(0.0, 255.0) as u8;
                        }
                        band[4 * i + 3] = (a * 255.0).round().clamp(0.0, 255.0) as u8;
                    }
                }
            });
        }
    });
    out
}

/// Renders the layers alone on transparency (module docs) for grid frames
/// `frames` of `index`'s video. A PNG sequence: `out` is a folder (made),
/// its pictures named after it and numbered by grid frame.
pub fn render_alpha(index: &VideoIndex, out: &Path, format: AlphaFormat, frames: Range<FrameIndex>, overlays: &[Overlay], progress: &AtomicUsize, cancel: &AtomicBool) -> Result<()> {
    let frames = within(index, frames)?;
    let (w, h) = (index.width as usize, index.height as usize);
    let mut cmd = command(&crate::ffmpeg::tool("ffmpeg", "FFMPEG"));
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-f", "rawvideo", "-pix_fmt", "rgba"])
        .args(["-s", &format!("{w}x{h}"), "-r", &format!("{}/{}", index.fps.num, index.fps.den), "-i", "pipe:0"]);
    let tags = ["-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709"];
    let (target, part): (PathBuf, Option<PathBuf>) = match format {
        AlphaFormat::ProRes4444 => {
            cmd.args(["-c:v", "prores_ks", "-profile:v", "4444", "-vendor", "apl0", "-alpha_bits", "16", "-pix_fmt", "yuva444p10le", "-vf", "scale=out_color_matrix=bt709:out_range=tv"]).args(tags);
            let part = crate::render::partial(out);
            cmd.args(["-f", "mov"]).arg(&part);
            (out.to_path_buf(), Some(part))
        }
        AlphaFormat::WebM => {
            cmd.args(["-c:v", "libvpx-vp9", "-pix_fmt", "yuva420p", "-b:v", "0", "-crf", "18", "-row-mt", "1", "-auto-alt-ref", "0", "-vf", "scale=out_color_matrix=bt709:out_range=tv"]).args(tags);
            let part = crate::render::partial(out);
            cmd.args(["-f", "webm"]).arg(&part);
            (out.to_path_buf(), Some(part))
        }
        AlphaFormat::PngSequence => {
            std::fs::create_dir_all(out).with_context(|| format!("making {}", out.display()))?;
            let stem = out.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "layers".into());
            let pattern = out.join(format!("{stem}_%06d.png"));
            cmd.args(["-c:v", "png", "-pix_fmt", "rgba", "-start_number", &frames.start.to_string(), "-f", "image2"]).arg(&pattern);
            (out.to_path_buf(), None)
        }
    };
    cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
    let mut child = cmd.spawn().context("starting ffmpeg to encode")?;
    let log = keep_log(&mut child);
    let input = std::io::BufWriter::with_capacity(16 << 20, child.stdin.take().context("ffmpeg stdin")?);
    let mut encoder = Encoder { child, input, log, part };
    let result = (|| -> Result<()> {
        for (k, g) in frames.clone().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                bail!("cancelled");
            }
            let on = on_frame(overlays, g);
            encoder.write(&draw_alpha(w, h, &on))?;
            progress.store(k + 1, Ordering::Relaxed);
        }
        Ok(())
    })();
    match result {
        Ok(()) => encoder.finish(&target),
        Err(e) => {
            encoder.abandon();
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn red_square() -> Arc<Frames> {
        // 4 × 4: opaque red, its right half half transparent.
        let mut px = Vec::new();
        for _y in 0..4 {
            for x in 0..4 {
                px.extend([255, 0, 0, if x < 2 { 255 } else { 128 }]);
            }
        }
        Arc::new(Frames { width: 4, height: 4, fps: 0.0, frames: vec![px] })
    }

    fn placed(at: [f64; 2], scale: f64) -> Placed {
        Placed { at, angle: 0.0, scale: [scale, scale], opacity: 1.0, clip_time: 0.0, anchor: [0.5, 0.5] }
    }

    #[test]
    fn colours_convert_with_the_sources_matrix_and_range() {
        let hd = ToYuv::new(Some("bt709"), 1080, false, false);
        let [y, cb, cr] = hd.yuv([1.0, 0.0, 0.0]);
        assert!((y - 63.0).abs() < 1.0 && (cb - 102.0).abs() < 1.0 && (cr - 240.0).abs() < 1.0, "BT.709 limited red: {y} {cb} {cr}");
        let deep = ToYuv::new(None, 1080, false, true).yuv([1.0, 1.0, 1.0]);
        assert!((deep[0] - 235.0 * 256.0).abs() < 1.0 && (deep[1] - 128.0 * 256.0).abs() < 1.0);
    }

    #[test]
    fn layers_are_drawn_over_transparency_and_into_the_video() {
        let frames = red_square();
        let place = |g: FrameIndex| (g == 0).then(|| placed([10.0, 10.0], 2.0));
        let ov = Overlay { frames: frames.clone(), size: [4.0, 4.0], placed: &place };
        let on = on_frame(std::slice::from_ref(&ov), 0);
        assert_eq!(on.len(), 1);
        assert!(on_frame(std::slice::from_ref(&ov), 1).is_empty(), "not placed: not drawn");
        // On transparency, 20 × 20: the 8 × 8 picture centred on (10, 10).
        let rgba = draw_alpha(20, 20, &on);
        let px = |x: usize, y: usize| &rgba[(y * 20 + x) * 4..(y * 20 + x) * 4 + 4];
        assert_eq!(px(0, 0), &[0, 0, 0, 0], "outside: transparent");
        assert_eq!(px(7, 10), &[255, 0, 0, 255], "its left half: opaque red");
        assert_eq!(px(12, 10)[3], 128, "its right half: half transparent");
        // Into 8-bit 4:4:4 planes (limited BT.709 black): red over black, half red on the right.
        let (w, h) = (20, 20);
        let mut planes = Vec::new();
        planes.extend(std::iter::repeat_n(16u8, w * h));
        planes.extend(std::iter::repeat_n(128u8, 2 * w * h));
        let conv = ToYuv::new(Some("bt709"), 1080, false, false);
        composite::<1>(&mut planes, w, h, &on, &conv);
        let y = |x: usize, yy: usize| planes[yy * w + x];
        assert_eq!(y(0, 0), 16, "uncovered: exactly as it was");
        assert_eq!(y(7, 10), 63, "red's Y");
        assert!((y(12, 10) as i32 - 40).abs() <= 1, "half way from black to red: {}", y(12, 10));
        assert_eq!(planes[2 * w * h + 10 * w + 7], 240, "red's Cr");
        // 16-bit the same, ×256.
        let mut deep: Vec<u8> = std::iter::repeat_n((16u16 << 8).to_le_bytes(), w * h).flatten().collect();
        deep.extend(std::iter::repeat_n((128u16 << 8).to_le_bytes(), 2 * w * h).flatten());
        composite::<2>(&mut deep, w, h, &on, &ToYuv::new(Some("bt709"), 1080, false, true));
        let at = (10 * w + 7) * 2;
        let want = (16.0 + 219.0 * 0.2126) * 256.0;
        assert!((u16::from_le_bytes([deep[at], deep[at + 1]]) as f64 - want).abs() < 1.0, "red's 16-bit Y");
    }
}
