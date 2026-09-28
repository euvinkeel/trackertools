//! Developer tasks. `cargo xtask fixtures [--force]` generates the test clips
//! into `fixtures/` (gitignored) with ffmpeg's lavfi sources, so tests never
//! depend on files outside the repo (a v1 lesson).
//!
//! Every counter clip carries a burned-in frame number and a binary frame-index
//! barcode (16 cells along the bottom-left; cell b is white when bit b of the
//! source frame index is set), so decoding tests can read the true frame index
//! from pixels without OCR.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Serialize;

const W: u32 = 1920;
const H: u32 = 1080;
const FPS: u32 = 60;
const BARCODE: Barcode = Barcode { x: 16, y: H - 40, bits: 16, cell: 24 };

#[derive(Serialize, Clone, Copy)]
struct Barcode {
    x: u32,
    y: u32,
    bits: u32,
    cell: u32,
}

#[derive(Serialize)]
struct Clip {
    file: String,
    codec: &'static str,
    width: u32,
    height: u32,
    fps: String,
    /// Frames on the source grid (before any VFR drops).
    grid_frames: u32,
    /// Encoded frames (fewer than grid_frames for the VFR clip).
    encoded_frames: u32,
    gop: u32,
    bframes: u32,
    open_gop: bool,
    /// Grid indices with no frame of their own (VFR gaps; they repeat the previous frame).
    dropped: Vec<u32>,
    barcode: Option<Barcode>,
    notes: &'static str,
}

#[derive(Serialize)]
struct Manifest {
    generator: &'static str,
    ffmpeg: String,
    clips: Vec<Clip>,
    sprite: SpriteInfo,
}

#[derive(Serialize)]
struct SpriteInfo {
    file: String,
    truth: String,
    size: u32,
    /// x_topleft = floor(expr_x(t)), t = frame / fps; centre = topleft + size / 2 (continuous pixels).
    expr_x: &'static str,
    expr_y: &'static str,
}

const SPRITE_SIZE: u32 = 21;
const SPRITE_X: &str = "950+500*sin(0.9*t)+60*sin(5.3*t)";
const SPRITE_Y: &str = "530+300*sin(1.3*t+0.7)+40*sin(4.1*t)";

fn sprite_x(t: f64) -> f64 {
    950.0 + 500.0 * (0.9 * t).sin() + 60.0 * (5.3 * t).sin()
}

fn sprite_y(t: f64) -> f64 {
    530.0 + 300.0 * (1.3 * t + 0.7).sin() + 40.0 * (4.1 * t).sin()
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("fixtures") => fixtures(args.iter().any(|a| a == "--force")),
        _ => {
            eprintln!("usage: cargo xtask fixtures [--force]");
            Ok(())
        }
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn ffmpeg() -> String {
    std::env::var("FFMPEG").unwrap_or_else(|_| "ffmpeg".into())
}

fn fixtures(force: bool) -> Result<()> {
    let out = repo_root().join("fixtures");
    std::fs::create_dir_all(&out)?;
    let version = String::from_utf8_lossy(&Command::new(ffmpeg()).arg("-version").output().context("ffmpeg not found (set FFMPEG=path)")?.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    println!("using {version}");

    let x264 = |gop: u32, bframes: u32, open: bool| {
        vec![
            "-c:v".into(), "libx264".into(), "-preset".into(), "veryfast".into(), "-crf".into(), "20".into(),
            "-x264-params".into(),
            format!("keyint={gop}:min-keyint={gop}:scenecut=0:bframes={bframes}:open-gop={}", u8::from(open)),
        ]
    };
    let x265 = |gop: u32, bframes: u32| {
        vec![
            "-c:v".into(), "libx265".into(), "-preset".into(), "fast".into(), "-crf".into(), "24".into(),
            "-x265-params".into(), format!("keyint={gop}:min-keyint={gop}:scenecut=0:bframes={bframes}:log-level=error"),
            "-tag:v".into(), "hvc1".into(),
        ]
    };

    let frames = 600; // 10 s: several 250-frame GOPs
    let vfr_dropped: Vec<u32> = (0..frames).filter(|n| n % 7 == 3).collect();
    let mut clips = vec![
        clip("counter_h264_gop250.mp4", "h264", frames, 250, 3, false, vec![], "long GOP like screen recordings; closed GOP"),
        clip("counter_h264_gop1.mp4", "h264", frames, 1, 0, false, vec![], "all-intra reference"),
        clip("counter_h264_opengop.mp4", "h264", frames, 120, 3, true, vec![], "open GOP: leading B-frames reference the previous GOP"),
        clip("counter_h264_vfr.mp4", "h264", frames, 250, 3, false, vfr_dropped.clone(), "VFR: grid frames n%7==3 dropped, timestamps kept"),
        clip("counter_hevc_gop250.mp4", "hevc", frames, 250, 3, false, vec![], "HEVC long GOP"),
    ];

    for c in &clips {
        let path = out.join(&c.file);
        if path.exists() && !force {
            println!("skip  {} (exists)", c.file);
            continue;
        }
        let mut graph = counter_graph(c.grid_frames);
        let mut extra: Vec<String> = Vec::new();
        if !c.dropped.is_empty() {
            graph.push_str(",select='not(eq(mod(n\\,7)\\,3))'");
            extra.extend(["-fps_mode".into(), "passthrough".into()]);
        }
        graph.push_str("[out]");
        let enc = if c.codec == "hevc" { x265(c.gop, c.bframes) } else { x264(c.gop, c.bframes, c.open_gop) };
        encode(&graph, &enc, &extra, &path)?;
        check_packets(&path, c.encoded_frames)?;
    }

    // Moving sprite with analytic ground truth (M3 sketch accuracy tests).
    let sprite_frames = 1200;
    let sprite_file = "sprite_1080p60.mp4";
    let sprite_path = out.join(sprite_file);
    if !sprite_path.exists() || force {
        let d = sprite_frames as f64 / FPS as f64;
        let graph = format!(
            "testsrc2=s={W}x{H}:r={FPS}:d={d},boxblur=6:1,format=yuv420p[bg];\
             color=c=black:s={s}x{s}:r={FPS}:d={d}[b];color=c=white:s={i}x{i}:r={FPS}:d={d}[w];[b][w]overlay=2:2[spr];\
             [bg][spr]overlay=x='floor({SPRITE_X})':y='floor({SPRITE_Y})':eval=frame[sp];\
             {code}[code];[sp][code]overlay=x={bx}:y={by}:shortest=1[out]",
            s = SPRITE_SIZE,
            i = SPRITE_SIZE - 4,
            code = barcode_source(sprite_frames),
            bx = BARCODE.x,
            by = BARCODE.y,
        );
        encode(&graph, &x264(250, 3, false), &[], &sprite_path)?;
        check_packets(&sprite_path, sprite_frames)?;
    } else {
        println!("skip  {sprite_file} (exists)");
    }
    let half = SPRITE_SIZE as f64 / 2.0;
    let truth: Vec<[f64; 2]> = (0..sprite_frames)
        .map(|f| {
            let t = f as f64 / FPS as f64;
            [sprite_x(t).floor() + half, sprite_y(t).floor() + half]
        })
        .collect();
    std::fs::write(out.join("sprite_truth.json"), serde_json::to_string(&serde_json::json!({ "centers": truth }))?)?;

    clips.push(clip(sprite_file, "h264", sprite_frames, 250, 3, false, vec![], "sprite over blurred testsrc2; see sprite_truth.json"));
    let manifest = Manifest {
        generator: "cargo xtask fixtures",
        ffmpeg: version,
        clips,
        sprite: SpriteInfo {
            file: sprite_file.into(),
            truth: "sprite_truth.json".into(),
            size: SPRITE_SIZE,
            expr_x: SPRITE_X,
            expr_y: SPRITE_Y,
        },
    };
    std::fs::write(out.join("manifest.json"), serde_json::to_string_pretty(&manifest)?)?;
    println!("wrote {}", out.join("manifest.json").display());
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn clip(file: &str, codec: &'static str, grid: u32, gop: u32, bframes: u32, open_gop: bool, dropped: Vec<u32>, notes: &'static str) -> Clip {
    Clip {
        file: file.into(),
        codec,
        width: W,
        height: H,
        fps: format!("{FPS}/1"),
        grid_frames: grid,
        encoded_frames: grid - dropped.len() as u32,
        gop,
        bframes,
        open_gop,
        dropped,
        barcode: Some(BARCODE),
        notes,
    }
}

/// A source whose luma cells encode the frame index N in binary (limited range 16/235).
fn barcode_source(frames: u32) -> String {
    let d = frames as f64 / FPS as f64;
    format!(
        "color=c=black:s={w}x{h}:r={FPS}:d={d},format=yuv420p,geq=lum='if(bitand(N,pow(2,floor(X/{c}))),235,16)':cb=128:cr=128",
        w = BARCODE.bits * BARCODE.cell,
        h = BARCODE.cell,
        c = BARCODE.cell,
    )
}

/// testsrc2 + barcode + burned-in frame number, without the trailing `[out]`.
fn counter_graph(frames: u32) -> String {
    let d = frames as f64 / FPS as f64;
    format!(
        "testsrc2=s={W}x{H}:r={FPS}:d={d},format=yuv420p[bg];{code}[code];\
         [bg][code]overlay=x={bx}:y={by}:shortest=1,\
         drawtext=fontfile='C\\:/Windows/Fonts/consola.ttf':text='%{{frame_num}}':start_number=0:x=40:y=40:fontsize=96:fontcolor=white:box=1:boxcolor=black@0.6:boxborderw=12",
        code = barcode_source(frames),
        bx = BARCODE.x,
        by = BARCODE.y,
    )
}

fn encode(graph: &str, codec_args: &[String], extra: &[String], path: &Path) -> Result<()> {
    println!("make  {}", path.file_name().unwrap().to_string_lossy());
    let status = Command::new(ffmpeg())
        .args(["-hide_banner", "-loglevel", "error", "-y", "-filter_complex", graph, "-map", "[out]"])
        .args(codec_args)
        .args(["-pix_fmt", "yuv420p", "-movflags", "+faststart"])
        .args(extra)
        .arg(path)
        .status()
        .context("running ffmpeg")?;
    if !status.success() {
        bail!("ffmpeg failed for {}", path.display());
    }
    Ok(())
}

fn check_packets(path: &Path, expected: u32) -> Result<()> {
    let ffprobe = std::env::var("FFPROBE").unwrap_or_else(|_| "ffprobe".into());
    let out = Command::new(ffprobe)
        .args(["-v", "error", "-select_streams", "v:0", "-count_packets", "-show_entries", "stream=nb_read_packets", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .context("running ffprobe")?;
    let n: u32 = String::from_utf8_lossy(&out.stdout).trim().trim_end_matches(',').parse().context("parsing packet count")?;
    if n != expected {
        bail!("{}: expected {expected} video packets, found {n}", path.display());
    }
    println!("  ok  {n} packets");
    Ok(())
}
