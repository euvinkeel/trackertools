//! Frame index (DESIGN §13 step 1): every frame's timestamps, keyframe flag and
//! byte range, read from the container's sample table with `re_mp4` — the media
//! data itself is skipped, so indexing a multi-GB recording reads only its moov box.
//!
//! Frames are kept in *presentation* order. The frame grid follows the v1 rule:
//! grid frame `g` shows the frame whose timestamp rounds to `g / fps`; grid slots
//! without a frame of their own (VFR gaps) repeat the previous frame.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tt_core::time::{FrameIndex, Rational};

#[derive(Clone, Copy, Debug)]
pub struct Frame {
    /// Presentation timestamp (container time units, first frame = 0).
    pub cts: i64,
    /// Decode timestamp (container time units).
    pub dts: i64,
    pub keyframe: bool,
    /// Byte range of the sample in the file.
    pub offset: u64,
    pub size: u32,
    /// Position of this frame in decode order.
    pub decode_order: u32,
}

#[derive(Debug)]
pub struct VideoIndex {
    pub path: PathBuf,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub timescale: u64,
    pub fps: Rational,
    /// Frames in presentation order.
    pub frames: Vec<Frame>,
    /// Grid frame → index into `frames`.
    pub grid: Vec<u32>,
    /// Grid position of each entry of `frames`.
    pub grid_of: Vec<FrameIndex>,
}

impl VideoIndex {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path).with_context(|| format!("opening {}", path.display()))?;
        let size = file.metadata()?.len();
        let mp4 = re_mp4::Mp4::read(BufReader::new(file), size)
            .with_context(|| format!("reading the MP4 sample table of {}", path.display()))?;
        let track = mp4
            .tracks()
            .values()
            .find(|t| t.kind == Some(re_mp4::TrackKind::Video))
            .context("no video track")?;
        if track.samples.is_empty() {
            bail!("video track has no samples");
        }
        let codec = track.codec_string(&mp4).unwrap_or_else(|| "unknown".into());

        let mut frames: Vec<Frame> = track
            .samples
            .iter()
            .enumerate()
            .map(|(i, s)| Frame {
                cts: s.composition_timestamp,
                dts: s.decode_timestamp,
                keyframe: s.is_sync,
                offset: s.offset,
                size: s.size as u32,
                decode_order: i as u32,
            })
            .collect();
        frames.sort_by_key(|f| (f.cts, f.decode_order));

        let timescale = track.timescale;
        let fps = estimate_fps(&frames, timescale);
        let end_cts = {
            let last = track.samples.iter().max_by_key(|s| s.composition_timestamp).unwrap();
            last.composition_timestamp + last.duration as i64
        };
        let (grid, grid_of) = build_grid(&frames, timescale, fps, end_cts);

        Ok(Self { path, codec, width: track.width as u32, height: track.height as u32, timescale, fps, frames, grid, grid_of })
    }

    /// Frames on the constant grid (what the timeline and transport count).
    pub fn frame_count(&self) -> FrameIndex {
        self.grid.len() as FrameIndex
    }

    /// Index into `frames` shown at grid frame `g` (clamped).
    pub fn presented_at(&self, g: FrameIndex) -> usize {
        self.grid[g.clamp(0, self.frame_count() - 1) as usize] as usize
    }

    /// Presentation time of `frames[p]` in seconds from the first frame.
    pub fn seconds(&self, p: usize) -> f64 {
        self.frames[p].cts as f64 / self.timescale as f64
    }

    /// Whether any frame is presented before a frame decoded ahead of it.
    pub fn has_reordering(&self) -> bool {
        self.frames.windows(2).any(|w| w[1].decode_order < w[0].decode_order)
    }

    pub fn keyframe_count(&self) -> usize {
        self.frames.iter().filter(|f| f.keyframe).count()
    }

    /// Keyframe intervals in presentation order (for stats and seek-cost estimates).
    pub fn gop_lengths(&self) -> Vec<usize> {
        let keys: Vec<usize> = self.frames.iter().enumerate().filter(|(_, f)| f.keyframe).map(|(i, _)| i).collect();
        let mut out: Vec<usize> = keys.windows(2).map(|w| w[1] - w[0]).collect();
        if let Some(last) = keys.last() {
            out.push(self.frames.len() - last);
        }
        out
    }

    /// Where decoding must start to produce presented frame `p`, as an index
    /// into `frames`: `None` when a seek straight to `p` is safe, or `Some(s)`
    /// when `p` is a *leading* frame — decoded after a keyframe that is
    /// presented after it. In open GOPs such frames reference the previous GOP,
    /// so a seek that lands on that keyframe cannot produce them; decoding must
    /// start at the keyframe before (`s`).
    pub fn leading_frame_start(&self, p: usize) -> Option<usize> {
        let target = self.frames[p].decode_order;
        // The GOP `p` belongs to in decode order: the last keyframe decoded at or before it.
        let (gop_key, _) = self
            .frames
            .iter()
            .enumerate()
            .filter(|(_, f)| f.keyframe && f.decode_order <= target)
            .max_by_key(|(_, f)| f.decode_order)?;
        if self.frames[gop_key].cts <= self.frames[p].cts {
            return None;
        }
        // Start at the keyframe presented before that GOP's keyframe.
        Some(self.frames[..gop_key].iter().rposition(|f| f.keyframe).unwrap_or(0))
    }

    /// First frame (index into `frames`) of the decode run that produces `p`:
    /// the keyframe at or before `p`, or the earlier keyframe for open-GOP
    /// leading frames. Starting a stream here and reading up to `p` caches the
    /// whole group — what backward stepping needs.
    pub fn group_start(&self, p: usize) -> usize {
        self.leading_frame_start(p)
            .unwrap_or_else(|| self.frames[..=p].iter().rposition(|f| f.keyframe).unwrap_or(0))
    }

    /// Bytes of one NV12 frame at the coded size.
    pub fn nv12_frame_bytes(&self) -> usize {
        nv12_bytes(self.width, self.height)
    }
}

pub fn nv12_bytes(width: u32, height: u32) -> usize {
    let (w, h) = (width as usize, height as usize);
    w * h + 2 * w.div_ceil(2) * h.div_ceil(2)
}

/// The frame rate as an exact rational: the timescale over the most common
/// frame duration (robust to VFR gaps and a short final frame).
fn estimate_fps(frames: &[Frame], timescale: u64) -> Rational {
    let mut deltas: Vec<i64> = frames.windows(2).map(|w| w[1].cts - w[0].cts).filter(|d| *d > 0).collect();
    if deltas.is_empty() {
        return Rational::default();
    }
    deltas.sort_unstable();
    let mut best = (deltas[0], 0usize);
    let mut run = (deltas[0], 0usize);
    for d in deltas {
        if d == run.0 {
            run.1 += 1;
        } else {
            run = (d, 1);
        }
        if run.1 > best.1 {
            best = run;
        }
    }
    Rational::new(timescale as i64, best.0)
}

fn build_grid(frames: &[Frame], timescale: u64, fps: Rational, end_cts: i64) -> (Vec<u32>, Vec<FrameIndex>) {
    // g = round(cts / timescale * fps), in exact integer arithmetic.
    let to_grid = |cts: i64| -> FrameIndex {
        let num = cts as i128 * fps.num as i128;
        let den = timescale as i128 * fps.den as i128;
        ((2 * num + den).div_euclid(2 * den)) as FrameIndex
    };
    let grid_of: Vec<FrameIndex> = frames.iter().map(|f| to_grid(f.cts)).collect();
    let count = to_grid(end_cts).max(grid_of.last().copied().unwrap_or(0) + 1).max(1) as usize;
    let mut grid = vec![0u32; count];
    let mut p = 0usize;
    for (g, slot) in grid.iter_mut().enumerate() {
        while p + 1 < frames.len() && grid_of[p + 1] <= g as FrameIndex {
            p += 1;
        }
        *slot = p as u32;
    }
    (grid, grid_of)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(cts: i64) -> Frame {
        Frame { cts, dts: cts, keyframe: false, offset: 0, size: 0, decode_order: 0 }
    }

    #[test]
    fn fps_from_most_common_delta() {
        let fr: Vec<Frame> = [0, 256, 512, 1024, 1280].into_iter().map(frame).collect();
        assert_eq!(estimate_fps(&fr, 15360), Rational::new(60, 1));
        let ntsc: Vec<Frame> = (0..5).map(|i| frame(i * 1001)).collect();
        assert_eq!(estimate_fps(&ntsc, 60000), Rational::new(60000, 1001));
    }

    #[test]
    fn grid_repeats_previous_frame_across_gaps() {
        // Grid slot 2 has no frame (dropped): it shows frame 1 again.
        let fr: Vec<Frame> = [0, 256, 768, 1024].into_iter().map(frame).collect();
        let (grid, grid_of) = build_grid(&fr, 15360, Rational::new(60, 1), 1280);
        assert_eq!(grid_of, vec![0, 1, 3, 4]);
        assert_eq!(grid, vec![0, 1, 1, 2, 3]);
    }
}
