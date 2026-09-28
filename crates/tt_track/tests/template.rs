//! The template tracker on synthetic frames with an exact answer: a textured
//! blob moving on subpixel paths over a smooth background, guided by a
//! wandering "rough pass" several pixels off.

use tt_core::view::{SourceSize, SpaceMap};
use tt_track::image::{Grid, Luma, Patch, resample};
use tt_track::ncc::{Template, best_match};
use tt_track::template::{Settings, TEMPLATE_R, TemplateTracker};

const W: usize = 320;
const H: usize = 240;

/// The blob's centre at frame f (subpixel).
fn truth(f: usize) -> [f64; 2] {
    let t = f as f64 / 60.0;
    [160.0 + 90.0 * (0.9 * t).sin() + 13.3 * (4.7 * t).sin(), 120.0 + 70.0 * (1.3 * t + 0.4).sin()]
}

/// A frame: a gentle gradient, and a blob of three signed Gaussians around `c`.
fn render(c: [f64; 2]) -> Vec<u8> {
    let mut out = vec![0u8; W * H];
    let g = |x: f64, y: f64, s: f64| (-(x * x + y * y) / (2.0 * s * s)).exp();
    for y in 0..H {
        for x in 0..W {
            let (px, py) = (x as f64 + 0.5, y as f64 + 0.5);
            let (dx, dy) = (px - c[0], py - c[1]);
            let blob = 90.0 * g(dx - 3.0, dy + 2.0, 3.0) - 70.0 * g(dx + 4.0, dy - 1.0, 2.5) + 50.0 * g(dx, dy - 5.0, 2.0);
            let bg = 90.0 + 0.2 * px + 0.1 * py;
            out[y * W + x] = (bg + blob).round().clamp(0.0, 255.0) as u8;
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
    let frame = render([150.0, 110.0]);
    let (grid, patch) = patch_at(&frame, [150.0, 110.0], 40.0, 1.0);
    for (dx, dy) in [(0.0, 0.0), (0.25, -0.4), (-0.5, 0.35), (3.3, -2.7)] {
        let c = grid.from_view([150.0 + dx, 110.0 + dy]);
        let t = Template::cut(&patch, c, TEMPLATE_R).expect("textured");
        let m = best_match(&patch, &t, [[0.0, 0.0], [80.0, 80.0]], None).expect("found");
        let err = ((m.pos[0] - c[0]).powi(2) + (m.pos[1] - c[1]).powi(2)).sqrt();
        assert!(err < 0.08, "offset ({dx}, {dy}): found {:?}, expected {c:?}", m.pos);
        assert!(m.score > 0.95, "score {}", m.score);
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
