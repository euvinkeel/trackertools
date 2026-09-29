//! Spike S1 (ROADMAP M1): frame-exact random access through the ffmpeg
//! subprocess path, checked against an independent full-decode reference.
//!
//! cargo run --release -p tt_media --example s1_decode -- <video> <ref.framemd5>
//!     [--hw cuda|d3d11va] [--random N] [--seed S] [--barcode] [--seq N]
//!
//! The reference comes from one sequential decode:
//!   ffmpeg -i <video> -map 0:v:0 -fps_mode passthrough -pix_fmt nv12 -f framemd5 ref.framemd5
//! `--barcode` additionally reads the fixture frame-index barcode (cargo xtask fixtures).

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use md5::{Digest, Md5};
use tt_media::{DecodeOptions, FrameStream, VideoIndex};

struct Args {
    video: String,
    reference: String,
    hw: Option<String>,
    random: usize,
    seed: u64,
    barcode: bool,
    seq: usize,
}

fn parse_args() -> Result<Args> {
    let mut it = std::env::args().skip(1);
    let video = it.next().context("usage: s1_decode <video> <ref.framemd5> [options]")?;
    let reference = it.next().context("missing reference framemd5")?;
    let mut a = Args { video, reference, hw: None, random: 40, seed: 7, barcode: false, seq: 600 };
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--hw" => a.hw = it.next(),
            "--random" => a.random = it.next().context("--random N")?.parse()?,
            "--seed" => a.seed = it.next().context("--seed S")?.parse()?,
            "--seq" => a.seq = it.next().context("--seq N")?.parse()?,
            "--barcode" => a.barcode = true,
            other => bail!("unknown flag {other}"),
        }
    }
    Ok(a)
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let opts = DecodeOptions { hwaccel: args.hw.clone(), ..Default::default() };

    let t = Instant::now();
    let idx = VideoIndex::open(&args.video)?;
    let index_time = t.elapsed();
    let gops = idx.gop_lengths();
    let mut sorted_gops = gops.clone();
    sorted_gops.sort_unstable();
    println!("## {}", idx.path.file_name().unwrap().to_string_lossy());
    println!(
        "index: {:?} | {}x{} {} | {} frames, grid {} @ {}/{} fps | {} keyframes (GOP median {}, max {}) | reordering: {}",
        index_time,
        idx.width,
        idx.height,
        idx.codec,
        idx.frames.len(),
        idx.frame_count(),
        idx.fps.num,
        idx.fps.den,
        idx.keyframe_count(),
        sorted_gops[sorted_gops.len() / 2],
        sorted_gops.last().unwrap(),
        idx.has_reordering(),
    );

    let reference = parse_framemd5(&args.reference)?;
    if reference.len() != idx.frames.len() {
        bail!("reference has {} frames, index has {}", reference.len(), idx.frames.len());
    }

    // Targets: ends, around keyframes (where exactness usually breaks), random.
    let n = idx.frames.len();
    let mut targets = vec![0, 1, n - 2, n - 1];
    let keys: Vec<usize> = idx.frames.iter().enumerate().filter(|(_, f)| f.keyframe).map(|(i, _)| i).collect();
    let mut rng = XorShift(args.seed | 1);
    let mut key_picks: Vec<usize> = keys.iter().copied().take(3).collect();
    for _ in 0..4 {
        key_picks.push(keys[rng.below(keys.len())]);
    }
    for k in key_picks {
        targets.extend([k.saturating_sub(1), k, (k + 1).min(n - 1)]);
    }
    for _ in 0..args.random {
        targets.push(rng.below(n));
    }
    targets.sort_unstable();
    targets.dedup();

    let mut buf = Vec::new();
    let mut mismatches = Vec::new();
    let mut barcode_errors = Vec::new();
    let mut samples: Vec<(usize, Duration)> = Vec::new(); // (decode distance from keyframe, latency)
    for &p in &targets {
        let t = Instant::now();
        let mut stream = FrameStream::start(&idx, p, &opts)?;
        let got = stream.read(&mut buf)?.context("no frame")?;
        let latency = t.elapsed();
        assert_eq!(got, p);
        let distance = p - keys.iter().rev().find(|&&k| k <= p).copied().unwrap_or(0);
        samples.push((distance, latency));
        if hex(&Md5::digest(&buf)) != reference[p] {
            mismatches.push(p);
        }
        if args.barcode {
            let code = read_barcode(&buf, idx.width as usize, idx.height as usize);
            if code as i64 != idx.grid_of[p] {
                barcode_errors.push((p, idx.grid_of[p], code));
            }
        }
    }
    println!(
        "random access: {} targets, {} hash mismatches{}",
        targets.len(),
        mismatches.len(),
        if args.barcode { format!(", {} barcode mismatches", barcode_errors.len()) } else { String::new() }
    );
    if !mismatches.is_empty() {
        println!("  mismatched frames: {:?}", &mismatches[..mismatches.len().min(20)]);
    }
    if !barcode_errors.is_empty() {
        println!("  barcode (frame, expected, read): {:?}", &barcode_errors[..barcode_errors.len().min(20)]);
    }
    report_latency(&samples);

    // Grid check on VFR sources: every grid slot must show the right frame.
    if args.barcode && idx.frame_count() as usize != idx.frames.len() {
        let mut stream = FrameStream::start(&idx, 0, &opts)?;
        let mut codes = Vec::with_capacity(n);
        while stream.read(&mut buf)?.is_some() {
            codes.push(read_barcode(&buf, idx.width as usize, idx.height as usize));
        }
        let bad = (0..idx.frame_count())
            .filter(|&g| {
                let shown = codes[idx.presented_at(g)] as i64;
                let expected = (0..=g).rev().find(|h| idx.grid_of.contains(h)).unwrap();
                shown != expected
            })
            .count();
        println!("grid: {} slots, {} frames, {} slots showing the wrong frame", idx.frame_count(), n, bad);
    }

    // Sequential throughput, verifying every frame.
    let count = args.seq.min(n);
    let start = if n > count { rng.below(n - count) } else { 0 };
    let t = Instant::now();
    let mut stream = FrameStream::start(&idx, start, &opts)?;
    let mut seq_bad = 0;
    for _ in 0..count {
        let p = stream.read(&mut buf)?.context("stream ended")?;
        if hex(&Md5::digest(&buf)) != reference[p] {
            seq_bad += 1;
        }
    }
    let secs = t.elapsed().as_secs_f64();
    println!(
        "sequential (hash-checked): {count} frames from {start} in {secs:.2}s = {:.0} fps, {seq_bad} mismatches",
        count as f64 / secs,
    );

    // Raw decode + pipe throughput (no hashing), what playback and trackers get.
    let t = Instant::now();
    let mut stream = FrameStream::start(&idx, start, &opts)?;
    for _ in 0..count {
        stream.read(&mut buf)?.context("stream ended")?;
    }
    let secs = t.elapsed().as_secs_f64();
    println!(
        "sequential (raw): {:.0} fps ({:.0} MB/s through the pipe)",
        count as f64 / secs,
        count as f64 * stream.frame_bytes() as f64 / secs / 1e6
    );
    Ok(())
}

fn report_latency(samples: &[(usize, Duration)]) {
    let mut all: Vec<f64> = samples.iter().map(|(_, d)| d.as_secs_f64() * 1e3).collect();
    all.sort_by(f64::total_cmp);
    let q = |v: &[f64], f: f64| v[((v.len() - 1) as f64 * f).round() as usize];
    println!("seek latency ms: min {:.0} | median {:.0} | p95 {:.0} | max {:.0}", all[0], q(&all, 0.5), q(&all, 0.95), all[all.len() - 1]);
    for (lo, hi) in [(0, 0), (1, 30), (31, 120), (121, 250), (251, usize::MAX)] {
        let mut v: Vec<f64> =
            samples.iter().filter(|(d, _)| *d >= lo && *d <= hi).map(|(_, t)| t.as_secs_f64() * 1e3).collect();
        if v.is_empty() {
            continue;
        }
        v.sort_by(f64::total_cmp);
        let label = if hi == usize::MAX { format!("{lo}+") } else { format!("{lo}-{hi}") };
        println!("  {label:>8} frames after keyframe: n={:<3} median {:.0} ms, max {:.0} ms", v.len(), q(&v, 0.5), v[v.len() - 1]);
    }
}

/// md5 per frame, in file order (== presentation order with -fps_mode passthrough).
fn parse_framemd5(path: &str) -> Result<Vec<String>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    Ok(text
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .filter_map(|l| l.rsplit(',').next().map(|s| s.trim().to_string()))
        .collect())
}

/// The fixture barcode: 16 cells of 24 px along y = H-40, x from 16; white = bit set.
fn read_barcode(nv12: &[u8], width: usize, height: usize) -> u32 {
    let y = height - 40 + 12;
    (0..16).filter(|b| nv12[y * width + 16 + b * 24 + 12] > 128).fold(0, |v, b| v | (1 << b))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

struct XorShift(u64);

impl XorShift {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}
