//! The template tracker on synthetic frames with an exact answer: a textured
//! blob moving on subpixel paths over a smooth background, guided by a
//! wandering "rough pass" several pixels off. Also patches resampled through
//! a view and from a proxy, against the scene they show.

use tt_core::view::{SourceSize, SpaceMap};
use tt_track::image::{Grid, Luma, Patch, resample, resample_xy};
use tt_track::ncc::{Template, best_match};
use tt_track::template::{Settings, TEMPLATE_R, TemplateTracker};

const W: usize = 320;
const H: usize = 240;

/// The blob's centre at frame f (subpixel).
fn truth(f: usize) -> [f64; 2] {
    let t = f as f64 / 60.0;
    [160.0 + 90.0 * (0.9 * t).sin() + 13.3 * (4.7 * t).sin(), 120.0 + 70.0 * (1.3 * t + 0.4).sin()]
}

/// The scene at source point `p`: a gentle gradient, and a blob of three
/// signed Gaussians around `c`.
fn scene(p: [f64; 2], c: [f64; 2]) -> f64 {
    let g = |x: f64, y: f64, s: f64| (-(x * x + y * y) / (2.0 * s * s)).exp();
    let (dx, dy) = (p[0] - c[0], p[1] - c[1]);
    let blob = 90.0 * g(dx - 3.0, dy + 2.0, 3.0) - 70.0 * g(dx + 4.0, dy - 1.0, 2.5) + 50.0 * g(dx, dy - 5.0, 2.0);
    90.0 + 0.2 * p[0] + 0.1 * p[1] + blob
}

/// A frame of the scene with the blob at `c`.
fn render(c: [f64; 2]) -> Vec<u8> {
    render_sized(c, W, H)
}

/// The same frame in a rendition of `w × h` pixels (a proxy): each pixel is
/// the scene at its centre, in source pixels.
fn render_sized(c: [f64; 2], w: usize, h: usize) -> Vec<u8> {
    let (kx, ky) = (w as f64 / W as f64, h as f64 / H as f64);
    let mut out = vec![0u8; w * h];
    for y in 0..h {
        for x in 0..w {
            let p = [(x as f64 + 0.5) / kx, (y as f64 + 0.5) / ky];
            out[y * w + x] = scene(p, c).round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

/// The rough pass: the truth plus a slow wander of a few pixels.
fn guide(f: usize) -> [f64; 2] {
    let t = f as f64 / 60.0;
    let c = truth(f);
    [c[0] + 4.0 * (2.1 * t).sin() + 1.5 * (7.3 * t).cos(), c[1] - 3.0 * (1.7 * t + 1.0).sin()]
}

fn patch_at(frame: &[u8], centre: [f64; 2], half: f64, scale: f64) -> (Grid, Patch) {
    let map = SpaceMap::identity(&SourceSize { width: W as f64, height: H as f64 });
    let side = (2.0 * half * scale).ceil() as usize;
    let grid = Grid { origin: [centre[0] - side as f64 / 2.0 / scale, centre[1] - side as f64 / 2.0 / scale], scale };
    (grid, resample(&Luma { data: frame, width: W, height: H }, 1.0, &map, grid, side, side))
}

#[test]
fn ncc_finds_a_template_at_subpixel_offsets() {
    // The template: the blob in one frame. Searched for in frames where the blob moved by a known subpixel offset.
    let c0 = [150.0, 110.0];
    let (grid, patch) = patch_at(&render(c0), c0, 40.0, 1.0);
    let t = Template::cut(&patch, grid.from_view(c0), TEMPLATE_R).expect("textured");
    for (dx, dy) in [(0.0, 0.0), (0.25, -0.4), (-0.5, 0.35), (3.3, -2.7)] {
        let c = [c0[0] + dx, c0[1] + dy];
        let (_, moved) = patch_at(&render(c), c0, 40.0, 1.0);
        let m = best_match(&moved, &t, [[0.0, 0.0], [80.0, 80.0]], None).expect("found");
        let want = grid.from_view(c);
        let err = (m.pos[0] - want[0]).hypot(m.pos[1] - want[1]);
        assert!(err < 0.1, "offset ({dx}, {dy}): found {:?}, expected {want:?} (error {err:.3})", m.pos);
        assert!(m.score > 0.95, "score {}", m.score);
    }
}

#[test]
fn patches_through_a_view_and_an_unevenly_scaled_proxy_show_the_scene() {
    // A proxy whose width was rounded to even (as `scale=-2:720` does): kx ≠ ky.
    let c = [200.3, 150.7];
    let (pw, ph) = (162, 120);
    let k = [pw as f64 / W as f64, ph as f64 / H as f64];
    assert_ne!(k[0], k[1]);
    // A view magnifying the source 2×: source = 0.5 · view + b.
    let map = SpaceMap { a: 0.5, b: [120.0, 90.0], canvas: [320.0, 240.0] };
    let at = map.from_source(c);
    let grid = Grid { origin: [at[0] - 40.0, at[1] - 40.0], scale: 1.0 };
    // What the view shows there, straight from the scene (no rendition, no rounding).
    let ideal = Patch { w: 80, h: 80, data: (0..80 * 80).map(|i| scene(map.to_source(grid.to_view([(i % 80) as f64 + 0.5, (i / 80) as f64 + 0.5])), c) as f32).collect() };
    let t = Template::cut(&ideal, grid.from_view(at), TEMPLATE_R).expect("textured");
    let want = grid.from_view(at);
    for (name, frame, w, h, k) in [("original", render(c), W, H, [1.0, 1.0]), ("proxy", render_sized(c, pw, ph), pw, ph, k)] {
        let patch = resample_xy(&Luma { data: &frame, width: w, height: h }, k, &map, grid, 80, 80);
        let m = best_match(&patch, &t, [[20.0, 20.0], [60.0, 60.0]], None).expect("found");
        let err = (m.pos[0] - want[0]).hypot(m.pos[1] - want[1]);
        eprintln!("{name}: the blob at {:?}, expected {want:?} (error {err:.3} view px), score {:.3}", m.pos, m.score);
        assert!(err < 0.25, "{name}: error {err:.3} view px");
    }
}

#[test]
fn flat_patches_are_not_trackable() {
    let frame = vec![128u8; W * H];
    let (grid, patch) = patch_at(&frame, [100.0, 100.0], 30.0, 1.0);
    assert!(Template::cut(&patch, grid.from_view([100.0, 100.0]), TEMPLATE_R).is_none());
}

/// Track 240 frames forward from frame 0 at patch scale `scale`; returns the
/// errors (px) after removing the anchor's offset, which the tracker inherits
/// by definition.
fn track(scale: f64) -> Vec<f64> {
    let settings = Settings { adapt: 0.25, min_score: 0.5 };
    let frame0 = render(truth(0));
    let (grid, patch) = patch_at(&frame0, guide(0), 30.0, scale);
    let mut tracker = TemplateTracker::seed(&patch, grid, guide(0), settings).expect("seeded");
    let bias = [guide(0)[0] - truth(0)[0], guide(0)[1] - truth(0)[1]];
    let mut errors = Vec::new();
    for f in 1..240 {
        let frame = render(truth(f));
        let (grid, patch) = patch_at(&frame, guide(f), 30.0, scale);
        let step = tracker.step(&patch, grid, guide(f));
        assert!(!step.lost, "lost at frame {f} (score {})", step.score);
        let c = truth(f);
        errors.push(((step.pos[0] - bias[0] - c[0]).powi(2) + (step.pos[1] - bias[1] - c[1]).powi(2)).sqrt());
    }
    errors
}

fn stats(mut e: Vec<f64>) -> (f64, f64) {
    e.sort_by(f64::total_cmp);
    (e[e.len() / 2], e[e.len() - 1])
}

#[test]
fn follows_a_moving_blob_through_a_wandering_guide() {
    for scale in [1.0, 0.75, 1.5] {
        let (median, max) = stats(track(scale));
        eprintln!("scale {scale}: median {median:.3} px, max {max:.3} px");
        assert!(median < 0.12 && max < 0.25, "scale {scale}: median {median:.3}, max {max:.3}");
    }
}

/// A hard-edged square (area-sampled, like a sprite or a cursor on screen)
/// shifted by fractions of a pixel. Both the correlation's parabola and
/// Lucas–Kanade land within a tenth of a pixel. (Here, between pixels, the
/// parabola is the closer of the two: bilinear resampling pulls LK toward
/// whole pixels by up to ~0.09 px. On the encoded fixtures, where subjects
/// sit on whole pixels or move smoothly, LK is the better one: see
/// `ncc::refine`.)
#[test]
fn both_subpixel_estimates_find_hard_edges_between_pixels() {
    // The patch: a 9 px square at `c`, each pixel its covered area.
    let square = |c: [f64; 2]| {
        let cover = |a: f64, b: f64, lo: f64, hi: f64| (b.min(hi) - a.max(lo)).max(0.0);
        let data = (0..60 * 60)
            .map(|i| {
                let (x, y) = ((i % 60) as f64, (i / 60) as f64);
                (40.0 + 180.0 * cover(x, x + 1.0, c[0] - 4.5, c[0] + 4.5) * cover(y, y + 1.0, c[1] - 4.5, c[1] + 4.5)) as f32
            })
            .collect();
        Patch { w: 60, h: 60, data }
    };
    let t = Template::cut(&square([30.0, 30.0]), [30.0, 30.0], 8).expect("textured");
    let (mut ncc, mut lk) = (0.0f64, 0.0f64);
    for (dx, dy) in [(0.25, 0.0), (0.5, -0.3), (-0.35, 0.15), (0.1, 0.4), (-0.45, -0.45)] {
        let c = [30.0 + dx, 30.0 + dy];
        let patch = square(c);
        let m = best_match(&patch, &t, [[20.0, 20.0], [40.0, 40.0]], None).expect("found");
        let r = tt_track::ncc::refine(&patch, &t, m.pos);
        ncc = ncc.max((m.pos[0] - c[0]).hypot(m.pos[1] - c[1]));
        lk = lk.max((r[0] - c[0]).hypot(r[1] - c[1]));
    }
    eprintln!("largest error: parabola {ncc:.3} px, Lucas-Kanade {lk:.3} px");
    assert!(ncc < 0.1 && lk < 0.1, "parabola {ncc:.3} px, Lucas-Kanade {lk:.3} px");
}
