//! Motion sketch (DESIGN §8): turn a recorded pointer gesture into, per
//! frame, a *point* on the followed subject (the lag-compensated smoothed
//! path — what trackers and targets consume) and a *region* around it
//! (`[left, top, right, bottom]`, grown by jiggle and by motion — what views
//! frame). Output channels: `[x, y, left, top, right, bottom]`.
//!
//! Data:
//! - A *sketch* is a `sketch` operator whose inputs are its *strokes*, in
//!   layer order. A stroke is a [`Capture`] entity (one press → release) with
//!   a [`Stroke`] component saying how it lies over the strokes before it:
//!   replacing the frames it visited and pulling neighbouring frames along
//!   with a falloff ([`layer_over`]).
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
//! 4. jiggle → size: RMS spread of the raw hand around a slow reference (the
//!    steadiness cutoff alone, so it doesn't chase the jiggle) over
//!    `jiggle_window`, × 2.2 × `gain` + `pad`, at least `min_half`;
//! 5. to video frames: each frame takes the path at the wall time it was
//!    shown, shifted by `lag` (the hand trails what it follows); a paused
//!    stretch maps to one frame and keeps the state at the end of the hold
//!    (no lag shift: the hand has settled);
//!    Light smoothing of the point and of the jiggle box's extents around
//!    it. Frames played in the last `lag` before the release get no result:
//!    the hand never reached them;
//! 6. after the sketch's strokes are layered: each frame's box grows to
//!    cover the boxes over [f − before, f + after] (the motion blur of the
//!    region), so an edit made while paused reads like a recorded frame.

use std::collections::BTreeMap;
use std::ops::Range;

use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::prelude::*;
use bevy_reflect::Reflect;

use crate::app::{AppBuilder, Module, Set};
use crate::meta::Class;
use crate::op::{EvalCtx, Footprint, Inputs, Invalidations, Operator, OperatorKind, Output};
use crate::signal::{Signal, SignalStore};
use crate::time::FrameIndex;
use crate::transport::Transport;

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

    /// For each visited video frame, when it was on screen as far as the
    /// capture is concerned. Returns `(first_frame, shown)`, `None` = not visited.
    ///
    /// - While playing, a frame takes the moment its *centre* was on screen
    ///   (only the clock segment that crosses the centre assigns it).
    /// - A hold (paused) is a [`Shown::Held`] span, and outranks the playback
    ///   that immediately follows on the same frame: resuming playback must
    ///   not overwrite the frame you just sculpted.
    /// - A jump (step, seek, scrub) starts a new *pass*; later passes win.
    pub fn frame_times(&self) -> Option<(FrameIndex, Vec<Option<Shown>>)> {
        if self.t.is_empty() {
            return None;
        }
        let lo = self.frame.iter().fold(f64::INFINITY, |a, b| a.min(*b)).floor() as FrameIndex;
        let hi = self.frame.iter().fold(f64::NEG_INFINITY, |a, b| a.max(*b)).floor() as FrameIndex;
        let n = (hi - lo + 1) as usize;
        // (shown, pass, rank): rank 0 = start/jump landing, 1 = playback crossing, 2 = hold.
        let mut best: Vec<Option<(Shown, u32, u8)>> = vec![None; n];
        let mut assign = |f: FrameIndex, shown: Shown, pass: u32, rank: u8| {
            if !(lo..=hi).contains(&f) {
                return;
            }
            let slot = &mut best[(f - lo) as usize];
            let wins = match slot {
                None => true,
                Some((_, p, r)) => pass > *p || (pass == *p && rank >= *r),
            };
            if wins {
                *slot = Some((shown, pass, rank));
            }
        };
        let mut pass = 0u32;
        // The frame being held and since when.
        let mut hold: Option<(FrameIndex, f64)> = None;
        assign(self.frame[0].floor() as FrameIndex, Shown::At(self.t[0]), pass, 0);
        for i in 0..self.t.len() - 1 {
            let (t0, t1) = (self.t[i], self.t[i + 1]);
            let (f0, f1) = (self.frame[i], self.frame[i + 1]);
            let span = f1 - f0;
            let continuous = self.playing[i] && span >= 0.0 && span <= (t1 - t0) * 480.0 + 1.0;
            if f0.floor() == f1.floor() && !self.playing[i] {
                // Holding on one frame (hold-to-simulate).
                let f = f1.floor() as FrameIndex;
                let from = match hold {
                    Some((g, from)) if g == f => from,
                    _ => t0,
                };
                hold = Some((f, from));
                assign(f, Shown::Held { from, to: t1 }, pass, 2);
                continue;
            }
            hold = None;
            if continuous {
                if span > 0.0 {
                    for f in (f0.floor() as FrameIndex)..=(f1.floor() as FrameIndex) {
                        let c = f as f64 + 0.5;
                        if c >= f0 && c < f1 {
                            assign(f, Shown::At(t0 + (c - f0) / span * (t1 - t0)), pass, 1);
                        }
                    }
                }
            } else {
                // A jump: a new pass, landing on the frame shown at t1.
                pass += 1;
                assign(f1.floor() as FrameIndex, Shown::At(t1), pass, 0);
            }
        }
        Some((lo, best.into_iter().map(|b| b.map(|(s, _, _)| s)).collect()))
    }

    /// The frames the capture visited (min..=max playhead), cheap: for lanes.
    pub fn frame_hull(&self) -> Option<(FrameIndex, FrameIndex)> {
        let lo = self.frame.iter().copied().reduce(f64::min)?;
        let hi = self.frame.iter().copied().reduce(f64::max)?;
        Some((lo.floor() as FrameIndex, hi.floor() as FrameIndex))
    }
}

/// When a visited frame was on screen during a capture.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Shown {
    /// Played through (its centre was on screen at this time), or landed on by a jump.
    At(f64),
    /// Held on screen (paused) from `from` to `to`.
    Held { from: f64, to: f64 },
}

/// A capture drawn inside a view: that view's mapping ([`crate::view::SpaceMap`])
/// for every frame the capture touched, as `[a, bx, by]` per frame. The hand
/// pipeline runs in the view's pixels (where the hand moved); its results
/// reach the source through these, so re-tuning the view later never moves
/// the stroke.
#[derive(Component, Reflect, Clone, Copy, Debug)]
#[reflect(Component)]
pub struct Through(pub crate::signal::SignalId);

/// How a capture is laid over the sketch it belongs to (a *stroke*).
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component)]
pub struct Stroke {
    /// Seconds (video time) over which frames beside the stroke are pulled
    /// along with it, fading out (proportional editing in time).
    pub falloff: f32,
    /// How much the stroke moves the point: 0 = no effect, 1 = it replaces what was there.
    pub influence: f32,
    /// How much the stroke's jiggle sets the region's size: 0 keeps the size
    /// that was there (a "move only" stroke, Ctrl at the press), 1 replaces it.
    pub size: f32,
    /// Makes the stroke's region bigger (> 1) or smaller around its point (the
    /// wheel while holding, by default).
    #[reflect(default = "one")]
    pub scale: f32,
}

fn one() -> f32 {
    1.0
}

impl Default for Stroke {
    fn default() -> Self {
        Self { falloff: 0.2, influence: 1.0, size: 1.0, scale: 1.0 }
    }
}

impl Stroke {
    /// `p` with the stroke's region size multiplied by `scale` (the jiggle
    /// size's gain and pad; the floor only when growing), so a smaller size
    /// never goes under `min_half`.
    pub fn sized(&self, p: &SketchParams) -> SketchParams {
        let k = self.scale.max(0.01);
        SketchParams { gain: p.gain * k, pad: p.pad * k, min_half: p.min_half * k.max(1.0), ..p.clone() }
    }
}

/// Tuning of the sketch pipeline (all re-tunable after capture).
#[derive(Component, Reflect, Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[reflect(Component)]
#[serde(default)]
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
pub(crate) fn gauss_trend(v: &[f64], sigma: f64) -> Vec<f64> {
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

/// The whole pipeline for one stroke on its own: [`stroke_frames`], then
/// the region's motion union ([`union_at`]). `samples` are `(t, x, y)` sorted
/// by t. Returns `(first_frame, frames)` with `[x, y, left, top, right,
/// bottom]` per frame; `None` entries are frames the capture didn't visit.
pub fn sketch_boxes(samples: &[[f64; 3]], clock: &ClockMap, p: &SketchParams, fps: f64) -> Option<Boxes> {
    let (first, frames) = stroke_frames(samples, clock, p, fps)?;
    let (before, after) = union_reach(p, fps);
    let get = |f: FrameIndex| frames.get(usize::try_from(f - first).ok()?).copied().flatten();
    let out = (0..frames.len()).map(|i| union_at(get, first + i as FrameIndex, before, after)).collect();
    Some((first, out))
}

/// One stroke, steps 1–5 of the pipeline plus light smoothing: per visited
/// frame the point and its *jiggle box* (the region before the motion
/// union, which is applied to the whole sketch after its strokes are
/// layered, [`union_at`]).
///
/// A played frame takes the hand at the moment it was shown plus `lag`, a
/// held (paused) frame the hand at the end of the hold; either way its box is
/// the smoothed point ± the jiggle size then, so the same jiggle reads as the
/// same size whether the video was playing or paused.
pub fn stroke_frames(samples: &[[f64; 3]], clock: &ClockMap, p: &SketchParams, fps: f64) -> Option<Boxes> {
    if samples.len() < 2 {
        return None;
    }
    let hz = GRID_HZ;
    let (t0, raw) = to_grid(samples, hz);
    let steady = if SYMMETRIC_DEAD_ZONE { dead_zone_symmetric(&raw, p.dead_zone as f64) } else { dead_zone(&raw, p.dead_zone as f64) };
    let center = one_euro_zero_phase(&steady, hz, p.steadiness.max(0.01) as f64, p.responsiveness.max(0.0) as f64);
    // Jiggle is measured against a slow, non-adaptive reference (the
    // steadiness cutoff alone): the point's path speeds up with the hand and
    // would follow part of a jiggle, by an amount that differs between
    // playing and paused. Against this reference the same jiggle reads as the
    // same size in both.
    let reference = one_euro_zero_phase(&raw, hz, p.steadiness.max(0.01) as f64, 0.0);
    let sigma = p.jiggle_window as f64 * hz;
    let var_x = gauss(&raw.iter().zip(&reference).map(|(r, c)| (r[0] - c[0]).powi(2)).collect::<Vec<_>>(), sigma);
    let var_y = gauss(&raw.iter().zip(&reference).map(|(r, c)| (r[1] - c[1]).powi(2)).collect::<Vec<_>>(), sigma);
    let half: Vec<[f64; 2]> = var_x
        .iter()
        .zip(&var_y)
        .map(|(vx, vy)| {
            let h = |v: f64| (p.gain as f64 * 2.2 * v.sqrt() + p.pad as f64).max(p.min_half as f64);
            [h(*vx), h(*vy)]
        })
        .collect();
    let jiggle_box = |t: f64| -> [f64; 6] {
        let (c, h) = (at(&center, t0, hz, t), at(&half, t0, hz, t));
        [c[0], c[1], c[0] - h[0], c[1] - h[1], c[0] + h[0], c[1] + h[1]]
    };

    // To video frames: the path at the moment each frame was shown, plus the lag.
    // - A played frame shown in the last `lag` before the release was never
    //   reached by the hand: no result.
    // - A held frame takes the end of the hold (the hand has settled on it),
    //   but at least `lag` after the hold began (it had to get there first).
    let (first, shown) = clock.frame_times()?;
    let lag = p.lag as f64;
    let (start, end) = (samples[0][0], samples[samples.len() - 1][0]);
    let frames: Vec<Option<[f64; 6]>> = shown
        .iter()
        .map(|s| match (*s)? {
            Shown::At(t) => {
                let t = t + lag;
                (start..=end).contains(&t).then(|| jiggle_box(t))
            }
            Shown::Held { from, to } => Some(jiggle_box(to.max(from + lag).min(end))),
        })
        .collect();

    // Light smoothing within each visited run (no bleeding across gaps). The
    // box is smoothed as extents around the point, so smoothing its size never
    // drags it behind a moving subject.
    let n = frames.len();
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
        let run: Vec<[f64; 6]> = frames[start..i].iter().map(|b| b.unwrap()).collect();
        let ext: Vec<[f64; 4]> = run.iter().map(extents).collect();
        let (sp, ss) = (p.smooth_position as f64 * fps, p.smooth_size as f64 * fps);
        let point = |k: usize| gauss_trend(&run.iter().map(|b| b[k]).collect::<Vec<_>>(), sp);
        let extent = |k: usize| gauss(&ext.iter().map(|e| e[k]).collect::<Vec<_>>(), ss);
        let (x, y) = (point(0), point(1));
        let e = [extent(0), extent(1), extent(2), extent(3)];
        for k in 0..run.len() {
            out[start + k] = Some([x[k], y[k], x[k] - e[0][k], y[k] - e[1][k], x[k] + e[2][k], y[k] + e[3][k]]);
        }
    }
    Some((first, out))
}

/// A `Through` value `[a, bx, by]` as a mapping.
pub fn through_map(m: &[f32]) -> crate::view::SpaceMap {
    crate::view::SpaceMap { a: m[0] as f64, b: [m[1] as f64, m[2] as f64], canvas: [0.0, 0.0] }
}

/// The home view's framing for frames `first..`, looked up once (the motion
/// union reads each frame's framing many times). Empty for the source.
pub struct HomeMaps {
    first: FrameIndex,
    maps: Vec<crate::view::SpaceMap>,
}

impl HomeMaps {
    pub fn new(world: &World, home: Option<Entity>, frames: Range<FrameIndex>) -> Self {
        let maps = match home {
            Some(h) => frames.clone().map(|f| crate::view::map_at(world, Some(h), f)).collect(),
            None => Vec::new(),
        };
        Self { first: frames.start, maps }
    }

    fn at(&self, f: FrameIndex) -> Option<crate::view::SpaceMap> {
        self.maps.get(usize::try_from(f - self.first).ok()?).copied()
    }

    /// [`union_at`] with the motion measured as it looked in the sketch's home
    /// view: each neighbouring frame's box goes through that view (as framed at
    /// its own frame) and back out as framed at `f`. In a stabilized view the
    /// subject barely moves, so a sketch drawn there stays tight. (For the
    /// source, the plain union.)
    pub fn union(&self, get: impl Fn(FrameIndex) -> Option<[f64; 6]>, f: FrameIndex, before: FrameIndex, after: FrameIndex) -> Option<[f64; 6]> {
        let Some(at_f) = self.at(f) else { return union_at(get, f, before, after) };
        union_at(
            |g| {
                let v = get(g)?;
                Some(match self.at(g) {
                    Some(at_g) if g != f => at_f.box_to_source(at_g.box_from_source(v)),
                    _ => v,
                })
            },
            f,
            before,
            after,
        )
    }
}

/// The motion union's reach in frames: `(before, after)`.
pub fn union_reach(p: &SketchParams, fps: f64) -> (FrameIndex, FrameIndex) {
    ((p.before as f64 * fps).round() as FrameIndex, (p.after as f64 * fps).round() as FrameIndex)
}

/// The region at `f`: its box grown to cover the boxes of every frame in
/// `[f − before, f + after]` (the motion blur of the region; step 6). The
/// point stays. `None` where `f` has no value.
pub fn union_at(get: impl Fn(FrameIndex) -> Option<[f64; 6]>, f: FrameIndex, before: FrameIndex, after: FrameIndex) -> Option<[f64; 6]> {
    let mut b = get(f)?;
    for g in (f - before)..=(f + after) {
        if let Some(v) = get(g).filter(|_| g != f) {
            b = [b[0], b[1], b[2].min(v[2]), b[3].min(v[3]), b[4].max(v[4]), b[5].max(v[5])];
        }
    }
    Some(b)
}

/// Read a capture's raw stream (`[t, x, y]` per sample index) into a Vec.
pub fn read_stream(stream: &Signal, count: u32) -> Vec<[f64; 3]> {
    (0..count as FrameIndex)
        .filter_map(|i| stream.get(i).map(|v| [v[0] as f64, v[1] as f64, v[2] as f64]))
        .collect()
}

// ---- layering strokes ------------------------------------------------------------------

/// Weight of a stroke edge `k` frames away, for a falloff of `radius` frames:
/// 1 at the edge, easing smoothly to 0 just past the radius (Blender's
/// "Smooth" proportional falloff).
pub fn falloff_weight(k: f64, radius: f64) -> f64 {
    if radius <= 0.0 {
        return if k <= 0.0 { 1.0 } else { 0.0 };
    }
    let x = (1.0 - k / (radius + 1.0)).clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

/// A stroke laid over the path so far (`base`):
/// - frames the stroke visited take its value: the point blended with `base`
///   by `stroke.influence`, the region's extents around the point by `stroke.size`;
/// - frames within `radius` of a visited run keep their own motion but are
///   moved by the run's edge offset, weighted by [`falloff_weight`] and
///   normalised where several edges reach (so the frames between two edits
///   with the same offset move by exactly that offset);
/// - where there is no path yet, a gap of at most `2 · radius` frames between
///   the stroke and another value is bridged linearly (blocking with holds).
///
/// `radius` is in frames. Returns `(first, values)` over the frames that may
/// change; `None` = unchanged.
pub fn layer_over(base: impl Fn(FrameIndex) -> Option<[f64; 6]>, first: FrameIndex, values: &[Option<[f64; 6]>], radius: f64, stroke: &Stroke) -> Boxes {
    let (influence, size) = (stroke.influence as f64, stroke.size as f64);
    let stroke = values;
    let reach = radius.max(0.0).ceil() as FrameIndex;
    let lo = first - 2 * reach;
    let n = stroke.len() + 4 * reach as usize;
    let hi = lo + n as FrameIndex;
    let idx = |f: FrameIndex| (f - lo) as usize;
    let base_at = |f: FrameIndex| if (lo..hi).contains(&f) { base(f) } else { None };
    let mut out: Vec<Option<[f64; 6]>> = vec![None; n];

    // Visited runs `(first, last, offset at first, offset at last)`; an offset
    // exists where there was a path to move.
    type Run = (FrameIndex, FrameIndex, Option<[f64; 6]>, Option<[f64; 6]>);
    let mut runs: Vec<Run> = Vec::new();
    let mut i = 0;
    while i < stroke.len() {
        if stroke[i].is_none() {
            i += 1;
            continue;
        }
        let a = i;
        while i < stroke.len() && let Some(v) = stroke[i] {
            let f = first + i as FrameIndex;
            out[idx(f)] = Some(match base_at(f) {
                Some(b) => {
                    let (x, y) = (b[0] + influence * (v[0] - b[0]), b[1] + influence * (v[1] - b[1]));
                    let (eb, ev) = (extents(&b), extents(&v));
                    let e: [f64; 4] = std::array::from_fn(|c| eb[c] + size * (ev[c] - eb[c]));
                    [x, y, x - e[0], y - e[1], x + e[2], y + e[3]]
                }
                None => v,
            });
            i += 1;
        }
        let (fa, fb) = (first + a as FrameIndex, first + i as FrameIndex - 1);
        let offset = |f: FrameIndex| -> Option<[f64; 6]> {
            let (b, v) = (base_at(f)?, out[idx(f)]?);
            Some(std::array::from_fn(|c| v[c] - b[c]))
        };
        runs.push((fa, fb, offset(fa), offset(fb)));
    }
    let visited = |f: FrameIndex| runs.iter().any(|(a, b, _, _)| (*a..=*b).contains(&f));

    // Falloff: frames with a path move with the nearby edges.
    for f in lo..hi {
        if visited(f) {
            continue;
        }
        let Some(b) = base_at(f) else { continue };
        let (mut sum_w, mut sum) = (0.0, [0.0; 6]);
        for (a, z, left, right) in &runs {
            let (k, d) = if f < *a { (a - f, left) } else { (f - z, right) };
            let w = falloff_weight(k as f64, radius);
            if let (true, Some(d)) = (w > 0.0, d) {
                sum_w += w;
                for c in 0..6 {
                    sum[c] += w * d[c];
                }
            }
        }
        if sum_w > 0.0 {
            let norm = sum_w.max(1.0);
            out[idx(f)] = Some(std::array::from_fn(|c| b[c] + sum[c] / norm));
        }
    }

    // Bridging: new territory between the stroke and the nearest other value, if short.
    let value = |f: FrameIndex| if (lo..hi).contains(&f) { out[idx(f)].or_else(|| base_at(f)) } else { None };
    let mut bridged = out.clone();
    for (a, z, _, _) in &runs {
        for (edge, dir) in [(*a, -1), (*z, 1)] {
            let Some(from) = value(edge) else { continue };
            if value(edge + dir).is_some() {
                continue; // no gap on this side
            }
            // At most 2·radius frames in between (the real radius, not its ceiling).
            let Some(g) = (2..=2 * reach + 1).map(|k| edge + dir * k).find(|f| value(*f).is_some()) else { continue };
            if ((g - edge).abs() - 1) as f64 > 2.0 * radius {
                continue;
            }
            let to = value(g).expect("found a value");
            let span = (g - edge).abs();
            for j in 1..span {
                let u = j as f64 / span as f64;
                bridged[idx(edge + dir * j)] = Some(std::array::from_fn(|c| from[c] + (to[c] - from[c]) * u));
            }
        }
    }
    (lo, bridged)
}

/// A region's extents around its point: `[x − left, y − top, right − x, bottom − y]`.
fn extents(b: &[f64; 6]) -> [f64; 4] {
    [b[0] - b[2], b[1] - b[3], b[4] - b[0], b[5] - b[1]]
}

// ---- the operator ------------------------------------------------------------------------

/// Per-frame `[x, y, left, top, right, bottom]` from a first frame; `None` = no value.
pub type Boxes = (FrameIndex, Vec<Option<[f64; 6]>>);

/// `sketch`: strokes (every input, in layer order; each a capture) → per-frame box.
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
        let path = compose(ctx.world, ctx.entity, &params, fps);
        let (before, after) = union_reach(&params, fps);
        let home = crate::view::home_of(ctx.world, ctx.entity);
        let (Some(lo), Some(hi)) = (path.keys().next().copied(), path.keys().next_back().copied()) else { return Ok(()) };
        let maps = HomeMaps::new(ctx.world, home, lo - before..hi + after + 1);
        let get = |f: FrameIndex| path.get(&f).copied();
        for f in path.range(range).map(|(f, _)| *f) {
            if let Some(v) = maps.union(get, f, before, after) {
                out.set(f, &v.map(|x| x as f32));
            }
        }
        Ok(())
    }
}

/// A sketch's path in source pixels before the motion union ([`union_at`]):
/// its strokes' frames ([`stroke_frames`]) laid over each other in order.
pub fn compose(world: &World, sketch: Entity, params: &SketchParams, fps: f64) -> BTreeMap<FrameIndex, [f64; 6]> {
    let strokes: Vec<Entity> = world
        .get::<Inputs>(sketch)
        .map(|i| i.0.iter().filter(|(slot, _)| slot == "stroke" || slot == "capture").map(|(_, e)| *e).collect())
        .unwrap_or_default();
    let mut path = BTreeMap::new();
    for e in strokes {
        let Some(((first, boxes), stroke)) = stroke_boxes(world, e, params, fps) else { continue };
        let (lo, values) = layer_over(|f| path.get(&f).copied(), first, &boxes, stroke.falloff as f64 * fps, &stroke);
        for (i, v) in values.into_iter().enumerate() {
            if let Some(v) = v {
                path.insert(lo + i as FrameIndex, v);
            }
        }
    }
    path
}

/// One stroke's frames and layering, read from the world (`None` unless `e`
/// is an enabled capture with a stream and a result).
pub fn stroke_boxes(world: &World, e: Entity, params: &SketchParams, fps: f64) -> Option<(Boxes, Stroke)> {
    let entity = world.get_entity(e).ok()?;
    if entity.contains::<Disabled>() {
        return None;
    }
    let (info, clock) = (entity.get::<Capture>()?, entity.get::<ClockMap>()?);
    let stream = world.resource::<SignalStore>().get(entity.get::<Output>()?.0)?;
    let stroke = entity.get::<Stroke>().cloned().unwrap_or_default();
    let (first, mut frames) = stroke_frames(&read_stream(stream, info.samples), clock, &stroke.sized(params), fps)?;
    // Drawn inside a view: from the view's pixels to the source, frame by frame.
    if let Some(through) = entity.get::<Through>().and_then(|t| world.resource::<SignalStore>().get(t.0)) {
        for (i, v) in frames.iter_mut().enumerate() {
            *v = v.zip(through.get(first + i as FrameIndex)).map(|(b, m)| through_map(m).box_to_source(b));
        }
    }
    Some(((first, frames), stroke))
}

/// A stroke's layering changed: the sketches reading it recompute.
fn stroke_changed(changed: Query<Entity, Changed<Stroke>>, mut inv: ResMut<Invalidations>, t: Res<Transport>) {
    for e in &changed {
        inv.output_changed(e, 0..t.frame_count);
    }
}

pub struct SketchModule;

impl Module for SketchModule {
    fn build(&self, app: &mut AppBuilder) {
        app.component::<Capture>(Class::Document)
            .component::<ClockMap>(Class::Document)
            .component::<Stroke>(Class::Document)
            .component::<Through>(Class::Document)
            .operator(SketchKind)
            .operator_params::<SketchParams>()
            .add_systems(stroke_changed.in_set(Set::Invalidate).before(crate::op::propagate));
    }
}

/// A capture's stream signal id (its Output).
pub fn stream_of(world: &World, capture: Entity) -> Option<crate::signal::SignalId> {
    world.get::<Output>(capture).map(|o| o.0)
}

/// Whether `e` is a sketch (a `sketch` operator).
pub fn is_sketch(world: &World, e: Entity) -> bool {
    world.get::<Operator>(e).is_some_and(|o| o.kind == "sketch")
}

/// The sketch whose region at `frame` contains `pos` (source pixels); the
/// smallest region wins where they overlap.
pub fn pick_sketch(world: &mut World, frame: FrameIndex, pos: [f64; 2]) -> Option<Entity> {
    let mut q = world.query::<(Entity, &Operator, &Output)>();
    let store = world.resource::<SignalStore>();
    q.iter(world)
        .filter(|(_, o, _)| o.kind == "sketch")
        .filter_map(|(e, _, out)| {
            let v = store.get(out.0)?.get(frame)?;
            let inside = (v[2] as f64..=v[4] as f64).contains(&pos[0]) && (v[3] as f64..=v[5] as f64).contains(&pos[1]);
            inside.then(|| (e, (v[4] - v[2]) * (v[5] - v[3])))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(e, _)| e)
}

/// The sketch a selected entity belongs to: the sketch itself, or the sketch
/// one of whose strokes it is.
pub fn sketch_of(world: &mut World, e: Entity) -> Option<Entity> {
    if is_sketch(world, e) {
        return Some(e);
    }
    world.get::<Capture>(e)?;
    let mut q = world.query::<(Entity, &Operator, &Inputs)>();
    q.iter(world).find(|(_, o, i)| o.kind == "sketch" && i.0.iter().any(|(_, p)| *p == e)).map(|(s, _, _)| s)
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
        let (first, s) = c.frame_times().unwrap();
        assert_eq!(first, 0);
        let at = |i: usize| match s[i] {
            Some(Shown::At(t)) => t,
            other => panic!("frame {i}: {other:?}"),
        };
        assert!((at(0) - 0.05 * 0.5 / 3.0).abs() < 1e-9, "frame 0 at the moment its centre was shown: {}", at(0));
        assert!((at(2) - 0.05 * 2.5 / 3.0).abs() < 1e-9);
        assert_eq!(s[3], Some(Shown::Held { from: 0.5, to: 1.05 }), "the hold keeps frame 3 even though playback then crossed its centre");
        assert!((at(1) - 1.20).abs() < 1e-9, "a later pass (after a jump) wins");
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
