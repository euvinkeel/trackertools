//! Motion sketch accuracy (ROADMAP M3): a scripted "noisy hand" follows the
//! fixture sprite's analytic path; the pipeline must recover the subject.

use tt_core::sketch::{ClockMap, SketchParams, sketch_boxes};

const FPS: f64 = 60.0;

/// The sprite fixture's centre (cargo xtask fixtures): source pixels at video time t.
fn subject(t: f64) -> [f64; 2] {
    [
        (950.0 + 500.0 * (0.9 * t).sin() + 60.0 * (5.3 * t).sin()).floor() + 10.5,
        (530.0 + 300.0 * (1.3 * t + 0.7).sin() + 40.0 * (4.1 * t).sin()).floor() + 10.5,
    ]
}

struct Rng(u64);
impl Rng {
    fn unit(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
    fn normal(&mut self) -> f64 {
        let (u, v) = (self.unit().max(1e-12), self.unit());
        (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
    }
}

/// A hand following the subject at `rate` × speed from video time `v0` for
/// `wall` seconds: `lag` behind, with tremor and jitter; 1 kHz samples and a
/// 175 Hz UI clock.
fn follow(rate: f64, v0: f64, wall: f64, lag: f64, tremor: f64, seed: u64) -> (Vec<[f64; 3]>, ClockMap) {
    let mut rng = Rng(seed);
    let mut samples = Vec::new();
    let mut w = 0.0;
    while w <= wall {
        // The hand follows the *displayed* frame (the image changes 60×/s of video).
        let shown = ((v0 + rate * (w - lag).max(0.0)) * FPS).floor();
        let s = subject(shown / FPS);
        let shake = |ph: f64| tremor * ((9.0 * std::f64::consts::TAU * w + ph).sin() * 0.6 + (12.3 * std::f64::consts::TAU * w + 2.0 * ph).sin() * 0.4);
        samples.push([w, s[0] + shake(0.0) + rng.normal() * tremor * 0.5, s[1] + shake(1.3) + rng.normal() * tremor * 0.5]);
        w += 0.001;
    }
    let mut clock = ClockMap::default();
    let mut w = 0.0;
    while w <= wall {
        clock.push(w, (v0 + rate * w) * FPS, true);
        w += 1.0 / 175.0;
    }
    (samples, clock)
}

fn quantile(mut v: Vec<f64>, q: f64) -> f64 {
    v.sort_by(f64::total_cmp);
    v[((v.len() - 1) as f64 * q).round() as usize]
}

#[test]
fn slow_motion_sketch_recovers_the_subject() {
    let (samples, clock) = follow(0.25, 2.0, 20.0, 0.25, 2.0, 7);
    let params = SketchParams::default(); // lag 0.25 s matches the hand
    let (first, frames) = sketch_boxes(&samples, &clock, &params, FPS).expect("result");
    let (mut errors, mut inside, mut total) = (Vec::new(), 0, 0);
    // Every frame counts, the capture's first and last included.
    for (i, f) in frames.iter().enumerate() {
        let Some(b) = f else { continue };
        let truth = subject((first + i as i64) as f64 / FPS);
        errors.push(((b[0] - truth[0]).powi(2) + (b[1] - truth[1]).powi(2)).sqrt());
        total += 1;
        if truth[0] >= b[2] && truth[0] <= b[4] && truth[1] >= b[3] && truth[1] <= b[5] {
            inside += 1;
        }
    }
    let (median, p95) = (quantile(errors.clone(), 0.5), quantile(errors, 0.95));
    let containment = inside as f64 / total as f64;
    println!("frames {total}: point error median {median:.2} px, p95 {p95:.2} px; subject inside region {:.2}%", containment * 100.0);
    // The frames shown in the last `lag` before the release (0.25 s × ¼ × 60 fps ≈ 4) are
    // never reached by the hand and have no result; every other visited frame does.
    let with_result: Vec<i64> = frames.iter().enumerate().filter(|(_, f)| f.is_some()).map(|(i, _)| first + i as i64).collect();
    let (a, z) = (with_result[0], *with_result.last().unwrap());
    assert_eq!(a, 120, "results start at the first frame");
    assert!((120 + 300 - 6..=120 + 300 - 3).contains(&z), "results end at frame {z}");
    assert_eq!(with_result.len() as i64, z - a + 1, "no gaps");
    assert!(median < 1.5, "median point error {median:.2} px (measured 1.10 when set)");
    assert!(p95 < 3.0, "p95 point error {p95:.2} px (measured 1.95 when set)");
    assert!(containment >= 0.995, "subject inside the region on {:.2}% of frames", containment * 100.0);
}

#[test]
fn hold_to_simulate_sizes_the_box_by_jiggle() {
    // Paused on frame 500 for 2 s: the pointer hovers near (800, 400). The box
    // reflects the jiggle of roughly the last jiggle-window × 3 of the hold.
    let run = |jiggle_at_end: bool| {
        let mut rng = Rng(3);
        let mut samples = Vec::new();
        let mut w = 0.0;
        while w <= 2.0 {
            let amp = if (w > 1.0) == jiggle_at_end { 25.0 } else { 0.5 };
            samples.push([w, 800.0 + rng.normal() * amp, 400.0 + rng.normal() * amp]);
            w += 0.001;
        }
        let mut clock = ClockMap::default();
        let mut w = 0.0;
        while w <= 2.0 {
            clock.push(w, 500.0, false);
            w += 1.0 / 175.0;
        }
        let params = SketchParams { lag: 0.0, ..SketchParams::default() };
        let (first, frames) = sketch_boxes(&samples, &clock, &params, FPS).unwrap();
        assert_eq!(first, 500);
        assert_eq!(frames.len(), 1, "a hold maps to exactly one frame");
        let b = frames[0].unwrap();
        (b[4] - b[2], b[5] - b[3])
    };
    let (still_w, still_h) = run(false);
    let (jiggle_w, jiggle_h) = run(true);
    println!("box after holding still: {still_w:.1}×{still_h:.1}; after jiggling: {jiggle_w:.1}×{jiggle_h:.1}");
    assert!(still_w <= 2.0 * 16.0 + 1.0, "holding still for 1 s settles to the minimum box ({still_w:.1})");
    assert!(jiggle_w > 3.0 * still_w, "jiggling grows the box ({jiggle_w:.1} vs {still_w:.1})");
}

/// Parameter sweep for choosing defaults: `cargo test -p tt_core --test sketch sweep -- --ignored --nocapture`
#[test]
#[ignore]
fn sweep() {
    let (samples, clock) = follow(0.25, 2.0, 20.0, 0.25, 2.0, 7);
    let trim = (0.25 * 0.25 * FPS) as usize + 2;
    let score = |p: &SketchParams| {
        let (first, frames) = sketch_boxes(&samples, &clock, p, FPS).unwrap();
        let errors: Vec<f64> = frames
            .iter()
            .enumerate()
            .skip(trim)
            .take(frames.len() - 2 * trim)
            .filter_map(|(i, f)| {
                let b = (*f)?;
                let t = subject((first + i as i64) as f64 / FPS);
                Some(((b[0] - t[0]).powi(2) + (b[1] - t[1]).powi(2)).sqrt())
            })
            .collect();
        (quantile(errors.clone(), 0.5), quantile(errors, 0.95))
    };
    println!("dead_zone steadiness responsiveness -> median p95");
    for dz in [0.0f32, 1.5] {
        for st in [0.5f32, 1.0, 2.0, 4.0] {
            for beta in [0.0f32, 0.005, 0.02] {
                let p = SketchParams { dead_zone: dz, steadiness: st, responsiveness: beta, ..SketchParams::default() };
                let (m, q) = score(&p);
                println!("{dz:>4} {st:>4} {beta:>6} -> {m:5.2} {q:5.2}");
            }
        }
    }
}

/// Diagnostic: a perfect hand (no tremor) isolates systematic error.
#[test]
#[ignore]
fn perfect_hand() {
    for (lag_true, lag_param) in [(0.25, 0.25), (0.0, 0.0)] {
        let (samples, clock) = follow(0.25, 2.0, 20.0, lag_true, 0.0, 7);
        let p = SketchParams { lag: lag_param as f32, dead_zone: 0.0, steadiness: 8.0, responsiveness: 0.05, smooth_position: 0.0, ..SketchParams::default() };
        let (first, frames) = sketch_boxes(&samples, &clock, &p, FPS).unwrap();
        let trim = 20;
        let signed: Vec<(f64, f64)> = frames
            .iter()
            .enumerate()
            .skip(trim)
            .take(frames.len() - 2 * trim)
            .filter_map(|(i, f)| {
                let b = (*f)?;
                let f = first + i as i64;
                let t = subject(f as f64 / FPS);
                let next = subject((f + 1) as f64 / FPS);
                // Project the error onto the direction of motion (in frames).
                let (vx, vy) = (next[0] - t[0], next[1] - t[1]);
                let v2 = vx * vx + vy * vy;
                (v2 > 4.0).then(|| (((b[0] - t[0]) * vx + (b[1] - t[1]) * vy) / v2, ((b[0] - t[0]).powi(2) + (b[1] - t[1]).powi(2)).sqrt()))
            })
            .collect();
        let along: Vec<f64> = signed.iter().map(|s| s.0).collect();
        let err: Vec<f64> = signed.iter().map(|s| s.1).collect();
        println!("true lag {lag_true}: error median {:.2} px; offset along motion median {:+.3} frames", quantile(err, 0.5), quantile(along, 0.5));
    }
}

/// Diagnostic: the first and last frames of a capture.
#[test]
#[ignore]
fn edges() {
    let (samples, clock) = follow(0.25, 2.0, 8.0, 0.25, 2.0, 7);
    let (first, frames) = sketch_boxes(&samples, &clock, &SketchParams::default(), FPS).unwrap();
    let n = frames.len();
    for i in (0..8).chain(n - 8..n) {
        let Some(b) = frames[i] else { continue };
        let t = subject((first + i as i64) as f64 / FPS);
        println!(
            "frame {:>4}: point err ({:+6.1}, {:+6.1})  box x {:+6.1}..{:+6.1} y {:+6.1}..{:+6.1} (relative to truth)",
            first + i as i64,
            b[0] - t[0],
            b[1] - t[1],
            b[2] - t[0],
            b[4] - t[0],
            b[3] - t[1],
            b[5] - t[1]
        );
    }
}

/// Re-tuning speed (ROADMAP M3: a 60 s capture re-derives in ≤ 5 ms):
/// `cargo test --release -p tt_core --test sketch retune_speed -- --ignored --nocapture`
#[test]
#[ignore]
fn retune_speed() {
    for rate in [1.0, 0.25] {
        let (samples, clock) = follow(rate, 2.0, 60.0, 0.25, 2.0, 7);
        let p = SketchParams::default();
        let _ = sketch_boxes(&samples, &clock, &p, FPS);
        let runs = 20;
        let t = std::time::Instant::now();
        let mut frames = 0;
        for _ in 0..runs {
            frames = sketch_boxes(&samples, &clock, &p, FPS).unwrap().1.len();
        }
        println!("60 s capture at {rate}× ({} samples, {frames} frames): {:.2} ms per re-derive", samples.len(), t.elapsed().as_secs_f64() * 1e3 / runs as f64);
    }
}

// ---- layering strokes --------------------------------------------------------------------

mod layering {
    use tt_core::sketch::{Stroke, falloff_weight, layer_over};

    fn stroke(influence: f32, size: f32) -> Stroke {
        Stroke { influence, size, ..Stroke::default() }
    }

    /// A path moving right one pixel per frame, frames 0..300.
    fn line(f: i64) -> Option<[f64; 6]> {
        (0..300).contains(&f).then(|| {
            let x = f as f64;
            [x, 50.0, x - 10.0, 40.0, x + 10.0, 60.0]
        })
    }

    fn shifted(f: i64, dx: f64) -> Option<[f64; 6]> {
        line(f).map(|v| [v[0] + dx, v[1], v[2] + dx, v[3], v[4] + dx, v[5]])
    }

    fn get(result: &(i64, Vec<Option<[f64; 6]>>), f: i64) -> Option<[f64; 6]> {
        result.1.get(usize::try_from(f - result.0).ok()?).copied().flatten()
    }

    #[test]
    fn a_one_frame_edit_pulls_its_neighbours_with_falloff() {
        let r = layer_over(line, 100, &[shifted(100, 40.0)], 10.0, &stroke(1.0, 1.0));
        assert_eq!(get(&r, 100).unwrap()[0], 140.0, "the edited frame takes the stroke");
        let mut last = 40.0;
        for k in 1..=10 {
            for f in [100 - k, 100 + k] {
                let moved = get(&r, f).unwrap()[0] - f as f64;
                assert!((moved - 40.0 * falloff_weight(k as f64, 10.0)).abs() < 1e-9, "frame {f} moved {moved}");
                assert!(moved < last + 1e-9 && moved > 0.0, "falloff decreases with distance");
            }
            last = get(&r, 100 + k).unwrap()[0] - (100 + k) as f64;
            // The neighbours keep their own motion: box edges move with the point.
            let v = get(&r, 100 + k).unwrap();
            assert!((v[4] - v[0] - 10.0).abs() < 1e-9);
        }
        assert!(get(&r, 89).is_none() && get(&r, 111).is_none(), "beyond the radius nothing changes");
    }

    #[test]
    fn frames_between_two_edits_with_the_same_offset_move_by_exactly_that_offset() {
        let stroke: Vec<_> = (100..=106).map(|f| if f == 100 || f == 106 { shifted(f, 40.0) } else { None }).collect();
        let r = layer_over(line, 100, &stroke, 10.0, &super::layering::stroke(1.0, 1.0));
        for f in 100..=106 {
            let moved = get(&r, f).unwrap()[0] - f as f64;
            assert!((moved - 40.0).abs() < 1e-9, "frame {f} moved {moved}: normalised, no overshoot");
        }
    }

    #[test]
    fn influence_blends_and_zero_radius_touches_only_the_visited_frames() {
        let r = layer_over(line, 100, &[shifted(100, 40.0)], 0.0, &stroke(0.5, 1.0));
        assert_eq!(get(&r, 100).unwrap()[0], 120.0);
        assert!(get(&r, 99).is_none() && get(&r, 101).is_none());
    }

    #[test]
    fn a_move_only_stroke_keeps_the_regions_size() {
        // The stroke's region is tiny (a quiet hold), 40 px to the right.
        let tiny = [140.0, 50.0, 138.0, 48.0, 142.0, 52.0];
        let moved = layer_over(line, 100, &[Some(tiny)], 10.0, &stroke(1.0, 0.0));
        let v = get(&moved, 100).unwrap();
        assert_eq!(v, [140.0, 50.0, 130.0, 40.0, 150.0, 60.0], "the point moves, the extents stay 10 px");
        let resized = layer_over(line, 100, &[Some(tiny)], 10.0, &stroke(1.0, 1.0));
        assert_eq!(get(&resized, 100).unwrap(), tiny, "size 1 takes the stroke's region");
    }

    #[test]
    fn bridging_follows_the_real_radius() {
        let none = |_| None;
        let key = |x: f64| Some([x, 0.0, x - 5.0, -5.0, x + 5.0, 5.0]);
        let gap = |n: usize| {
            let mut s = vec![None; n + 2];
            s[0] = key(0.0);
            s[n + 1] = key(10.0);
            s
        };
        // radius 0.5 frames: a one-frame gap (≤ 2·0.5) is bridged, a two-frame gap is not.
        assert!(get(&layer_over(none, 100, &gap(1), 0.5, &stroke(1.0, 1.0)), 101).is_some());
        let two = layer_over(none, 100, &gap(2), 0.5, &stroke(1.0, 1.0));
        assert!(get(&two, 101).is_none() && get(&two, 102).is_none());
    }

    #[test]
    fn new_territory_is_bridged_between_nearby_edits_only() {
        let none = |_| None;
        let key = |x: f64| Some([x, 0.0, x - 5.0, -5.0, x + 5.0, 5.0]);
        // Two holds 8 frames apart, radius 10: the frames between are interpolated.
        let mut stroke = vec![None; 9];
        stroke[0] = key(0.0);
        stroke[8] = key(80.0);
        let r = layer_over(none, 100, &stroke, 10.0, &super::layering::stroke(1.0, 1.0));
        for k in 0..=8 {
            assert!((get(&r, 100 + k).unwrap()[0] - 10.0 * k as f64).abs() < 1e-9, "frame {}", 100 + k);
        }
        assert!(get(&r, 99).is_none() && get(&r, 109).is_none(), "no extrapolation past the ends");
        // 30 frames apart (more than twice the radius): left alone.
        let mut far = vec![None; 31];
        far[0] = key(0.0);
        far[30] = key(300.0);
        let r = layer_over(none, 100, &far, 10.0, &super::layering::stroke(1.0, 1.0));
        assert!((101..130).all(|f| get(&r, f).is_none()));
    }
}

/// Diagnostic: a pure hold with a ±20 px, 3 Hz jiggle in x, by hold length.
#[test]
#[ignore]
fn hold_jiggle_size() {
    for secs in [0.5, 1.0, 2.0] {
        let mut samples = Vec::new();
        let mut w = 0.0;
        while w <= secs {
            samples.push([w, 500.0 + 20.0 * (std::f64::consts::TAU * 3.0 * (w + 7.3)).sin(), 300.0]);
            w += 0.001;
        }
        let mut clock = ClockMap::default();
        let mut w = 0.0;
        while w <= secs {
            clock.push(w, 150.0, false);
            w += 1.0 / 175.0;
        }
        let (_, frames) = tt_core::sketch::stroke_frames(&samples, &clock, &SketchParams::default(), FPS).unwrap();
        let b = frames[0].unwrap();
        println!("hold {secs} s: box {:.0}×{:.0}, point x {:.1}", b[4] - b[2], b[5] - b[3], b[0]);
    }
}
