//! Anticipatory speed (DESIGN §8.4): while a stroke records with the video
//! playing, the playback rate is set for you, so all you do is follow the
//! subject. It needs no numbers: it calibrates itself.
//!
//! - **Foresight:** the parent sketch (the one whose view you're drawing
//!   in; on the source, the sketch you're editing) already recorded how hard
//!   the subject was to follow: its box grew where the hand jiggled or
//!   raced. How big its box *normally* is, is measured on itself: the 20th
//!   percentile of its box sizes is "calm", the 80th "busy". The busiest box
//!   in the next `look_ahead` seconds, placed in that range, picks a rate in
//!   your range: calm plays at `fastest`, busy at `slowest`, in between on a
//!   log scale. So playback slows *before* a busy stretch arrives.
//! - **Q / E** while it drives set a multiplier on what it picks (×1.5 per
//!   press), kept until changed: "a bit faster everywhere".
//! - **Your hand** (optional, off by default): the older reactive limits, the
//!   hand's speed on screen against a comfortable speed, which *Calibrate*
//!   sets from how fast your hand moved in your recent strokes.
//!
//! The rate moves exponentially in log-rate: quickly down, slowly up. It acts
//! only while a stroke is held and the transport plays; the release restores
//! the rate you had set. The ClockMap records every frame's playhead against
//! wall time, so a varying rate needs nothing special downstream.

use std::collections::{HashMap, VecDeque};

use bevy_ecs::prelude::*;
use bevy_reflect::Reflect;

use crate::app::{AppBuilder, Module, Set};
use crate::capture::{Live, LiveCapture};
use crate::input::{Action, PendingActions};
use crate::meta::Class;
use crate::op::Output;
use crate::signal::SignalStore;
use crate::sketch::ClockMap;
use crate::time::{FrameIndex, WallClock};
use crate::tool::PointerFrame;
use crate::transport::Transport;

/// Which sketch's future the look-ahead reads.
#[derive(Reflect, Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Foresight {
    /// The sketch whose view you're drawing in (its human uncertainty is the
    /// foresight); drawing on the source, the sketch being edited.
    #[default]
    Parent,
    /// The sketch the stroke edits.
    Editing,
    /// Both: whichever is busier.
    Both,
}

/// The knobs (a user setting; the app remembers them).
#[derive(Resource, Reflect, Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[reflect(Resource)]
#[serde(default)]
pub struct AutoSpeed {
    /// On: while you hold a stroke and the video plays, the playback speed is set for you.
    pub enabled: bool,
    /// The speed range: busy stretches play at `slowest`, calm ones at `fastest` (× real time).
    pub slowest: f32,
    pub fastest: f32,
    /// How far ahead (seconds of video) it looks: it slows this long before a busy stretch.
    pub look_ahead: f32,
    /// (Advanced) Which sketch it reads ahead in.
    pub foresight: Foresight,
    /// (Advanced) Also slow down when your hand moves fast or starts to jiggle.
    pub react_to_hand: bool,
    /// (Advanced, with `react_to_hand`) The on-screen speed your hand follows comfortably
    /// (points per second); *Calibrate* sets it from your recent strokes.
    pub comfort: f32,
    /// (Advanced, with `react_to_hand`) How strongly your jiggle growing slows it (0 = not).
    pub jiggle: f32,
    /// (Advanced) Seconds (real time) to slow down, and to speed back up.
    pub slow_down: f32,
    pub speed_up: f32,
}

impl Default for AutoSpeed {
    fn default() -> Self {
        Self { enabled: true, slowest: 0.1, fastest: 2.0, look_ahead: 1.0, foresight: Foresight::Parent, react_to_hand: false, comfort: 300.0, jiggle: 1.0, slow_down: 0.1, speed_up: 1.0 }
    }
}

impl AutoSpeed {
    /// `(slowest, fastest)`, sanitized.
    pub fn range(&self) -> (f64, f64) {
        let slowest = self.slowest.max(0.01) as f64;
        (slowest, (self.fastest as f64).max(slowest))
    }
}

/// Auto speed at work (derived): the stroke it drives and what set the rate.
#[derive(Resource, Debug)]
pub struct AutoSpeedState {
    run: Option<Run>,
    /// The rate auto speed last wrote into the transport, so the HUD can tell
    /// its changes (never flashed) from yours.
    pub wrote: Option<f64>,
    /// The slowest and fastest rates it set during the current or last stroke.
    pub range: Option<(f64, f64)>,
    /// What sets the rate right now, in a word or two (for the HUD).
    pub reason: &'static str,
    /// How busy the stretch ahead is: 0 calm … 1 busy (for the HUD).
    pub busy: Option<f64>,
    /// Q/E while it drives: a multiplier on what it picks, kept until changed.
    pub bias: f64,
    /// When `bias` last changed (wall seconds), for the flash.
    pub bias_changed: f64,
    /// Per foresight sketch: (its signal's version, calm, busy box sizes).
    refs: HashMap<Entity, (u64, f64, f64)>,
    /// The hand's on-screen speed at 1× (points per second), one per app
    /// frame while recording and playing: the last minute or so.
    hand: VecDeque<f64>,
}

impl Default for AutoSpeedState {
    fn default() -> Self {
        Self { run: None, wrote: None, range: None, reason: "", busy: None, bias: 1.0, bias_changed: f64::NEG_INFINITY, refs: HashMap::new(), hand: VecDeque::new() }
    }
}

impl AutoSpeedState {
    /// Auto speed drives the rate right now (a stroke is held and it has something to go on).
    pub fn acting(&self) -> bool {
        self.run.as_ref().is_some_and(|r| r.driving)
    }

    /// A comfortable hand speed from your recent strokes (points per second
    /// on screen at 1×): the 80th percentile of how fast your hand moved.
    /// None until there's a couple of seconds of recording to go on.
    pub fn calibrated_comfort(&self) -> Option<f64> {
        (self.hand.len() >= 350).then(|| percentiles(&mut self.hand.iter().copied().collect::<Vec<_>>()).1)
    }
}

#[derive(Debug)]
struct Run {
    /// The stroke's press time (identifies it).
    stroke: f64,
    /// The rate you had set; restored at the release.
    manual: f64,
    /// The rate the transport should have; anything else was you (a multiplier).
    expected: f64,
    /// It has set the rate this stroke.
    driving: bool,
    /// The jiggle box's calm size (screen points), updated slowly (hand mode).
    calm: Option<f64>,
}

/// Hand speed is measured over this much of the latest samples (s, real time).
const SPEED_WINDOW: f64 = 0.1;
/// Jiggle below about this size (screen points) is hand tremor: it doesn't count as growth.
const JIGGLE_FLOOR: f64 = 10.0;
/// Box growth up to this ratio is ordinary wobble: only growth beyond it slows playback.
const JIGGLE_SLACK: f64 = 1.5;
/// The calm size follows a shrinking box within this long, a growing one within this long (s).
const CALM_FALL: f64 = 0.3;
const CALM_RISE: f64 = 3.0;
/// The side of the box the sketch's jiggle stage makes from an RMS spread (2 × 2.2 σ per axis, without pad).
const JIGGLE_SIDE: f64 = 2.0 * 2.2 / std::f64::consts::SQRT_2;
/// App frames of hand speed kept for calibration (about a minute).
const HAND_HISTORY: usize = 12_000;
/// One Q/E press while it drives multiplies its choice by this.
pub const BIAS_STEP: f64 = 1.5;

/// The 20th and 80th percentiles: "calm" and "busy".
fn percentiles(v: &mut [f64]) -> (f64, f64) {
    v.sort_by(f64::total_cmp);
    let at = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
    (at(0.2), at(0.8))
}

/// Where `x` sits from calm (0) to busy (1). A sketch that is about the
/// same size everywhere (busy ≈ calm) still needs a clear step up to read as
/// busy: the spread is at least half the calm size.
pub fn busyness(x: f64, calm: f64, busy: f64) -> f64 {
    let spread = (busy - calm).max(0.5 * calm.abs()).max(1e-9);
    ((x - calm) / spread).clamp(0.0, 1.0)
}

/// Busyness 0 … 1 → a rate from `fastest` down to `slowest`, even on a log scale.
pub fn busy_rate(u: f64, slowest: f64, fastest: f64) -> f64 {
    (fastest.ln() + u.clamp(0.0, 1.0) * (slowest.ln() - fastest.ln())).exp()
}

/// A box value's mean side `[x, y, left, top, right, bottom]`.
fn side(v: &[f32]) -> f64 {
    ((v[4] - v[2]) + (v[5] - v[3])) as f64 / 2.0
}

/// The sketches whose future the look-ahead reads: the parent (the box the
/// view being drawn in frames), and/or the sketch the stroke edits.
fn foresight_sources(world: &World, live: &Live, foresight: Foresight) -> Vec<Entity> {
    let parent = live.drawn_in.and_then(|v| crate::view::sketch_framed(world, v)).filter(|e| world.get::<bevy_ecs::entity_disabling::Disabled>(*e).is_none());
    let mut out: Vec<Entity> = match foresight {
        Foresight::Parent => parent.or(live.target).into_iter().collect(),
        Foresight::Editing => live.target.into_iter().collect(),
        Foresight::Both => parent.into_iter().chain(live.target).collect(),
    };
    out.dedup();
    out
}

/// How busy the next `look_ahead` seconds are in the foresight sketches
/// (0 calm … 1 busy; the busier of them), each against its own normal.
fn busy_ahead(world: &World, live: &Live, knobs: &AutoSpeed, refs: &mut HashMap<Entity, (u64, f64, f64)>) -> Option<f64> {
    let t = world.resource::<Transport>();
    let (f0, n) = (t.frame(), (knobs.look_ahead.max(0.0) as f64 * t.fps.as_f64()).ceil() as FrameIndex);
    let store = world.resource::<SignalStore>();
    let mut out: Option<f64> = None;
    for source in foresight_sources(world, live, knobs.foresight) {
        let Some(sig) = world.get::<Output>(source).and_then(|o| store.get(o.0)) else { continue };
        // Only the frames the sketch is alive on (its span) count, for its normal and ahead.
        let span = crate::span::span_of(world, source);
        let sig = crate::span::clip(sig, span);
        let key = sig.version() ^ (span.range().start as u64).rotate_left(17) ^ (span.range().end as u64).rotate_left(41);
        let (calm, busy) = match refs.get(&source) {
            Some((v, c, b)) if *v == key => (*c, *b),
            _ => {
                let Some((lo, hi)) = sig.present_hull() else { continue };
                let mut sides: Vec<f64> = (lo..=hi).filter_map(|f| sig.get(f)).map(side).collect();
                if sides.is_empty() {
                    continue;
                }
                let (c, b) = percentiles(&mut sides);
                refs.insert(source, (key, c, b));
                (c, b)
            }
        };
        // (Ahead is behind while playing backward.)
        let ahead = if t.reverse { f0 - n..=f0 } else { f0..=f0 + n };
        let ahead = ahead.filter_map(|f| sig.get(f)).map(side).fold(None, |m: Option<f64>, s| Some(m.map_or(s, |m| m.max(s))));
        if let Some(a) = ahead {
            let u = busyness(a, calm, busy);
            out = Some(out.map_or(u, |o| o.max(u)));
        }
    }
    out
}

/// A least-squares line through `(t, x, y)` samples: its velocity (units per
/// second) and the samples' RMS distance from it.
fn line_fit(s: &[[f64; 3]]) -> Option<([f64; 2], f64)> {
    let n = s.len() as f64;
    if s.len() < 3 {
        return None;
    }
    let mean = |c: usize| s.iter().map(|v| v[c]).sum::<f64>() / n;
    let (tm, xm, ym) = (mean(0), mean(1), mean(2));
    let stt: f64 = s.iter().map(|v| (v[0] - tm).powi(2)).sum();
    let slope = |c: usize, m: f64| if stt > 1e-12 { s.iter().map(|v| (v[0] - tm) * (v[c] - m)).sum::<f64>() / stt } else { 0.0 };
    let (vx, vy) = (slope(1, xm), slope(2, ym));
    let ss: f64 = s.iter().map(|v| (v[1] - xm - vx * (v[0] - tm)).powi(2) + (v[2] - ym - vy * (v[0] - tm)).powi(2)).sum();
    Some(([vx, vy], (ss / n).sqrt()))
}

/// The samples of the last `window` seconds.
fn recent(samples: &[[f64; 3]], window: f64) -> &[[f64; 3]] {
    let Some(end) = samples.last().map(|s| s[0]) else { return samples };
    &samples[samples.partition_point(|s| s[0] < end - window)..]
}

/// The playhead at capture time `t`, from the clock map (None before it starts).
fn playhead_at(clock: &ClockMap, t: f64) -> Option<f64> {
    let i = clock.t.partition_point(|x| *x <= t);
    if i == 0 {
        return None;
    }
    let j = i.min(clock.t.len() - 1);
    let (t0, t1, f0, f1) = (clock.t[i - 1], clock.t[j], clock.frame[i - 1], clock.frame[j]);
    Some(if t1 > t0 { f0 + (f1 - f0) * ((t - t0) / (t1 - t0)).clamp(0.0, 1.0) } else { f0 })
}

/// Playback rate while the hand's view of it was on screen: over
/// [t − lag − window, t − lag]. None while it was paused, jumping or unknown.
fn rate_seen(clock: &ClockMap, t: f64, lag: f64, fps: f64) -> Option<f64> {
    let b = t - lag;
    let a = (b - SPEED_WINDOW).max(*clock.t.first()?);
    if b - a < 0.02 {
        return None;
    }
    let r = (playhead_at(clock, b)? - playhead_at(clock, a)?) / ((b - a) * fps);
    (0.02..=8.0).contains(&r).then_some(r)
}

/// The hand now: its on-screen speed at 1× (points per second) if it can be
/// told, and (hand mode) the rate that keeps it comfortable, slowed further
/// by its jiggle growing. `scale` = screen points per canvas pixel.
fn hand(world: &World, live: &Live, knobs: &AutoSpeed, calm: &mut Option<f64>, scale: f64, dt: f64) -> (Option<f64>, f64) {
    let fps = world.resource::<Transport>().fps.as_f64();
    let t_end = live.samples.last().map_or(0.0, |s| s[0]);
    let at_1x = match (line_fit(recent(&live.samples, SPEED_WINDOW)), rate_seen(&live.clock, t_end, live.stroke.lag as f64, fps)) {
        (Some((v, _)), Some(r)) => Some(v[0].hypot(v[1]) * scale / r),
        _ => None,
    };
    let (_, fastest) = knobs.range();
    let mut limit = at_1x.filter(|s| *s > 1e-6).map_or(fastest, |s| knobs.comfort.max(1.0) as f64 / s);
    if let Some((_, rms)) = line_fit(recent(&live.samples, live.params.jiggle_window.max(0.05) as f64)) {
        let size = JIGGLE_SIDE * rms * scale;
        let c = calm.get_or_insert(size);
        let growth = (size + JIGGLE_FLOOR) / (*c + JIGGLE_FLOOR);
        limit *= (growth / JIGGLE_SLACK).max(1.0).powf(-knobs.jiggle.max(0.0) as f64);
        let tau = if size < *c { CALM_FALL } else { CALM_RISE };
        *c += (size - *c) * (1.0 - (-dt / tau).exp());
    }
    (at_1x, limit)
}

/// Q/E while it drives: multiply its choice (`Set::Input`, before the
/// transport would step the rate itself).
fn auto_keys(mut actions: ResMut<PendingActions>, mut state: ResMut<AutoSpeedState>, clock: Res<WallClock>) {
    if !state.acting() {
        return;
    }
    let steps = actions.take(|a| matches!(a, Action::FasterPlayback | Action::SlowerPlayback));
    for a in steps {
        let k = if a == Action::FasterPlayback { BIAS_STEP } else { 1.0 / BIAS_STEP };
        state.bias = (state.bias * k).clamp(0.05, 20.0);
        state.bias_changed = clock.now;
        tracing::info!("auto speed: your multiplier ×{:.2}", state.bias);
    }
}

/// Drive the transport's rate while a stroke records (`Set::Tools`, after
/// the Sketch tool, so the next frame plays at the new rate).
pub fn auto_speed(world: &mut World) {
    let knobs = world.resource::<AutoSpeed>().clone();
    let (dt, now) = {
        let c = world.resource::<WallClock>();
        (c.dt, c.now)
    };
    let stroke = world.resource::<LiveCapture>().0.as_ref().map(|l| l.start);
    let mut state = std::mem::take(&mut *world.resource_mut::<AutoSpeedState>());

    // The stroke ended (commit or cancel), or auto speed was switched off: your rate is back.
    if state.run.as_ref().is_some_and(|r| Some(r.stroke) != stroke || !knobs.enabled) {
        let run = state.run.take().expect("a run");
        if run.driving {
            world.resource_mut::<Transport>().rate = run.manual;
            state.wrote = Some(run.manual);
            if let Some((lo, hi)) = state.range {
                tracing::info!("auto speed: stroke over, rate ×{lo:.2}–×{hi:.2}; back to ×{:.2}", run.manual);
            }
        }
        state.busy = None;
    }
    if let (None, true, Some(start)) = (&state.run, knobs.enabled, stroke) {
        let rate = world.resource::<Transport>().rate;
        state.run = Some(Run { stroke: start, manual: rate, expected: rate, driving: false, calm: None });
        state.range = None;
    }
    if let Some(mut run) = state.run.take() {
        let t = world.resource::<Transport>().clone();
        if run.driving && (t.rate - run.expected).abs() > 1e-9 {
            // Any other change of rate while it drives (the speed menu): your multiplier.
            state.bias = (state.bias * t.rate / run.expected).clamp(0.05, 20.0);
            state.bias_changed = now;
        }
        if t.playing {
            let scale = world.resource::<PointerFrame>().scale;
            let scale = if scale > 0.0 { scale } else { 1.0 };
            let live = world.resource::<LiveCapture>().0.as_ref().expect("a live stroke");
            let busy = busy_ahead(world, live, &knobs, &mut state.refs);
            let (at_1x, hand_limit) = hand(world, live, &knobs, &mut run.calm, scale, dt);
            if let Some(s) = at_1x {
                state.hand.push_back(s);
                if state.hand.len() > HAND_HISTORY {
                    state.hand.pop_front();
                }
            }
            let (slowest, fastest) = knobs.range();
            let ahead = busy.map(|u| busy_rate(u, slowest, fastest));
            let target = match (ahead, knobs.react_to_hand) {
                (Some(a), true) => Some(a.min(hand_limit)),
                (Some(a), false) => Some(a),
                (None, true) => Some(hand_limit.min(fastest)),
                (None, false) => None,
            };
            state.busy = busy;
            state.reason = match (ahead, knobs.react_to_hand) {
                (Some(a), true) if hand_limit < a => "your hand",
                (Some(_), _) if busy.is_some_and(|u| u > 0.05) => "busy ahead",
                (Some(_), _) => "calm ahead",
                (None, true) => "your hand",
                (None, false) => "nothing to read ahead",
            };
            if let Some(target) = target {
                let target = (target.clamp(slowest, fastest) * state.bias).clamp(0.02, 8.0);
                // Exponential in log-rate: quickly down, slowly up.
                let tau = if target < t.rate { knobs.slow_down } else { knobs.speed_up }.max(0.001) as f64;
                let k = 1.0 - (-dt / tau).exp();
                let rate = (t.rate.ln() + (target.ln() - t.rate.ln()) * k).exp().clamp(0.02, 8.0);
                world.resource_mut::<Transport>().rate = rate;
                run.expected = rate;
                run.driving = true;
                state.wrote = Some(rate);
                state.range = Some(state.range.map_or((rate, rate), |(lo, hi)| (lo.min(rate), hi.max(rate))));
            }
        } else {
            run.expected = t.rate;
        }
        state.run = Some(run);
    }
    *world.resource_mut::<AutoSpeedState>() = state;
}

pub struct AutoSpeedModule;

impl Module for AutoSpeedModule {
    fn build(&self, app: &mut AppBuilder) {
        app.resource_type::<AutoSpeed>(Class::Session)
            .register_type::<Foresight>()
            .declare::<AutoSpeedState>(Class::Derived)
            .init_resource::<AutoSpeed>()
            .init_resource::<AutoSpeedState>()
            .add_systems(auto_keys.in_set(Set::Input))
            .add_systems(auto_speed.in_set(Set::Tools).after(crate::capture::sketch_tool));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busyness_places_a_size_between_calm_and_busy() {
        assert_eq!(busyness(40.0, 40.0, 80.0), 0.0);
        assert_eq!(busyness(60.0, 40.0, 80.0), 0.5);
        assert_eq!(busyness(200.0, 40.0, 80.0), 1.0);
        // The same size everywhere: a step of half the calm size reads as busy.
        assert_eq!(busyness(60.0, 40.0, 40.0), 1.0);
        assert_eq!(busyness(50.0, 40.0, 40.0), 0.5);
    }

    #[test]
    fn busy_rate_spans_the_range_on_a_log_scale() {
        assert!((busy_rate(0.0, 0.1, 2.0) - 2.0).abs() < 1e-12);
        assert!((busy_rate(1.0, 0.1, 2.0) - 0.1).abs() < 1e-12);
        assert!((busy_rate(0.5, 0.1, 2.0) - (0.1f64 * 2.0).sqrt()).abs() < 1e-12, "the middle is the geometric mean");
    }

    #[test]
    fn line_fit_separates_motion_from_jiggle() {
        // 200 px/s to the right with a ±10 px, 7 Hz jiggle in y.
        let s: Vec<[f64; 3]> = (0..250)
            .map(|i| {
                let t = i as f64 / 1000.0;
                [t, 200.0 * t, 10.0 * (std::f64::consts::TAU * 7.0 * t).sin()]
            })
            .collect();
        let (v, rms) = line_fit(&s).unwrap();
        assert!((v[0] - 200.0).abs() < 1e-6, "{v:?}");
        assert!((5.0..9.0).contains(&rms), "{rms}");
    }
}
