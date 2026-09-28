//! Masked, rectangular templates: a cursor-like arrow over a background that
//! changes completely from frame to frame. Only the arrow's pixels are the
//! subject; a mask painted over them keeps the background from counting.

use tt_core::view::{SourceSize, SpaceMap};
use tt_track::image::{Grid, Luma, Patch, resample};
use tt_track::ncc::{Mask, Template, best_match, photometric};

const W: usize = 200;
const H: usize = 160;

/// Inside the arrow (a Windows-cursor-like wedge, 12 wide × 18 tall) with its tip at `tip`.
fn in_arrow(x: f64, y: f64, tip: [f64; 2]) -> bool {
    let (u, v) = (x - tip[0], y - tip[1]);
    (0.0..18.0).contains(&v) && u >= 0.0 && u <= v * 0.66
}

/// A frame: stripes whose phase and angle change with `seed`, and the arrow
/// at `tip` (a black outline around a white fill, antialiased: 4 × 4 samples
/// per pixel, like a real cursor on screen).
fn render(tip: [f64; 2], seed: u32) -> Vec<u8> {
    let (a, b) = (0.3 + 0.17 * (seed % 7) as f64, 1.1 * seed as f64);
    let mut out = vec![0u8; W * H];
    for y in 0..H {
        for x in 0..W {
            let mut acc = 0.0;
            for k in 0..16 {
                let (px, py) = (x as f64 + (k % 4) as f64 * 0.25 + 0.125, y as f64 + (k / 4) as f64 * 0.25 + 0.125);
                let inner = [tip[0] + 1.3, tip[1] + 2.6];
                acc += if in_arrow(px, py, tip) {
                    if (py - inner[1]) < 13.0 && in_arrow(px, py, inner) { 245.0 } else { 15.0 }
                } else {
                    128.0 + 100.0 * ((a * px + (1.0 - a) * py) * 0.35 + b).sin()
                };
            }
            out[y * W + x] = (acc / 16.0).clamp(0.0, 255.0) as u8;
        }
    }
    out
}

fn patch_of(frame: &[u8]) -> (Grid, Patch) {
    let map = SpaceMap::identity(&SourceSize { width: W as f64, height: H as f64 });
    let grid = Grid { origin: [0.0, 0.0], scale: 1.0 };
    (grid, resample(&Luma { data: frame, width: W, height: H }, 1.0, &map, grid, W, H))
}

/// A mask over the rectangle `[tip.x − 2, tip.x + 14] × [tip.y − 2, tip.y + 20]`: the arrow's cells.
fn arrow_mask(n: usize) -> Vec<u8> {
    let (w, h) = (16.0, 22.0);
    (0..n * n)
        .map(|k| {
            let (i, j) = ((k % n) as f64, (k / n) as f64);
            let (x, y) = (-2.0 + (i + 0.5) / n as f64 * w, -2.0 + (j + 0.5) / n as f64 * h);
            if in_arrow(x, y, [0.0, 0.0]) { 255 } else { 0 }
        })
        .collect()
}

#[test]
fn a_masked_template_finds_the_cursor_over_any_background() {
    let tip0 = [60.0, 50.0];
    let (grid, patch0) = patch_of(&render(tip0, 0));
    // The look: the rectangle around the arrow, 17 × 23 template pixels.
    let centre = [tip0[0] + 6.0, tip0[1] + 9.0];
    let cells = arrow_mask(32);
    let masked = Template::cut_rect(&patch0, grid.from_view(centre), [8, 11], Some(Mask { cells: &cells, w: 32, h: 32 })).expect("textured");
    let plain = Template::cut_rect(&patch0, grid.from_view(centre), [8, 11], None).expect("textured");
    let (mut worst_masked, mut worst_plain, mut err) = (1.0f32, 1.0f32, 0.0f64);
    for f in 1..30u32 {
        let tip = [60.0 + 3.3 * f as f64, 50.0 + 1.7 * f as f64];
        let (grid, patch) = patch_of(&render(tip, f));
        let want = grid.from_view([tip[0] + 6.0, tip[1] + 9.0]);
        let window = [[want[0] - 20.0, want[1] - 20.0], [want[0] + 20.0, want[1] + 20.0]];
        let m = best_match(&patch, &masked, window, None).expect("found");
        let p = best_match(&patch, &plain, window, None).expect("found");
        worst_masked = worst_masked.min(m.score);
        worst_plain = worst_plain.min(p.score);
        err = err.max((m.pos[0] - want[0]).hypot(m.pos[1] - want[1]));
    }
    println!("worst score: masked {worst_masked:.2}, unmasked {worst_plain:.2}; masked error at most {err:.2} px");
    assert!(worst_masked > 0.7, "the mask keeps the background out: {worst_masked}");
    assert!(err < 0.6, "on the cursor every frame: {err}");
    assert!(worst_plain < worst_masked - 0.2, "without the mask the background drags the score down: {worst_plain}");
}

#[test]
fn an_empty_mask_is_no_template() {
    let (grid, patch) = patch_of(&render([60.0, 50.0], 0));
    let empty = vec![0u8; 16];
    assert!(Template::cut_rect(&patch, grid.from_view([66.0, 59.0]), [8, 11], Some(Mask { cells: &empty, w: 4, h: 4 })).is_none());
}

/// A dim, low-contrast copy of the cursor's shape (what dark foliage with a
/// similar gradient amounts to) correlates perfectly, but it isn't the
/// cursor: its brightness and contrast are nothing like the cursor's, so it
/// scores low, and the real one wins wherever both are in the search.
#[test]
fn a_dim_look_alike_does_not_score_like_the_bright_cursor() {
    let (bright, dim) = ([40.0, 40.0], [140.0, 100.0]);
    let mut frame = vec![20u8; W * H];
    for y in 0..H {
        for x in 0..W {
            let (px, py) = (x as f64 + 0.5, y as f64 + 0.5);
            if in_arrow(px, py, bright) {
                frame[y * W + x] = 240;
            } else if in_arrow(px, py, dim) {
                frame[y * W + x] = 30; // the same shape, a tenth of the contrast, near black
            }
        }
    }
    let (grid, patch) = patch_of(&frame);
    let cells = arrow_mask(32);
    let masked = Template::cut_rect(&patch, grid.from_view([bright[0] + 6.0, bright[1] + 9.0]), [8, 11], Some(Mask { cells: &cells, w: 32, h: 32 })).expect("textured");
    let plain = Template::cut_rect(&patch, grid.from_view([bright[0] + 6.0, bright[1] + 9.0]), [8, 11], None).expect("textured");
    let around = |c: [f64; 2]| [[c[0] + 6.0 - 4.0, c[1] + 9.0 - 4.0], [c[0] + 6.0 + 4.0, c[1] + 9.0 + 4.0]];
    for (name, t) in [("masked", &masked), ("plain", &plain)] {
        let at_dim = best_match(&patch, t, around(dim), None).expect("placed").score;
        let whole = best_match(&patch, t, [[f64::NEG_INFINITY; 2], [f64::INFINITY; 2]], None).expect("found");
        println!("{name}: the dim look-alike scores {at_dim:.2}; over the whole frame the best is at {:?} ({:.2})", whole.pos, whole.score);
        assert!(at_dim < 0.3, "{name}: the dim look-alike scores {at_dim:.2}");
        assert!((whole.pos[0] - bright[0] - 6.0).abs() < 1.0 && (whole.pos[1] - bright[1] - 9.0).abs() < 1.0, "{name}: {:?}", whole.pos);
        assert!(whole.score > 0.8, "{name}: {:.2}", whole.score);
    }
    // The factor itself: like the template, it costs nothing; 2x either way, still nothing.
    assert_eq!(photometric(&masked, masked.mean, masked.sd), 1.0);
    assert_eq!(photometric(&masked, masked.mean, masked.sd * 2.0), 1.0);
    assert!(photometric(&masked, masked.mean, masked.sd * 0.1) < 0.25);
}
