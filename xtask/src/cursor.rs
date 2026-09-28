//! The cursor fixture: a mouse cursor over the kinds of footage that make
//! trackers miss, with its exact hotspot on every frame. Rendered here (4 × 4
//! samples per pixel, like a real cursor on screen), piped to ffmpeg as RGB
//! and encoded like a screen recording (long GOP, B-frames, lossy).
//!
//! Four stretches of 150 frames each:
//! - `changing` (0–149): textured stripes whose angle and phase change every frame;
//! - `bright` (150–299): pale, bright scenery (sky, sand, clouds): the
//!   cursor's white body barely differs from it in luma, only in colour and
//!   by its black outline;
//! - `icons` (300–449): dark foliage-like texture; the cursor turns into a
//!   hand (330–369) and an I-beam (370–409), then back into the arrow;
//! - `flicks` (450–599): a patterned game-like floor; the cursor rests, then
//!   flicks across the screen in 3–5 frames (up to ~190 px per frame);
//! - `decoy` (600–749): a flat desktop with a second, static arrow on it (a
//!   look-alike, like an icon in the scenery): the cursor comes to rest
//!   exactly on it (620–639), then flicks away and wanders off.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::Serialize;

pub const W: usize = 960;
pub const H: usize = 540;
pub const FPS: u32 = 60;
pub const FRAMES: usize = 750;
/// The static look-alike arrow's hotspot (the `decoy` stretch).
pub const DECOY: [f64; 2] = [420.0, 250.0];

/// The hotspot's path: minimum-jerk moves between these `(frame, x, y)`.
/// Moves a few frames long are flicks.
const WAYPOINTS: &[(f64, f64, f64)] = &[
    (0.0, 200.0, 150.0),
    (40.0, 420.0, 260.0),
    (80.0, 300.0, 380.0),
    (120.0, 620.0, 200.0),
    (150.0, 700.0, 300.0),
    (190.0, 520.0, 160.0),
    (230.0, 760.0, 380.0),
    (270.0, 400.0, 300.0),
    (300.0, 480.0, 260.0),
    (330.0, 500.0, 270.0),
    (350.0, 560.0, 240.0),
    (370.0, 560.0, 245.0),
    (400.0, 430.0, 300.0),
    (440.0, 470.0, 280.0),
    (450.0, 480.0, 280.0),
    (470.0, 500.0, 285.0),
    (474.0, 850.0, 180.0),
    (500.0, 860.0, 190.0),
    (503.0, 300.0, 400.0),
    (530.0, 310.0, 395.0),
    (535.0, 650.0, 120.0),
    (560.0, 640.0, 130.0),
    (563.0, 660.0, 450.0),
    (599.0, 600.0, 420.0),
    (620.0, DECOY[0], DECOY[1]),
    (640.0, DECOY[0], DECOY[1]),
    (644.0, 700.0, 330.0),
    (680.0, 760.0, 240.0),
    (715.0, 610.0, 420.0),
    (749.0, 520.0, 380.0),
];

/// The hotspot at (continuous) frame `f`.
pub fn hotspot(f: f64) -> [f64; 2] {
    let i = WAYPOINTS.partition_point(|w| w.0 <= f).clamp(1, WAYPOINTS.len() - 1);
    let (a, b) = (WAYPOINTS[i - 1], WAYPOINTS[i]);
    let u = ((f - a.0) / (b.0 - a.0)).clamp(0.0, 1.0);
    let s = u * u * u * (10.0 - 15.0 * u + 6.0 * u * u);
    [a.1 + (b.1 - a.1) * s, a.2 + (b.2 - a.2) * s]
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Icon {
    Arrow,
    Hand,
    Ibeam,
}

pub fn icon(f: usize) -> Icon {
    match f {
        330..370 => Icon::Hand,
        370..410 => Icon::Ibeam,
        _ => Icon::Arrow,
    }
}

/// Each icon's outline, relative to its hotspot (px), and whether its body
/// is light with a dark rim (the arrow, the hand) or dark with a light rim (the I-beam).
pub fn shape(icon: Icon) -> (&'static [[f64; 2]], bool) {
    match icon {
        Icon::Arrow => (&[[0.0, 0.0], [0.0, 17.0], [4.0, 13.0], [7.0, 19.5], [9.5, 18.5], [6.5, 12.0], [12.0, 12.0]], true),
        Icon::Hand => (
            &[[-1.5, 0.0], [1.5, 0.0], [1.5, 8.0], [3.5, 7.0], [6.0, 8.0], [8.0, 8.0], [10.0, 9.0], [12.0, 10.0], [12.0, 17.0], [10.0, 21.0], [1.0, 21.0], [-4.5, 14.0], [-5.0, 11.0], [-3.0, 10.0], [-1.5, 12.0]],
            true,
        ),
        Icon::Ibeam => (
            &[[-3.5, -9.0], [3.5, -9.0], [3.5, -7.0], [1.0, -7.0], [1.0, 7.0], [3.5, 7.0], [3.5, 9.0], [-3.5, 9.0], [-3.5, 7.0], [-1.0, 7.0], [-1.0, -7.0], [-3.5, -7.0]],
            false,
        ),
    }
}

/// The rim's width (px).
pub const RIM: f64 = 1.2;

/// Signed distance to a polygon: negative inside.
fn signed_distance(p: [f64; 2], poly: &[[f64; 2]]) -> f64 {
    let (mut d, mut inside) = (f64::INFINITY, false);
    for i in 0..poly.len() {
        let (a, b) = (poly[i], poly[(i + 1) % poly.len()]);
        let (ex, ey) = (b[0] - a[0], b[1] - a[1]);
        let t = (((p[0] - a[0]) * ex + (p[1] - a[1]) * ey) / (ex * ex + ey * ey)).clamp(0.0, 1.0);
        d = d.min((p[0] - a[0] - t * ex).hypot(p[1] - a[1] - t * ey));
        if (a[1] > p[1]) != (b[1] > p[1]) && p[0] < a[0] + (p[1] - a[1]) / (b[1] - a[1]) * ex {
            inside = !inside;
        }
    }
    if inside { -d } else { d }
}

/// The cursor's colour at `p` (relative to the hotspot), if it covers `p`.
pub fn cursor_at(icon: Icon, p: [f64; 2]) -> Option<[f64; 3]> {
    let (poly, light) = shape(icon);
    let d = signed_distance(p, poly);
    if d > 0.0 {
        return None;
    }
    let rim = d > -RIM;
    Some(if rim == light { [8.0; 3] } else { [252.0; 3] })
}

/// A cheap hash → [0, 1).
fn hash(x: i64, y: i64, s: i64) -> f64 {
    let mut h = (x as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ (y as u64).wrapping_mul(0xc2b2_ae3d_27d4_eb4f) ^ (s as u64).wrapping_mul(0x1656_67b1_9e37_79f9);
    h ^= h >> 29;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 32;
    (h >> 11) as f64 / (1u64 << 53) as f64
}

/// Smooth value noise at scale `cell` px.
fn noise(x: f64, y: f64, cell: f64, s: i64) -> f64 {
    let (u, v) = (x / cell, y / cell);
    let (i, j) = (u.floor() as i64, v.floor() as i64);
    let (fx, fy) = (u - u.floor(), v - v.floor());
    let (sx, sy) = (fx * fx * (3.0 - 2.0 * fx), fy * fy * (3.0 - 2.0 * fy));
    let a = hash(i, j, s) + (hash(i + 1, j, s) - hash(i, j, s)) * sx;
    let b = hash(i, j + 1, s) + (hash(i + 1, j + 1, s) - hash(i, j + 1, s)) * sx;
    a + (b - a) * sy
}

/// The scenery at pixel point `(x, y)` on frame `f` (RGB, 0–255).
pub fn background(x: f64, y: f64, f: usize) -> [f64; 3] {
    let t = f as f64 / FPS as f64;
    match f / 150 {
        0 => {
            // Stripes whose angle and phase change every frame, lightly tinted.
            let (a, b) = (0.3 + 0.17 * (f % 7) as f64, 1.1 * f as f64);
            let v = 128.0 + 95.0 * ((a * x + (1.0 - a) * y) * 0.35 + b).sin();
            [v * 0.95 + 6.0, v, v * 0.9 + 12.0]
        }
        1 => {
            // Pale sky over pale sand, with slow bright clouds.
            let sky = [196.0, 222.0, 250.0];
            let sand = [246.0, 232.0, 176.0];
            let k = (y / H as f64 + 0.15 * (x * 0.004 + t).sin()).clamp(0.0, 1.0);
            let base: [f64; 3] = std::array::from_fn(|c| sky[c] + (sand[c] - sky[c]) * k);
            let cloud = (noise(x + 30.0 * t, y, 70.0, 11) * 1.6 - 0.55).clamp(0.0, 1.0);
            std::array::from_fn(|c| base[c] + (248.0 - base[c]) * cloud)
        }
        2 => {
            // Dark foliage: layered greenish noise, slowly panning.
            let n = 0.55 * noise(x + 8.0 * t, y, 9.0, 3) + 0.3 * noise(x, y + 5.0 * t, 3.5, 5) + 0.15 * noise(x, y, 40.0, 7);
            [18.0 + 60.0 * n, 34.0 + 110.0 * n, 16.0 + 45.0 * n]
        }
        4 => {
            // A flat desktop: two panels and a faint grid.
            let panel = if x < 540.0 { [58.0, 110.0, 150.0] } else { [196.0, 200.0, 206.0] };
            let grid = ((x / 48.0).fract() < 0.03 || (y / 48.0).fract() < 0.03) as u8 as f64;
            std::array::from_fn(|c| panel[c] - 14.0 * grid)
        }
        _ => {
            // A game-like floor: bricks with mortar, coloured, panning.
            let (px, py) = (x + 40.0 * t, y + 12.0 * t);
            let row = (py / 18.0).floor();
            let bx = (px + if row as i64 % 2 == 0 { 0.0 } else { 16.0 }) / 32.0;
            let mortar = (bx - bx.floor() < 0.06) || (py / 18.0 - row < 0.1);
            let shade = 0.75 + 0.25 * hash(bx.floor() as i64, row as i64, 9) + 0.1 * noise(px, py, 4.0, 13);
            if mortar { [70.0, 66.0, 60.0] } else { [150.0 * shade, 82.0 * shade, 58.0 * shade] }
        }
    }
}

/// Frame `f` as RGB bytes.
pub fn render(f: usize) -> Vec<u8> {
    let c = hotspot(f as f64);
    let ic = icon(f);
    // The look-alike, under the cursor.
    let decoy = f / 150 == 4;
    let scene = |x: f64, y: f64| {
        let d = [x - DECOY[0], y - DECOY[1]];
        decoy.then(|| cursor_at(Icon::Arrow, d)).flatten().unwrap_or_else(|| background(x, y, f))
    };
    let mut out = vec![0u8; W * H * 3];
    for y in 0..H {
        for x in 0..W {
            let near_to = |c: [f64; 2]| (x as f64 + 0.5 - c[0]).abs() < 24.0 && (y as f64 + 0.5 - c[1]).abs() < 30.0;
            let near = near_to(c) || (decoy && near_to(DECOY));
            let px: [f64; 3] = if near {
                let mut acc = [0.0; 3];
                for k in 0..16 {
                    let (sx, sy) = (x as f64 + (k % 4) as f64 * 0.25 + 0.125, y as f64 + (k / 4) as f64 * 0.25 + 0.125);
                    let v = cursor_at(ic, [sx - c[0], sy - c[1]]).unwrap_or_else(|| scene(sx, sy));
                    (0..3).for_each(|i| acc[i] += v[i] / 16.0);
                }
                acc
            } else {
                background(x as f64 + 0.5, y as f64 + 0.5, f)
            };
            for i in 0..3 {
                out[(y * W + x) * 3 + i] = px[i].round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

#[derive(Serialize)]
struct Truth {
    file: String,
    width: usize,
    height: usize,
    fps: u32,
    /// The hotspot per frame (continuous source px).
    hotspot: Vec<[f64; 2]>,
    icon: Vec<Icon>,
    /// `(name, first frame, end frame)`.
    stretches: Vec<(&'static str, usize, usize)>,
    shapes: Vec<(Icon, Vec<[f64; 2]>, bool)>,
    rim: f64,
    /// The static look-alike's hotspot in the `decoy` stretch.
    decoy: [f64; 2],
}

/// Write `cursor_540p60.mp4` and `cursor_truth.json` into `out`.
pub fn make(out: &Path, ffmpeg: &str, force: bool) -> Result<()> {
    let file = "cursor_540p60.mp4";
    let path = out.join(file);
    if path.exists() && !force {
        println!("skip  {file} (exists)");
    } else {
        println!("make  {file}");
        let mut child = Command::new(ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "rawvideo", "-pix_fmt", "rgb24", "-s", &format!("{W}x{H}"), "-r", &FPS.to_string(), "-i", "-"])
            .args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "18", "-x264-params", "keyint=250:min-keyint=250:scenecut=0:bframes=3"])
            .args(["-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709", "-vf", "scale=out_color_matrix=bt709:out_range=tv"])
            .args(["-pix_fmt", "yuv420p", "-movflags", "+faststart"])
            .arg(&path)
            .stdin(Stdio::piped())
            .spawn()
            .context("running ffmpeg")?;
        let mut stdin = child.stdin.take().expect("piped");
        for f in 0..FRAMES {
            stdin.write_all(&render(f))?;
        }
        drop(stdin);
        if !child.wait()?.success() {
            bail!("ffmpeg failed for {file}");
        }
    }
    let truth = Truth {
        file: file.into(),
        width: W,
        height: H,
        fps: FPS,
        hotspot: (0..FRAMES).map(|f| hotspot(f as f64)).collect(),
        icon: (0..FRAMES).map(icon).collect(),
        stretches: vec![("changing", 0, 150), ("bright", 150, 300), ("icons", 300, 450), ("flicks", 450, 600), ("decoy", 600, 750)],
        decoy: DECOY,
        shapes: [Icon::Arrow, Icon::Hand, Icon::Ibeam].into_iter().map(|i| (i, shape(i).0.to_vec(), shape(i).1)).collect(),
        rim: RIM,
    };
    std::fs::write(out.join("cursor_truth.json"), serde_json::to_string(&truth)?)?;
    Ok(())
}
