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
    // Skip the first/last lag of the capture (the hand hadn't started / had stopped).
    let trim = (0.25 * 0.25 * FPS) as usize + 2;
    for (i, f) in frames.iter().enumerate().skip(trim).take(frames.len().saturating_sub(2 * trim)) {
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
    assert!(total > 250, "most of the 300 covered frames have results");
    assert!(median < 1.5, "median point error {median:.2} px (measured 1.11 when set)");
    assert!(p95 < 3.0, "p95 point error {p95:.2} px (measured 1.97 when set)");
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
