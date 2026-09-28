//! Build a scrub proxy and verify it is aligned frame for frame with its source.
//!
//! cargo run --release -p tt_media --example proxy_check -- <video> [--barcode] [--samples N]
//!
//! Alignment is checked by content: proxy frame p must match the downscaled
//! source frame p better than frames p±1 (static stretches, where neighbours
//! are identical, count as ambiguous). With `--barcode` the fixture's frame
//! index barcode is read from the proxy directly.

use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::Instant;

use anyhow::{Context, Result};
use tt_media::{DecodeOptions, FrameStream, VideoIndex, proxy};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let video = args.next().context("usage: proxy_check <video> [--barcode] [--samples N]")?;
    let rest: Vec<String> = args.collect();
    let barcode = rest.iter().any(|a| a == "--barcode");
    let samples: usize = rest.iter().position(|a| a == "--samples").and_then(|i| rest.get(i + 1)).and_then(|s| s.parse().ok()).unwrap_or(24);

    let source = VideoIndex::open(&video)?;
    let out = std::env::temp_dir().join(format!("tt_proxy_check_{}.mp4", std::process::id()));
    let opts = DecodeOptions::default();
    let t = Instant::now();
    let proxy = proxy::build(&source, &out, &opts, Arc::new(AtomicU32::new(0)))?;
    let secs = t.elapsed().as_secs_f64();
    println!(
        "built {}x{} proxy of {} frames in {secs:.1} s ({:.0} fps); GOP median {}",
        proxy.width,
        proxy.height,
        proxy.frames.len(),
        proxy.frames.len() as f64 / secs,
        { let mut g = proxy.gop_lengths(); g.sort_unstable(); g[g.len() / 2] }
    );

    let n = source.frames.len();
    let mut rng = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        (rng % (n as u64 - 2)) as usize + 1
    };
    let (mut aligned, mut ambiguous, mut misaligned) = (0, 0, 0);
    let (mut pbuf, mut sbuf) = (Vec::new(), Vec::new());
    for _ in 0..samples {
        let p = next();
        let mut ps = FrameStream::start(&proxy, p, &opts)?;
        ps.read(&mut pbuf)?.context("proxy frame")?;
        if barcode {
            let s = proxy.height as f64 / source.height as f64;
            let y = (((source.height - 40 + 12) as f64) * s) as usize;
            let code = (0..16)
                .filter(|b| pbuf[y * proxy.width as usize + ((16 + b * 24 + 12) as f64 * s) as usize] > 128)
                .fold(0u32, |v, b| v | (1 << b));
            if code as i64 == source.grid_of[p] { aligned += 1 } else { misaligned += 1; println!("  frame {p}: barcode {code}") }
            continue;
        }
        // Content match against source frames p-1, p, p+1.
        let mut ss = FrameStream::start(&source, p - 1, &opts)?;
        let mut mad = [0f64; 3];
        for m in &mut mad {
            ss.read(&mut sbuf)?.context("source frame")?;
            *m = luma_mad(&pbuf, proxy.width as usize, proxy.height as usize, &sbuf, source.width as usize, source.height as usize);
        }
        // Differences within encoding noise are ties (static stretches of a
        // screen recording repeat frames exactly).
        let eps = 0.05 + 0.02 * mad[1];
        let neighbour = mad[0].min(mad[2]);
        let best = if mad[1] + eps < neighbour { "aligned" } else if neighbour + eps < mad[1] { "misaligned" } else { "ambiguous" };
        match best {
            "aligned" => aligned += 1,
            "ambiguous" => ambiguous += 1,
            _ => { misaligned += 1; println!("  frame {p}: MAD p-1/p/p+1 = {:.2}/{:.2}/{:.2}", mad[0], mad[1], mad[2]) }
        }
    }
    println!("alignment over {samples} random frames: {aligned} aligned, {ambiguous} ambiguous (static), {misaligned} MISALIGNED");
    let _ = std::fs::remove_file(&out);
    Ok(())
}

/// Mean absolute luma difference between a proxy frame and a source frame
/// sampled at the proxy's pixel centres (nearest), on a sparse grid.
fn luma_mad(p: &[u8], pw: usize, ph: usize, s: &[u8], sw: usize, sh: usize) -> f64 {
    let (mut sum, mut count) = (0f64, 0f64);
    for y in (0..ph).step_by(3) {
        let sy = ((y as f64 + 0.5) * sh as f64 / ph as f64) as usize;
        for x in (0..pw).step_by(3) {
            let sx = ((x as f64 + 0.5) * sw as f64 / pw as f64) as usize;
            sum += (p[y * pw + x] as f64 - s[sy * sw + sx] as f64).abs();
            count += 1.0;
        }
    }
    sum / count
}
