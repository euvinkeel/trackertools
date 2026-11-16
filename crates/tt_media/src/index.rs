//! Frame index (DESIGN §13 step 1): every frame's timestamps, keyframe flag and
//! byte range. MP4 and QuickTime files (ProRes included) are read from the
//! container's sample table with `re_mp4` — the media data itself is skipped,
//! so indexing a multi-GB recording reads only its moov box. Anything else
//! FFmpeg reads (MKV, WebM, AVI, …) is listed packet by packet with ffprobe,
//! which reads the whole file once.
//!
//! Frames are kept in *presentation* order. The frame grid follows the v1 rule:
//! grid frame `g` shows the frame whose timestamp rounds to `g / fps`; grid slots
//! without a frame of their own (VFR gaps) repeat the previous frame.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
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
    /// Index the video at `path`: from its sample table if it is an MP4 or
    /// QuickTime file, else from ffprobe's packet list.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        // A missing or locked file says so, rather than two readers' complaints.
        File::open(&path).with_context(|| format!("opening {}", path.display()))?;
        match Self::open_mp4(&path) {
            Ok(index) => Ok(index),
            Err(mp4) => Self::open_probed(&path).map_err(|probe| anyhow!("{probe:#} (read as MP4: {mp4:#})")),
        }
    }

    /// From an MP4 or QuickTime sample table.
    fn open_mp4(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let size = file.metadata()?.len();
        let mp4 = re_mp4::Mp4::read(BufReader::new(file), size)
            .with_context(|| format!("reading the MP4 sample table of {}", path.display()))?;
        // `re_mp4` names only the codecs it parses; a QuickTime track of another
        // codec (ProRes, DNxHD, …) is still marked as video by its handler.
        let track = mp4
            .tracks()
            .values()
            .find(|t| t.kind == Some(re_mp4::TrackKind::Video) || t.trak(&mp4).mdia.hdlr.handler_type.value == *b"vide")
            .context("no video track")?;
        if track.samples.is_empty() {
            bail!("video track has no samples");
        }
        if track.width == 0 || track.height == 0 {
            bail!("video track has no picture size");
        }
        let codec = track.codec_string(&mp4).unwrap_or_else(|| match &track.trak(&mp4).mdia.minf.stbl.stsd.contents {
            re_mp4::StsdBoxContent::Unknown(fourcc) => quicktime_codec(&fourcc.to_string()),
            _ => "unknown".into(),
        });

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

        Ok(Self {
            path: path.to_path_buf(),
            codec,
            width: track.width as u32,
            height: track.height as u32,
            timescale,
            fps,
            frames,
            grid,
            grid_of,
        })
    }

    /// From ffprobe's packet list: any container FFmpeg reads (MKV, WebM, AVI, …).
    fn open_probed(path: &Path) -> Result<Self> {
        let ffprobe = crate::ffmpeg::tool("ffprobe", "FFPROBE");
        let stream = probe(
            &ffprobe,
            &["-show_entries", "stream=codec_name,profile,width,height,time_base,r_frame_rate,avg_frame_rate", "-of", "default=nw=1"],
            path,
        )?;
        let field = |key: &str| stream.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix('=')).unwrap_or("");
        let (width, height) = (field("width").parse::<u32>().unwrap_or(0), field("height").parse::<u32>().unwrap_or(0));
        if width == 0 || height == 0 {
            bail!("no video FFmpeg can read in {}", path.display());
        }
        let codec = match field("profile") {
            "" | "unknown" => field("codec_name").to_string(),
            profile => format!("{} {profile}", field("codec_name")),
        };
        // Timestamps count time_base = num/den seconds; in units of 1/den they are `num` times larger.
        let (scale, timescale) = ratio(field("time_base")).filter(|&(n, d)| n > 0 && d > 0).context("no time base")?;
        let packets = probe(&ffprobe, &["-show_entries", "packet=pts,dts,duration,pos,size,flags", "-of", "compact=p=0"], path)?;
        let (mut frames, end_cts) = probed_frames(&packets, scale);
        if frames.is_empty() {
            bail!("the video has no frames");
        }
        frames.sort_by_key(|f| (f.cts, f.decode_order));
        // Coarse container clocks (MKV counts milliseconds) blur the frame
        // durations, so the most common one isn't the rate; and with dropped
        // frames ffprobe's rate is the average. The rate is the one whose grid
        // every frame sits on: ffprobe's if it fits, else a standard one,
        // else (truly variable timing) ffprobe's anyway.
        let rate = |key: &str| ratio(field(key)).filter(|&(n, d)| n > 0 && d > 0 && n <= 1000 * d).map(|(n, d)| standard_rate(Rational::new(n, d)));
        let timescale = timescale as u64;
        let stated = rate("r_frame_rate").or_else(|| rate("avg_frame_rate"));
        let fps = stated
            .filter(|&r| grid_fit(&frames, timescale, r).is_some())
            .or_else(|| fitting_standard_rate(&frames, timescale))
            .or(stated)
            .unwrap_or_else(|| estimate_fps(&frames, timescale));
        let (grid, grid_of) = build_grid(&frames, timescale, fps, end_cts);
        Ok(Self { path: path.to_path_buf(), codec, width, height, timescale, fps, frames, grid, grid_of })
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

/// A QuickTime codec `re_mp4` doesn't parse (FFmpeg decodes it), by name
/// where it is one an editor exports.
fn quicktime_codec(fourcc: &str) -> String {
    let name = match fourcc {
        "apco" => "ProRes 422 Proxy",
        "apcs" => "ProRes 422 LT",
        "apcn" => "ProRes 422",
        "apch" => "ProRes 422 HQ",
        "ap4h" => "ProRes 4444",
        "ap4x" => "ProRes 4444 XQ",
        "AVdn" => "DNxHD",
        "AVdh" => "DNxHR",
        _ => return fourcc.trim().to_string(),
    };
    format!("{name} ({fourcc})")
}

/// `rate`, or the standard rate it is a rounded guess of: ffprobe guesses
/// 59.94 fps in an MKV (millisecond times) as 19001/317, not 60000/1001.
fn standard_rate(rate: Rational) -> Rational {
    let r = rate.as_f64();
    // 0.02%: well inside the 0.1% between 60 and 59.94.
    STANDARD.iter().map(|&(n, d)| Rational::new(n, d)).find(|s| (s.as_f64() - r).abs() <= 2e-4 * r).unwrap_or(rate)
}

/// How far the frames (sorted, presentation order) sit off the grid of
/// `fps` at worst, in frames — `None` if they don't fit it: off by more than
/// the clock's rounding allows, or two frames on one grid slot.
fn grid_fit(frames: &[Frame], timescale: u64, fps: Rational) -> Option<f64> {
    let per_tick = fps.as_f64() / timescale as f64;
    // A clock tick of rounding (or truncation), and a little.
    let allowed = 0.02 + per_tick;
    let mut worst = 0.0f64;
    let mut last = None;
    for f in frames {
        let at = f.cts as f64 * per_tick;
        let slot = at.round();
        worst = worst.max((at - slot).abs());
        if worst > allowed || last == Some(slot) {
            return None;
        }
        last = Some(slot);
    }
    Some(worst)
}

/// The slowest standard rate the frames fit (of 59.94 and 60, the closer).
fn fitting_standard_rate(frames: &[Frame], timescale: u64) -> Option<Rational> {
    STANDARD
        .iter()
        .map(|&(n, d)| Rational::new(n, d))
        .filter_map(|r| Some((r.as_f64().round() as i64, grid_fit(frames, timescale, r)?, r)))
        .min_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)))
        .map(|(_, _, r)| r)
}

/// Frame rates cameras, editors and screen recorders use.
const STANDARD: [(i64, i64); 16] = [
    (24000, 1001),
    (24, 1),
    (25, 1),
    (30000, 1001),
    (30, 1),
    (48, 1),
    (50, 1),
    (60000, 1001),
    (60, 1),
    (72, 1),
    (90, 1),
    (100, 1),
    (120000, 1001),
    (120, 1),
    (144, 1),
    (240, 1),
];

/// `a/b` as two integers.
fn ratio(text: &str) -> Option<(i64, i64)> {
    let (a, b) = text.trim().split_once('/')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

/// Run ffprobe on the first video stream of `path`; its standard output.
fn probe(ffprobe: &Path, args: &[&str], path: &Path) -> Result<String> {
    let mut cmd = std::process::Command::new(ffprobe);
    cmd.args(["-v", "error", "-select_streams", "v:0"]).args(args).arg(path);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = cmd.output().with_context(|| format!("starting {}", ffprobe.display()))?;
    if !out.status.success() {
        bail!("FFmpeg can't read {}: {}", path.display(), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// ffprobe's packet lines (`pts=…|dts=…|duration=…|size=…|pos=…|flags=K_`,
/// in decode order) → frames, shifted so the first presented one is at 0,
/// and the end of the last one. Times are multiplied by `scale` (the time
/// base's numerator). Packets without a time, and discarded ones, are left out.
fn probed_frames(packets: &str, scale: i64) -> (Vec<Frame>, i64) {
    let mut frames = Vec::new();
    let mut end = i64::MIN;
    for line in packets.lines() {
        let field = |key: &str| line.split('|').find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='));
        let number = |key: &str| field(key).and_then(|v| v.trim().parse::<i64>().ok());
        let flags = field("flags").unwrap_or("");
        if flags.contains('D') {
            continue;
        }
        let Some(cts) = number("pts").or_else(|| number("dts")) else { continue };
        let dts = number("dts").unwrap_or(cts);
        end = end.max((cts + number("duration").unwrap_or(0)) * scale);
        frames.push(Frame {
            cts: cts * scale,
            dts: dts * scale,
            keyframe: flags.contains('K'),
            offset: number("pos").unwrap_or(0).max(0) as u64,
            size: number("size").unwrap_or(0).clamp(0, u32::MAX as i64) as u32,
            decode_order: frames.len() as u32,
        });
    }
    // As `re_mp4` does for MP4: shift so the first presented frame is at 0.
    let first = frames.iter().map(|f| f.cts).min().unwrap_or(0);
    for f in &mut frames {
        f.cts -= first;
        f.dts -= first;
    }
    (frames, end - first)
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

    #[test]
    fn probed_packets_become_frames_from_zero() {
        // An MKV with B-frames: millisecond times, no decode times, a keyframe
        // first, and a discarded packet.
        let text = "pts=100|dts=N/A|duration=16|size=600|pos=40|flags=K_
pts=167|dts=N/A|duration=16|size=300|pos=640|flags=__
pts=133|dts=N/A|duration=16|size=200|pos=940|flags=__
pts=117|dts=N/A|duration=17|size=100|pos=1140|flags=__
pts=150|dts=N/A|duration=17|size=100|pos=1240|flags=_D
";
        let (mut frames, end) = probed_frames(text, 1);
        assert_eq!(frames.iter().map(|f| f.cts).collect::<Vec<_>>(), vec![0, 67, 33, 17]);
        assert_eq!(frames.iter().map(|f| f.decode_order).collect::<Vec<_>>(), vec![0, 1, 2, 3]);
        assert!(frames[0].keyframe && !frames[1].keyframe);
        assert_eq!(end, 83);
        // At 60 fps the millisecond times land on grid frames 0, 1, 2 and 4; slot 3 repeats.
        frames.sort_by_key(|f| (f.cts, f.decode_order));
        let (grid, grid_of) = build_grid(&frames, 1000, Rational::new(60, 1), end);
        assert_eq!(grid_of, vec![0, 1, 2, 4]);
        assert_eq!(grid, vec![0, 1, 2, 2, 3]);
    }

    #[test]
    fn guessed_rates_snap_to_standard_ones() {
        assert_eq!(standard_rate(Rational::new(19001, 317)), Rational::new(60000, 1001));
        assert_eq!(standard_rate(Rational::new(60, 1)), Rational::new(60, 1));
        assert_eq!(standard_rate(Rational::new(37, 1)), Rational::new(37, 1));
    }

    #[test]
    fn millisecond_times_with_drops_fit_their_real_rate() {
        // 60 fps in milliseconds with every 7th frame dropped: the average (~51.4 fps) doesn't fit.
        let fr: Vec<Frame> = (0..600i64).filter(|n| n % 7 != 3).map(|n| frame((n * 1000 + 30) / 60)).collect();
        assert!(grid_fit(&fr, 1000, Rational::new(14519, 282)).is_none());
        assert_eq!(fitting_standard_rate(&fr, 1000), Some(Rational::new(60, 1)));
        // 59.94 fps over 20 s: 60 drifts off by a frame and a bit.
        let ntsc: Vec<Frame> = (0..1200i64).map(|n| frame((n * 1001 * 1000 + 30000) / 60000)).collect();
        assert_eq!(fitting_standard_rate(&ntsc, 1000), Some(Rational::new(60000, 1001)));
        // Truly variable timing fits nothing.
        let jitter: Vec<Frame> = (0..300i64).scan(0, |t, n| Some(std::mem::replace(t, *t + 10 + (n * 7919) % 31))).map(frame).collect();
        assert_eq!(fitting_standard_rate(&jitter, 1000), None);
    }

    #[test]
    fn prores_has_a_name() {
        assert_eq!(quicktime_codec("apcs"), "ProRes 422 LT (apcs)");
        assert_eq!(quicktime_codec("xyz1"), "xyz1");
    }
}
