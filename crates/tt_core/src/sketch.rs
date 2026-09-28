//! Motion sketch (DESIGN §8): turn a recorded pointer gesture into, per
//! frame, a *point* on the followed subject (the lag-compensated smoothed
//! path — what trackers and targets consume) and a *region* around it
//! (`[left, top, right, bottom]`, grown by jiggle and by motion — what views
//! frame). Output channels: `[x, y, left, top, right, bottom]`.
//!
//! Data:
//! - A [`Capture`] entity's [`Output`] signal is its raw pointer stream,
//!   indexed by *sample number* with channels `[t, x, y]` (t in wall seconds
//!   from the capture start; x, y in source pixels). Kept forever: every
//!   parameter below can be re-tuned after the fact.
//! - Its [`ClockMap`] records (wall time, playhead, playing) once per UI frame,
//!   so slowed playback, pauses (hold-to-simulate), steps and scrubs all map
//!   back to video frames; a later pass over a frame wins.
//!
//! Pipeline (the `sketch` operator; hand noise lives in real time, so the
//! hand stages run on a uniform wall-time grid):
//! 1. resample the samples onto a 240 Hz grid;
//! 2. dead zone (lazy mouse, run both ways so it adds no lag): tremor
//!    smaller than `dead_zone` is ignored;
//! 3. One Euro smoothing run forward then backward (zero-phase: no lag, so no
//!    end-of-stroke catch-up is needed) — `steadiness` = min cutoff,
//!    `responsiveness` = beta, tuned in that order;
//! 4. jiggle → size: RMS spread of the raw hand around the smoothed path over
//!    `jiggle_window`, × 2.2 × `gain` + `pad`, at least `min_half`;
//! 5. to video frames: each frame takes the path at the wall time it was
//!    shown, shifted by `lag` (the hand trails what it follows); a paused
//!    stretch maps to one frame and keeps the state at the end of the hold;
//! 6. union of the boxes over [f − before, f + after] (motion blur of the
//!    hand), then light smoothing of the point and of the region's extents
//!    around it. Frames shown in the last `lag` before the release get no
//!    result: the hand never reached them.

use std::ops::Range;

use bevy_ecs::prelude::*;
use bevy_reflect::Reflect;

use crate::app::{AppBuilder, Module};
use crate::meta::Class;
use crate::op::{EvalCtx, Footprint, OperatorKind, Output};
use crate::signal::Signal;
use crate::time::FrameIndex;

/// Channels of a capture's raw stream: `[t, x, y]`.
pub const STREAM_CHANNELS: usize = 3;
/// Channels of a sketch result: `[x, y, left, top, right, bottom]`.
pub const BOX_CHANNELS: usize = 6;
const GRID_HZ: f64 = 240.0;
/// Dead-zone variant (see `dead_zone_symmetric`); chosen by the sweep in tests/sketch.rs.
const SYMMETRIC_DEAD_ZONE: bool = false;

/// A recorded pointer gesture. The raw samples are its [`Output`] signal.
#[derive(Component, Reflect, Clone, Debug, Default)]
#[reflect(Component)]
pub struct Capture {
    /// Playback rate while recording (1 = real time).
    pub rate: f64,
    /// Number of raw samples in the stream.
    pub samples: u32,
}

/// Wall time ↔ playhead during a capture, one entry per UI frame.
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component)]
pub struct ClockMap {
    /// Seconds from the capture start.
    pub t: Vec<f64>,
    /// Continuous playhead (frames).
    pub frame: Vec<f64>,
    pub playing: Vec<bool>,
}

impl ClockMap {
    pub fn push(&mut self, t: f64, frame: f64, playing: bool) {
        self.t.push(t);
        self.frame.push(frame);
        self.playing.push(playing);
    }

    /// For each visited video frame, the wall time at which it represents the
    /// capture. Returns `(first_frame, times)`, NaN = not visited.
    ///
    /// - While playing, a frame takes the moment its *centre* was on screen
    ///   (only the clock segment that crosses the centre assigns it).
    /// - A hold (paused) takes the end of the hold, and outranks the playback
    ///   that immediately follows on the same frame: releasing a freeze must
    ///   not overwrite the frame you just sculpted.
    /// - A jump (step, seek, scrub) starts a new *pass*; later passes win.
    pub fn frame_times(&self) -> Option<(FrameIndex, Vec<f64>)> {
        if self.t.is_empty() {
            return None;
        }
        let lo = self.frame.iter().fold(f64::INFINITY, |a, b| a.min(*b)).floor() as FrameIndex;
        let hi = self.frame.iter().fold(f64::NEG_INFINITY, |a, b| a.max(*b)).floor() as FrameIndex;
        let n = (hi - lo + 1) as usize;
        // (time, pass, rank): rank 0 = start/jump landing, 1 = playback crossing, 2 = hold.
        let mut best: Vec<Option<(f64, u32, u8)>> = vec![None; n];
        let mut assign = |f: FrameIndex, t: f64, pass: u32, rank: u8| {
            if !(lo..=hi).contains(&f) {
                return;
            }
            let slot = &mut best[(f - lo) as usize];
            let wins = match slot {
                None => true,
                Some((_, p, r)) => pass > *p || (pass == *p && rank >= *r),
            };
            if wins {
                *slot = Some((t, pass, rank));
            }
        };
        let mut pass = 0u32;
        assign(self.frame[0].floor() as FrameIndex, self.t[0], pass, 0);
        for i in 0..self.t.len() - 1 {
            let (t0, t1) = (self.t[i], self.t[i + 1]);
            let (f0, f1) = (self.frame[i], self.frame[i + 1]);
            let span = f1 - f0;
            let continuous = self.playing[i] && span >= 0.0 && span <= (t1 - t0) * 480.0 + 1.0;
            if continuous {
                if span > 0.0 {
                    for f in (f0.floor() as FrameIndex)..=(f1.floor() as FrameIndex) {
                        let c = f as f64 + 0.5;
                        if c >= f0 && c < f1 {
                            assign(f, t0 + (c - f0) / span * (t1 - t0), pass, 1);
                        }
                    }
                }
            } else if f0.floor() == f1.floor() && !self.playing[i] {
                // Holding on one frame (hold-to-simulate): the end of the hold counts.
                assign(f1.floor() as FrameIndex, t1, pass, 2);
            } else {
                // A jump: a new pass, landing on the frame shown at t1.
                pass += 1;
                assign(f1.floor() as FrameIndex, t1, pass, 0);
            }
        }
        Some((lo, best.into_iter().map(|b| b.map_or(f64::NAN, |(t, _, _)| t)).collect()))
    }
}

/// Tuning of the sketch pipeline (all re-tunable after capture).
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component)]
pub struct SketchParams {
    /// Seconds (real time) the hand trails the subject.
    pub lag: f32,
    /// One Euro minimum cutoff (Hz): lower = steadier when the hand is slow.
    pub steadiness: f32,
    /// One Euro beta: higher = follows fast moves with less lag.
    pub responsiveness: f32,
    /// Hand tremor ignored below this many source pixels.
    pub dead_zone: f32,
    /// Window (s, real time) over which jiggle is measured.
    pub jiggle_window: f32,
    /// Jiggle → box size multiplier.
    pub gain: f32,
    /// Pixels added to every half-size.
    pub pad: f32,
    /// Smallest half-size (px).
    pub min_half: f32,
    /// The box includes motion from this long before … (s, video time)
    pub before: f32,
    /// … to this long after each frame (s, video time).
    pub after: f32,
    /// Final smoothing of the box position and size (s, video time).
    pub smooth_position: f32,
    pub smooth_size: f32,
}

impl Default for SketchParams {
    fn default() -> Self {
        Self {
            lag: 0.25,
            steadiness: 1.0,
            responsiveness: 0.005,
            dead_zone: 1.5,
            jiggle_window: 0.25,
            gain: 1.0,
            pad: 12.0,
            min_half: 16.0,
            before: 0.1,
            after: 0.15,
            smooth_position: 0.03,
            smooth_size: 0.2,
        }
    }
}

impl SketchParams {
    /// Named starting points over the numbers (as in Premiere's Auto Reframe);
    /// every value stays editable afterwards.
    pub const PRESETS: [&'static str; 3] = ["Tight", "Default", "Loose"];

    pub fn preset(name: &str) -> Option<Self> {
        let d = Self::default();
        Some(match name {
            // Follows closely with a snug region: steady subjects, careful hands.
            "Tight" => Self {
                steadiness: 2.0,
                responsiveness: 0.02,
                dead_zone: 1.0,
                jiggle_window: 0.2,
                gain: 0.8,
                pad: 6.0,
                min_half: 12.0,
                before: 0.05,
                after: 0.08,
                smooth_size: 0.12,
                ..d
            },
            "Default" => d,
            // Steadier path and a generous region: erratic subjects, quick passes.
            "Loose" => Self {
                steadiness: 0.5,
                responsiveness: 0.005,
                dead_zone: 2.5,
                jiggle_window: 0.35,
                gain: 1.4,
                pad: 24.0,
                min_half: 28.0,
                before: 0.15,
                after: 0.3,
                smooth_size: 0.35,
                ..d
            },
            _ => return None,
        })
    }
}

// ---- building blocks (pure) --------------------------------------------------------------

/// Samples `(t, x, y)` resampled onto a uniform grid (linear interpolation).
fn to_grid(samples: &[[f64; 3]], hz: f64) -> (f64, Vec<[f64; 2]>) {
    let (t0, t1) = (samples[0][0], samples[samples.len() - 1][0]);
    let n = ((t1 - t0) * hz).floor() as usize + 1;
    let mut out = Vec::with_capacity(n);
    let mut j = 0;
    for i in 0..n {
        let t = t0 + i as f64 / hz;
        while j + 1 < samples.len() && samples[j + 1][0] <= t {
            j += 1;
        }
        let a = samples[j];
        let b = samples[(j + 1).min(samples.len() - 1)];
        let u = if b[0] > a[0] { ((t - a[0]) / (b[0] - a[0])).clamp(0.0, 1.0) } else { 0.0 };
        out.push([a[1] + (b[1] - a[1]) * u, a[2] + (b[2] - a[2]) * u]);
    }
    (t0, out)
}

/// Lazy mouse: the anchor follows only once the pointer is farther than `r`.
fn dead_zone(p: &[[f64; 2]], r: f64) -> Vec<[f64; 2]> {
    let mut anchor = p[0];
    p.iter()
        .map(|q| {
            let (dx, dy) = (q[0] - anchor[0], q[1] - anchor[1]);
            let d = (dx * dx + dy * dy).sqrt();
            if d > r {
                let k = 1.0 - r / d;
                anchor = [anchor[0] + dx * k, anchor[1] + dy * k];
            }
            anchor
        })
        .collect()
}

/// Dead zone run forward and backward, averaged: a one-way lazy mouse trails
/// the pointer by `r` whenever it moves (a bias the zero-phase smoothing can't
/// remove); the two directions' biases cancel, while tremor at rest is still
/// swallowed.
fn dead_zone_symmetric(p: &[[f64; 2]], r: f64) -> Vec<[f64; 2]> {
    let fwd = dead_zone(p, r);
    let rev: Vec<[f64; 2]> = p.iter().rev().copied().collect();
    let mut back = dead_zone(&rev, r);
    back.reverse();
    fwd.iter().zip(&back).map(|(a, b)| [(a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0]).collect()
}

/// One Euro filter (Casiez et al.) over a uniformly sampled 2D signal.
fn one_euro(p: &[[f64; 2]], hz: f64, min_cutoff: f64, beta: f64) -> Vec<[f64; 2]> {
    let alpha = |cutoff: f64| {
        let tau = 1.0 / (2.0 * std::f64::consts::PI * cutoff);
        1.0 / (1.0 + tau * hz)
    };
    let d_alpha = alpha(1.0);
    let mut out = Vec::with_capacity(p.len());
    let mut prev = p[0];
    let mut dprev = [0.0, 0.0];
    for q in p {
        let d = [(q[0] - prev[0]) * hz, (q[1] - prev[1]) * hz];
        let dhat = [dprev[0] + d_alpha * (d[0] - dprev[0]), dprev[1] + d_alpha * (d[1] - dprev[1])];
        let speed = (dhat[0] * dhat[0] + dhat[1] * dhat[1]).sqrt();
        let a = alpha(min_cutoff + beta * speed);
        let x = [prev[0] + a * (q[0] - prev[0]), prev[1] + a * (q[1] - prev[1])];
        out.push(x);
        prev = x;
        dprev = dhat;
    }
    out
}

/// One Euro forward, then backward over the result: zero-phase (no lag).
/// Both ends are padded with an odd reflection first (SciPy `filtfilt`'s
/// default), so the ends continue their trend instead of starting from a
/// lagged state.
fn one_euro_zero_phase(p: &[[f64; 2]], hz: f64, min_cutoff: f64, beta: f64) -> Vec<[f64; 2]> {
    let pad = ((0.5 * hz) as usize).min(p.len() - 1);
    let (a, z) = (p[0], p[p.len() - 1]);
    let mut padded: Vec<[f64; 2]> = Vec::with_capacity(p.len() + 2 * pad);
    padded.extend((1..=pad).rev().map(|k| [2.0 * a[0] - p[k][0], 2.0 * a[1] - p[k][1]]));
    padded.extend_from_slice(p);
    padded.extend((1..=pad).map(|k| {
        let q = p[p.len() - 1 - k];
        [2.0 * z[0] - q[0], 2.0 * z[1] - q[1]]
    }));
    let fwd = one_euro(&padded, hz, min_cutoff, beta);
    let rev: Vec<[f64; 2]> = fwd.into_iter().rev().collect();
    let mut back = one_euro(&rev, hz, min_cutoff, beta);
    back.reverse();
    back.drain(..pad);
    back.truncate(p.len());
    back
}

/// Approximate Gaussian blur (three box passes, O(n)), edges clamped.
pub fn gauss(v: &[f64], sigma: f64) -> Vec<f64> {
    if sigma < 0.5 || v.len() < 2 {
        return v.to_vec();
    }
    // Box width for three passes approximating sigma.
    let w = ((12.0 * sigma * sigma / 3.0 + 1.0).sqrt()).round().max(1.0) as usize;
    let r = w / 2;
    let mut a = v.to_vec();
    let mut b = vec![0.0; v.len()];
    for _ in 0..3 {
        let n = a.len();
        let mut prefix = vec![0.0; n + 1];
        for i in 0..n {
            prefix[i + 1] = prefix[i] + a[i];
        }
        for (i, out) in b.iter_mut().enumerate() {
            let lo = i.saturating_sub(r);
            let hi = (i + r + 1).min(n);
            *out = (prefix[hi] - prefix[lo]) / (hi - lo) as f64;
        }
        std::mem::swap(&mut a, &mut b);
    }
    a
}

/// [`gauss`] for a signal with a trend (a moving point): the ends are padded
/// with an odd reflection so a run's first and last values aren't pulled
/// toward their only-one-sided neighbours.
fn gauss_trend(v: &[f64], sigma: f64) -> Vec<f64> {
    if sigma < 0.5 || v.len() < 2 {
        return v.to_vec();
    }
    let pad = ((3.0 * sigma).ceil() as usize).min(v.len() - 1);
    let (a, z) = (v[0], v[v.len() - 1]);
    let mut padded = Vec::with_capacity(v.len() + 2 * pad);
    padded.extend((1..=pad).rev().map(|k| 2.0 * a - v[k]));
    padded.extend_from_slice(v);
    padded.extend((1..=pad).map(|k| 2.0 * z - v[v.len() - 1 - k]));
    let mut out = gauss(&padded, sigma);
    out.drain(..pad);
    out.truncate(v.len());
    out
}

/// Linear interpolation into a grid series at time `t` (clamped).
fn at(series: &[[f64; 2]], t0: f64, hz: f64, t: f64) -> [f64; 2] {
    let x = ((t - t0) * hz).clamp(0.0, (series.len() - 1) as f64);
    let i = x.floor() as usize;
    let j = (i + 1).min(series.len() - 1);
    let u = x - i as f64;
    [series[i][0] + (series[j][0] - series[i][0]) * u, series[i][1] + (series[j][1] - series[i][1]) * u]
}

/// The whole pipeline. `samples` are `(t, x, y)` sorted by t. Returns
/// `(first_frame, frames)` with `[x, y, left, top, right, bottom]` per frame;
/// `None` entries are frames the capture didn't visit.
pub fn sketch_boxes(samples: &[[f64; 3]], clock: &ClockMap, p: &SketchParams, fps: f64) -> Option<(FrameIndex, Vec<Option<[f64; 6]>>)> {
    if samples.len() < 2 {
        return None;
    }
    let hz = GRID_HZ;
    let (t0, raw) = to_grid(samples, hz);
    let steady = if SYMMETRIC_DEAD_ZONE { dead_zone_symmetric(&raw, p.dead_zone as f64) } else { dead_zone(&raw, p.dead_zone as f64) };
    let center = one_euro_zero_phase(&steady, hz, p.steadiness.max(0.01) as f64, p.responsiveness.max(0.0) as f64);
    let sigma = p.jiggle_window as f64 * hz;
    let var_x = gauss(&raw.iter().zip(&center).map(|(r, c)| (r[0] - c[0]).powi(2)).collect::<Vec<_>>(), sigma);
    let var_y = gauss(&raw.iter().zip(&center).map(|(r, c)| (r[1] - c[1]).powi(2)).collect::<Vec<_>>(), sigma);
    let half: Vec<[f64; 2]> = var_x
        .iter()
        .zip(&var_y)
        .map(|(vx, vy)| {
            let h = |v: f64| (p.gain as f64 * 2.2 * v.sqrt() + p.pad as f64).max(p.min_half as f64);
            [h(*vx), h(*vy)]
        })
        .collect();

    // To video frames: the path at the moment each frame was shown, plus the lag.
    let (first, times) = clock.frame_times()?;
    // A frame shown in the last `lag` before the release was never reached by the hand: no result.
    let lag = p.lag as f64;
    let reached = samples[0][0]..=samples[samples.len() - 1][0];
    let raw_frames: Vec<Option<([f64; 2], [f64; 2])>> = times
        .iter()
        .map(|t| reached.contains(&(t + lag)).then(|| (at(&center, t0, hz, t + lag), at(&half, t0, hz, t + lag))))
        .collect();

    // Region: union of the jiggle boxes over [f − before, f + after] (video time).
    let (before, after) = ((p.before as f64 * fps).round() as usize, (p.after as f64 * fps).round() as usize);
    let n = raw_frames.len();
    let mut frames: Vec<Option<[f64; 6]>> = vec![None; n];
    for (i, slot) in frames.iter_mut().enumerate() {
        let Some((point, _)) = raw_frames[i] else { continue };
        let (mut x0, mut y0, mut x1, mut y1) = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        for (c, h) in raw_frames[i.saturating_sub(before)..(i + after + 1).min(n)].iter().flatten() {
            x0 = x0.min(c[0] - h[0]);
            x1 = x1.max(c[0] + h[0]);
            y0 = y0.min(c[1] - h[1]);
            y1 = y1.max(c[1] + h[1]);
        }
        *slot = Some([point[0], point[1], x0, y0, x1, y1]);
    }
    // Light smoothing within each visited run (no bleeding across gaps).
    let mut out = frames.clone();
    let mut i = 0;
    while i < n {
        if frames[i].is_none() {
            i += 1;
            continue;
        }
        let start = i;
        while i < n && frames[i].is_some() {
            i += 1;
        }
        // The region is smoothed as extents around the point, so smoothing its
        // size never drags it behind a moving subject.
        let run: Vec<[f64; 6]> = frames[start..i].iter().map(|b| b.unwrap()).collect();
        let extents: Vec<[f64; 4]> = run.iter().map(|b| [b[0] - b[2], b[1] - b[3], b[4] - b[0], b[5] - b[1]]).collect();
        let (sp, ss) = (p.smooth_position as f64 * fps, p.smooth_size as f64 * fps);
        let point = |k: usize| gauss_trend(&run.iter().map(|b| b[k]).collect::<Vec<_>>(), sp);
        let extent = |k: usize| gauss(&extents.iter().map(|e| e[k]).collect::<Vec<_>>(), ss);
        let (x, y) = (point(0), point(1));
        let e = [extent(0), extent(1), extent(2), extent(3)];
        for k in 0..run.len() {
            out[start + k] = Some([x[k], y[k], x[k] - e[0][k], y[k] - e[1][k], x[k] + e[2][k], y[k] + e[3][k]]);
        }
    }
    Some((first, out))
}

/// Read a capture's raw stream (`[t, x, y]` per sample index) into a Vec.
pub fn read_stream(stream: &Signal, count: u32) -> Vec<[f64; 3]> {
    (0..count as FrameIndex)
        .filter_map(|i| stream.get(i).map(|v| [v[0] as f64, v[1] as f64, v[2] as f64]))
        .collect()
}

// ---- the operator ------------------------------------------------------------------------

/// `sketch`: capture stream (input "capture") → per-frame box.
pub struct SketchKind;

impl OperatorKind for SketchKind {
    fn name(&self) -> &'static str {
        "sketch"
    }

    fn channels(&self) -> usize {
        BOX_CHANNELS
    }

    fn footprint(&self, _: EntityRef<'_>) -> Footprint {
        Footprint::Global
    }

    fn evaluate(&self, ctx: &EvalCtx<'_>, range: Range<FrameIndex>, out: &mut Signal) -> anyhow::Result<()> {
        let params = ctx.params::<SketchParams>().cloned().unwrap_or_default();
        let fps = ctx.world.get_resource::<crate::transport::Transport>().map_or(60.0, |t| t.fps.as_f64());
        out.clear(range.clone());
        let Some(capture) = ctx.input_entity("capture") else { return Ok(()) };
        let (Some(info), Some(clock), Some(stream)) =
            (ctx.world.get::<Capture>(capture), ctx.world.get::<ClockMap>(capture), ctx.input("capture"))
        else {
            return Ok(());
        };
        let samples = read_stream(stream, info.samples);
        let Some((first, boxes)) = sketch_boxes(&samples, clock, &params, fps) else { return Ok(()) };
        for (i, b) in boxes.iter().enumerate() {
            let f = first + i as FrameIndex;
            if let (Some(b), true) = (b, range.contains(&f)) {
                out.set(f, &b.map(|v| v as f32));
            }
        }
        Ok(())
    }
}

pub struct SketchModule;

impl Module for SketchModule {
    fn build(&self, app: &mut AppBuilder) {
        app.component::<Capture>(Class::Document)
            .component::<ClockMap>(Class::Document)
            .operator(SketchKind)
            .operator_params::<SketchParams>();
    }
}

/// A capture's stream signal id (its Output).
pub fn stream_of(world: &World, capture: Entity) -> Option<crate::signal::SignalId> {
    world.get::<Output>(capture).map(|o| o.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_times_playback_pause_and_rescrub() {
        let mut c = ClockMap::default();
        // Play 0 → 3 over 0.05 s (centres of frames 0, 1, 2 are crossed), hold on
        // frame 3 for 1 s, play on through frame 3, then jump back to frame 1.
        c.push(0.0, 0.0, true);
        c.push(0.05, 3.0, true);
        c.push(0.5, 3.0, false);
        c.push(1.05, 3.0, false);
        c.push(1.10, 4.2, true);
        c.push(1.20, 1.2, false);
        let (first, t) = c.frame_times().unwrap();
        assert_eq!(first, 0);
        assert!((t[0] - 0.05 * 0.5 / 3.0).abs() < 1e-9, "frame 0 at the moment its centre was shown: {}", t[0]);
        assert!((t[2] - 0.05 * 2.5 / 3.0).abs() < 1e-9);
        assert!((t[3] - 1.05).abs() < 1e-9, "the hold keeps frame 3 even though playback then crossed its centre");
        assert!((t[1] - 1.20).abs() < 1e-9, "a later pass (after a jump) wins");
    }

    #[test]
    fn gauss_preserves_constants_and_smooths_steps() {
        let flat = vec![5.0; 50];
        assert!(gauss(&flat, 4.0).iter().all(|v| (v - 5.0).abs() < 1e-9));
        let step: Vec<f64> = (0..100).map(|i| if i < 50 { 0.0 } else { 1.0 }).collect();
        let s = gauss(&step, 5.0);
        assert!(s[49] > 0.2 && s[49] < 0.8 && s[0] < 1e-9 && s[99] > 1.0 - 1e-9);
    }

    #[test]
    fn dead_zone_ignores_tremor() {
        let p: Vec<[f64; 2]> = (0..100).map(|i| [((i % 2) as f64) * 1.0, 0.0]).collect();
        let d = dead_zone(&p, 1.5);
        assert!(d.iter().all(|q| q[0] == 0.0), "1 px tremor inside a 1.5 px dead zone never moves");
    }
}
