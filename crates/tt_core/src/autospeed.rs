//! Anticipatory speed (DESIGN §8.4): while a stroke records with the video
//! playing, the playback rate is set for you, smoothly, so all you do is
//! follow the subject.
//!
//! - **Reactive:** the hand's recent speed on screen (converted to the
//!   subject's speed at 1×) caps the rate at a comfortable hand speed; the
//!   hand's jiggle box growing past its recent calm size (the subject turned
//!   erratic) slows it further. A still, calm hand lets it rise toward the
//!   fastest rate, slowly.
//! - **Anticipatory (foresight):** the sketch you're drawing inside (the
//!   *parent*: the one whose view you're in) already recorded how hard the
//!   subject was to follow, frame by frame: where its box grew past its
//!   typical size, a human was unsure there. Its path just ahead of the
//!   playhead ([`look_ahead`], over any box signal, so tracker results can
//!   feed it too) slows playback *before* a fast or erratic stretch arrives.
//!   Drawing on the source, or as a setting, the sketch being edited is read
//!   instead (or both).
//!
//! The rate moves exponentially in log-rate: quickly down, slowly up. It acts
//! only while a stroke is held and the transport plays. The release (commit
//! or cancel) restores the rate you had set; Q/E during a stroke (any rate
//! change not made here) hands the rate back to you until the release. The
//! ClockMap records every frame's playhead against wall time, so a varying
//! rate needs nothing special downstream.

use bevy_ecs::prelude::*;
use bevy_reflect::Reflect;

use crate::app::{AppBuilder, Module, Set};
use crate::capture::{Live, LiveCapture};
use crate::meta::Class;
use crate::op::Output;
use crate::signal::SignalStore;
use crate::sketch::ClockMap;
use crate::time::{FrameIndex, WallClock};
use crate::tool::PointerFrame;
use crate::transport::Transport;
use crate::view::{ActiveView, map_at};

/// Which sketch's future the look-ahead reads.
#[derive(Reflect, Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Foresight {
    /// The sketch whose view you're drawing in (its human uncertainty is the
    /// foresight); drawing on the source, the sketch being edited.
    #[default]
    Parent,
    /// The sketch the stroke edits.
    Editing,
    /// Both: whichever is more cautious.
    Both,
}

/// The knobs (a user setting; the app remembers them).
#[derive(Resource, Reflect, Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[reflect(Resource)]
#[serde(default)]
pub struct AutoSpeed {
    /// On: while you hold a stroke and the video plays, the playback speed is set for you.
    pub enabled: bool,
    /// The slowest it goes (× real time).
    pub slowest: f32,
    /// The fastest it goes, through still parts (× real time).
    pub fastest: f32,
    /// The fastest your hand should have to move, in screen points per second:
    /// a subject faster than this on screen slows playback until it isn't.
    pub comfort: f32,
    /// How strongly the box growing past its recent calm size (you started to
    /// jiggle: the subject turned erratic) slows playback. 0 ignores it; at 1,
    /// a box three times its calm size halves the speed (growth up to 1.5× is ignored).
    pub jiggle: f32,
    /// Seconds (real time) to slow down: short, so it brakes in time.
    pub slow_down: f32,
    /// Seconds (real time) to speed back up: long, so it doesn't lurch.
    pub speed_up: f32,
    /// How far ahead (seconds of video) the foresight sketch is read, so
    /// playback slows before a fast or erratic stretch arrives.
    pub look_ahead: f32,
    /// Which sketch the look-ahead reads.
    pub foresight: Foresight,
    /// How strongly an erratic stretch ahead (the box growing past its
    /// typical size) slows playback: at 2, a box three times its typical
    /// size runs at a quarter of the speed (growth up to 1.5× is ignored).
    pub erratic: f32,
}

impl Default for AutoSpeed {
    fn default() -> Self {
        Self {
            enabled: true,
            slowest: 0.1,
            fastest: 2.0,
            comfort: 300.0,
            jiggle: 1.0,
            slow_down: 0.1,
            speed_up: 1.0,
            look_ahead: 0.75,
            foresight: Foresight::Parent,
            erratic: 2.0,
        }
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
#[derive(Resource, Debug, Default)]
pub struct AutoSpeedState {
    run: Option<Run>,
    /// The rate auto speed last wrote into the transport, so the HUD can tell
    /// its changes (never flashed) from yours.
    pub wrote: Option<f64>,
    /// The slowest and fastest rates it set during the current or last stroke.
    pub range: Option<(f64, f64)>,
    /// What limits the rate right now, in a word or two (for the HUD).
    pub reason: &'static str,
}

impl AutoSpeedState {
    /// Auto speed drives the rate right now (a stroke is held, and you haven't taken the rate back).
    pub fn acting(&self) -> bool {
        self.run.as_ref().is_some_and(|r| !r.off)
    }
}

#[derive(Debug)]
struct Run {
    /// The stroke's press time (identifies it).
    stroke: f64,
    /// The rate you had set; restored at the release.
    manual: f64,
    /// You changed the rate during this stroke: auto speed leaves it to you until the release.
    off: bool,
    /// The rate the transport should have; anything else was you.
    expected: f64,
    /// The jiggle box's calm size (screen points), updated slowly.
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

/// What a box signal says about one frame just ahead of the playhead.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ahead {
    /// Frames from the playhead.
    pub frames: usize,
    /// How fast its point moves there (screen points per second of video).
    pub speed: f64,
    /// Its box there relative to the box at the playhead (mean side, with
    /// tremor-sized boxes floored; 1 = the same, 2 = twice as big).
    pub growth: f64,
}

/// Read a box-producing signal over the frames ahead: `boxes` are
/// `[x, y, left, top, right, bottom]` in screen points, one per video frame
/// from the playhead on (`None` = no value). One entry per frame with a
/// value. Speeds are measured over about 50 ms of video, so per-frame noise
/// doesn't read as speed.
pub fn look_ahead(boxes: &[Option<[f64; 6]>], fps: f64) -> Vec<Ahead> {
    let now = boxes.iter().flatten().next().map(side);
    look_ahead_from(boxes, now, fps)
}

/// A box's mean side.
fn side(b: &[f64; 6]) -> f64 {
    ((b[4] - b[2]) + (b[5] - b[3])) / 2.0
}

/// [`look_ahead`], with growth measured against `calm` (a typical box side,
/// in the same points) instead of the box at the playhead.
pub fn look_ahead_from(boxes: &[Option<[f64; 6]>], calm: Option<f64>, fps: f64) -> Vec<Ahead> {
    let Some(calm) = calm else { return Vec::new() };
    let k = ((0.05 * fps).round() as usize).max(1);
    let dist = |a: [f64; 6], b: [f64; 6]| (b[0] - a[0]).hypot(b[1] - a[1]) * fps / k as f64;
    (0..boxes.len())
        .filter_map(|i| {
            let b = boxes[i]?;
            let speed = match (boxes.get(i + k).copied().flatten(), i.checked_sub(k).and_then(|j| boxes[j])) {
                (Some(next), _) => dist(b, next),
                (None, Some(prev)) => dist(prev, b),
                _ => 0.0,
            };
            Some(Ahead { frames: i, speed, growth: (side(&b) + JIGGLE_FLOOR) / (calm + JIGGLE_FLOOR) })
        })
        .collect()
}

/// The slow-down factor (≤ 1) for a box grown by `growth` over its calm size.
fn slow_for(growth: f64, sensitivity: f64) -> f64 {
    (growth / JIGGLE_SLACK).max(1.0).powf(-sensitivity)
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

/// Rate limits from the stroke so far (× real time).
struct Limits {
    /// The comfortable rate for the hand's recent speed.
    hand: f64,
    /// The slow-down factor (≤ 1) from the hand's jiggle box growing.
    jiggle: f64,
    /// The rate the stretch ahead allows now.
    ahead: f64,
}

/// `scale` = screen points per pixel of the space the stroke is drawn in (the shown one).
fn limits(world: &World, live: &Live, knobs: &AutoSpeed, calm: &mut Option<f64>, scale: f64, dt: f64) -> Limits {
    let t = world.resource::<Transport>();
    let fps = t.fps.as_f64();
    let comfort = knobs.comfort.max(1.0) as f64;
    let sensitivity = knobs.jiggle.max(0.0) as f64;
    let (_, fastest) = knobs.range();
    let t_end = live.samples.last().map_or(0.0, |s| s[0]);
    let mut out = Limits { hand: f64::INFINITY, jiggle: 1.0, ahead: f64::INFINITY };

    // The hand: its speed now is the subject's as it was on screen `lag` ago.
    if let (Some((v, _)), Some(r)) = (line_fit(recent(&live.samples, SPEED_WINDOW)), rate_seen(&live.clock, t_end, live.stroke.lag as f64, fps)) {
        let at_1x = v[0].hypot(v[1]) * scale / r;
        if at_1x > 1e-6 {
            out.hand = comfort / at_1x;
        }
    }
    // Its jiggle box against the calm size it had.
    if let Some((_, rms)) = line_fit(recent(&live.samples, live.params.jiggle_window.max(0.05) as f64)) {
        let size = JIGGLE_SIDE * rms * scale;
        let c = calm.get_or_insert(size);
        let growth = (size + JIGGLE_FLOOR) / (*c + JIGGLE_FLOOR);
        out.jiggle = slow_for(growth, sensitivity);
        let tau = if size < *c { CALM_FALL } else { CALM_RISE };
        *c += (size - *c) * (1.0 - (-dt / tau).exp());
    }
    // Ahead: the foresight sketch (the parent, by default), as it will be on
    // screen. Growth is against its typical size around here (the lower
    // quartile over the last 2 s and the window), so a box that is large now
    // counts as erratic too. Each frame's comfortable rate binds fully from a
    // braking margin before its arrival (3 slow-down times at the current
    // rate) and fades out toward the end of the look-ahead (a ramp up to the
    // fastest rate), so playback arrives slowed, braking progressively
    // instead of crawling for the whole window.
    let store = world.resource::<SignalStore>();
    let window = knobs.look_ahead.max(0.0) as f64;
    let margin = 3.0 * knobs.slow_down.max(0.0) as f64 * t.rate;
    let erratic = knobs.erratic.max(0.0) as f64;
    let view = world.resource::<ActiveView>().0;
    let (f0, n, past) = (t.frame(), (window * fps).ceil() as FrameIndex, (2.0 * fps) as FrameIndex);
    for source in foresight_sources(world, live, knobs.foresight) {
        let Some(sig) = world.get::<Output>(source).and_then(|o| store.get(o.0)).filter(|_| window > 0.0) else { continue };
        let boxes: Vec<Option<[f64; 6]>> = (f0 - past..=f0 + n)
            .map(|f| {
                let v = sig.get(f)?;
                let b = map_at(world, view, f).box_from_source(std::array::from_fn(|c| v[c] as f64));
                Some(b.map(|x| x * scale))
            })
            .collect();
        let mut sides: Vec<f64> = boxes.iter().flatten().map(side).collect();
        sides.sort_by(f64::total_cmp);
        let calm = sides.get(sides.len() / 4).copied();
        for a in look_ahead_from(&boxes[past as usize..], calm, fps) {
            let wanted = (comfort / a.speed.max(1e-6)).min(fastest) * slow_for(a.growth, erratic);
            let reach = if window > margin { ((a.frames as f64 / fps - margin) / (window - margin)).clamp(0.0, 1.0) } else { 0.0 };
            out.ahead = out.ahead.min(wanted + (fastest - wanted).max(0.0) * reach);
        }
    }
    out
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

/// Drive the transport's rate while a stroke records (`Set::Tools`, after
/// the Sketch tool, so the next frame plays at the new rate).
pub fn auto_speed(world: &mut World) {
    let knobs = world.resource::<AutoSpeed>().clone();
    let dt = world.resource::<WallClock>().dt;
    let stroke = world.resource::<LiveCapture>().0.as_ref().map(|l| l.start);
    let mut state = std::mem::take(&mut *world.resource_mut::<AutoSpeedState>());

    // The stroke ended (commit or cancel), or auto speed was switched off: your rate is back.
    if state.run.as_ref().is_some_and(|r| Some(r.stroke) != stroke || !knobs.enabled) {
        let run = state.run.take().expect("a run");
        if !run.off {
            world.resource_mut::<Transport>().rate = run.manual;
            state.wrote = Some(run.manual);
        }
        if let Some((lo, hi)) = state.range {
            tracing::info!("auto speed: stroke over, rate ×{lo:.2}–×{hi:.2}; back to ×{:.2}", world.resource::<Transport>().rate);
        }
    }
    if let (None, true, Some(start)) = (&state.run, knobs.enabled, stroke) {
        let rate = world.resource::<Transport>().rate;
        state.run = Some(Run { stroke: start, manual: rate, off: false, expected: rate, calm: None });
        state.range = None;
    }
    if let Some(run) = state.run.as_mut().filter(|r| !r.off) {
        let t = world.resource::<Transport>().clone();
        if t.rate != run.expected {
            // Q/E (or any change not made here): yours until the release.
            run.off = true;
            tracing::info!("auto speed: you set ×{:.2}; off for the rest of this stroke", t.rate);
        } else if t.playing {
            let scale = world.resource::<PointerFrame>().scale;
            let scale = if scale > 0.0 { scale } else { 1.0 };
            let live = world.resource::<LiveCapture>().0.as_ref().expect("a live stroke");
            let l = limits(world, live, &knobs, &mut run.calm, scale, dt);
            let (slowest, fastest) = knobs.range();
            let live_rate = fastest.min(l.hand) * l.jiggle;
            let target = live_rate.min(l.ahead).clamp(slowest, fastest);
            state.reason = if l.ahead < live_rate && l.ahead < fastest {
                "ahead"
            } else if l.jiggle < 0.9 {
                "jiggle"
            } else if l.hand < fastest {
                "fast hand"
            } else {
                "calm"
            };
            // Exponential in log-rate: quickly down, slowly up.
            let tau = if target < t.rate { knobs.slow_down } else { knobs.speed_up }.max(0.001) as f64;
            let k = 1.0 - (-dt / tau).exp();
            let rate = (t.rate.ln() + (target.ln() - t.rate.ln()) * k).exp().clamp(slowest, fastest);
            world.resource_mut::<Transport>().rate = rate;
            run.expected = rate;
            state.wrote = Some(rate);
            state.range = Some(state.range.map_or((rate, rate), |(lo, hi)| (lo.min(rate), hi.max(rate))));
        }
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
            .add_systems(auto_speed.in_set(Set::Tools).after(crate::capture::sketch_tool));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn look_ahead_finds_the_fastest_stretch_and_the_growth() {
        // Still for 20 frames, then 10 px per frame; the box doubles at frame 30.
        let boxes: Vec<Option<[f64; 6]>> = (0..45)
            .map(|i| {
                let x = if i < 20 { 0.0 } else { 10.0 * (i - 20) as f64 };
                let h = if i < 30 { 20.0 } else { 40.0 };
                Some([x, 0.0, x - h, -h, x + h, h])
            })
            .collect();
        let a = look_ahead(&boxes, 60.0);
        assert_eq!(a.len(), 45);
        assert_eq!((a[10].speed, a[25].speed), (0.0, 600.0), "still, then 10 px per frame");
        assert_eq!(a[25].frames, 25);
        let side = |h: f64| (2.0 * h + JIGGLE_FLOOR) / (40.0 + JIGGLE_FLOOR);
        assert!((a[40].growth - side(40.0)).abs() < 1e-9 && a[10].growth == 1.0, "{}", a[40].growth);
        assert!(look_ahead(&[None, None], 60.0).is_empty());
    }

    #[test]
    fn line_fit_separates_motion_from_jiggle() {
        // 200 px/s to the right with a ±10 px, 7 Hz jiggle in y.
        let s: Vec<[f64; 3]> = (0..250).map(|i| {
            let t = i as f64 / 1000.0;
            [t, 200.0 * t, 10.0 * (std::f64::consts::TAU * 7.0 * t).sin()]
        }).collect();
        let (v, rms) = line_fit(&s).unwrap();
        assert!((v[0] - 200.0).abs() < 1e-6, "{v:?}");
        assert!((5.0..9.0).contains(&rms), "{rms}");
    }
}
