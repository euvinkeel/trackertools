//! Dev/benchmark: `TT_SKETCH_DEMO=<sprite_truth.json>` with the sprite fixture
//! open. Through the real app path (pointer frames, the tool, the live
//! preview, commit, evaluation, overlays, lanes), a scripted noisy hand
//! (tremor, 250 ms lag, 1 kHz):
//! 1. records a new sketch: holds the button at frame 120, plays at ¼ speed
//!    (Space taps), pauses for a second mid-way, plays on, pauses, releases;
//! 2. edits it: at frame 180, holds 40 px to the right of the sprite.
//!
//! It logs the error against the truth and the edit's falloff, then quits.

use bevy_ecs::prelude::*;
use tt_core::input::{Action, PendingActions};
use tt_core::op::{Inputs, Output};
use tt_core::selection::Selection;
use tt_core::signal::SignalStore;
use tt_core::sketch::falloff_weight;
use tt_core::tool::{ActiveTool, PointerFrame, Tool};
use tt_core::transport::Transport;

const LAG: f64 = 0.25;
const START_FRAME: i64 = 120;
/// Wall seconds from the press: Space taps (play, pause, play, pause), release.
const TAPS: [f64; 4] = [0.05, 4.0, 5.0, 9.0];
const RELEASE: f64 = 9.4;
const EDIT_FRAME: i64 = 180;
const EDIT_DX: f64 = 40.0;
const EDIT_HOLD: f64 = 0.6;

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Wait,
    /// On the start frame; the hand settles on the sprite before pressing.
    Ready { since: f64 },
    Record { t0: f64, taps: usize },
    Settle { t0: f64 },
    Edit { t0: f64 },
    Done { t0: f64 },
}

pub struct SketchDemo {
    truth: Vec<[f64; 2]>,
    phase: Phase,
    /// (wall time, frame shown) per app frame: what the hand is looking at.
    shown: Vec<(f64, i64)>,
    rng: u64,
    last: f64,
    /// The path around the edit frame before the edit.
    before: Vec<Option<[f32; 6]>>,
}

impl SketchDemo {
    pub fn start() -> Option<Self> {
        let path = std::env::var_os("TT_SKETCH_DEMO")?;
        let text = std::fs::read_to_string(&path).map_err(|e| tracing::error!("sketch demo: {e}")).ok()?;
        let json: serde_json::Value = serde_json::from_str(&text).ok()?;
        let truth = json["centers"].as_array()?.iter().filter_map(|c| Some([c[0].as_f64()?, c[1].as_f64()?])).collect();
        Some(Self { truth, phase: Phase::Wait, shown: Vec::new(), rng: 0x9e37_79b9_7f4a_7c15, last: 0.0, before: Vec::new() })
    }

    fn normal(&mut self) -> f64 {
        let mut unit = || {
            self.rng ^= self.rng << 13;
            self.rng ^= self.rng >> 7;
            self.rng ^= self.rng << 17;
            (self.rng >> 11) as f64 / (1u64 << 53) as f64
        };
        let (u, v) = (unit().max(1e-12), unit());
        (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
    }

    fn truth_at(&self, f: i64) -> [f64; 2] {
        self.truth[(f.max(0) as usize).min(self.truth.len() - 1)]
    }

    /// Where the hand is at wall time `w`, shaking: on the frame it saw `LAG`
    /// ago while recording, or on the edit target while editing.
    fn hand(&mut self, w: f64) -> [f64; 2] {
        let p = if let Phase::Edit { .. } = self.phase {
            let t = self.truth_at(EDIT_FRAME);
            [t[0] + EDIT_DX, t[1]]
        } else {
            let i = self.shown.partition_point(|(t, _)| *t <= w - LAG).saturating_sub(1);
            self.truth_at(self.shown.get(i).map_or(START_FRAME, |s| s.1))
        };
        let shake = |ph: f64| 2.0 * ((9.0 * std::f64::consts::TAU * w + ph).sin() * 0.6 + (12.3 * std::f64::consts::TAU * w + 2.0 * ph).sin() * 0.4);
        [p[0] + shake(0.0) + self.normal(), p[1] + shake(1.3) + self.normal()]
    }

    /// Replace this frame's pointer input. Returns true when finished.
    pub fn drive(&mut self, world: &mut World, now: f64, frame: &mut PointerFrame) -> bool {
        let t = world.resource::<Transport>().clone();
        self.shown.push((now, t.frame()));
        // The edit's hand is on its target from the press on.
        if let Phase::Settle { t0 } = self.phase
            && now - t0 > 0.5
            && t.frame() == EDIT_FRAME
        {
            self.before = self.path(world, EDIT_FRAME - 30..EDIT_FRAME + 30);
            self.phase = Phase::Edit { t0: now };
            self.last = self.last.max(now - 0.004);
            let mut samples = Vec::new();
            let mut w = self.last + 0.001;
            while w <= now {
                let p = self.hand(w);
                samples.push([w, p[0], p[1]]);
                w += 0.001;
            }
            self.last = samples.last().map_or(self.last, |s| s[0]);
            *frame = PointerFrame { samples, pressed: Some(now - 0.003), down: true, scale: 1.0, ..PointerFrame::default() };
            return false;
        }
        let mut samples = Vec::new();
        let mut w = self.last.max(now - 0.05) + 0.001;
        while w <= now {
            let p = self.hand(w);
            samples.push([w, p[0], p[1]]);
            w += 0.001;
        }
        if let Some(s) = samples.last() {
            self.last = s[0];
        }
        let hold = |samples, pressed, released: Option<f64>| PointerFrame { samples, pressed, down: released.is_none(), released, scale: 1.0, ..PointerFrame::default() };
        let push = |world: &mut World, a| world.resource_mut::<PendingActions>().push(a);
        match self.phase {
            Phase::Wait => {
                *frame = PointerFrame::default();
                if world.get_resource::<crate::media::Media>().is_none() || now < 1.5 {
                    return false;
                }
                if t.frame() != START_FRAME || t.playing {
                    push(world, Action::Seek(START_FRAME));
                    push(world, Action::SetRate(1)); // 0.25×
                    return false;
                }
                self.phase = Phase::Ready { since: now };
            }
            Phase::Ready { since } => {
                *frame = PointerFrame::default();
                if now - since < LAG + 0.25 {
                    return false;
                }
                world.resource_mut::<ActiveTool>().0 = Tool::Sketch;
                world.resource_mut::<Selection>().clear();
                *frame = hold(samples, Some(now - 0.002), None);
                self.phase = Phase::Record { t0: now, taps: 0 };
                tracing::info!("sketch demo: recording from frame {START_FRAME} at 0.25× (Space taps at {TAPS:?} s)");
            }
            Phase::Record { t0, taps } => {
                let el = now - t0;
                if taps < TAPS.len() && el >= TAPS[taps] {
                    push(world, Action::TogglePlay);
                    self.phase = Phase::Record { t0, taps: taps + 1 };
                }
                let release = el >= RELEASE;
                *frame = hold(samples, None, release.then_some(now - 0.001));
                if release {
                    self.phase = Phase::Settle { t0: now };
                }
            }
            Phase::Settle { t0 } => {
                *frame = PointerFrame::default();
                if now - t0 > 0.3 && t.frame() != EDIT_FRAME {
                    self.report_recording(world);
                    push(world, Action::Seek(EDIT_FRAME));
                }
            }
            Phase::Edit { t0 } => {
                let release = now - t0 >= EDIT_HOLD;
                *frame = hold(samples, None, release.then_some(now - 0.001));
                if release {
                    self.phase = Phase::Done { t0: now };
                }
            }
            Phase::Done { t0 } => {
                *frame = PointerFrame::default();
                if now - t0 > 0.3 {
                    self.report_edit(world);
                    return true;
                }
            }
        }
        false
    }

    fn path(&self, world: &World, frames: std::ops::Range<i64>) -> Vec<Option<[f32; 6]>> {
        let Some(sketch) = world.resource::<Selection>().primary() else { return Vec::new() };
        let Some(sig) = world.get::<Output>(sketch).and_then(|o| world.resource::<SignalStore>().get(o.0)) else { return Vec::new() };
        frames.map(|f| sig.get(f).map(|v| v.try_into().unwrap())).collect()
    }

    fn report_recording(&self, world: &World) {
        let n_frames = self.truth.len() as i64;
        let path = self.path(world, 0..n_frames);
        let (mut errors, mut inside, mut outside) = (Vec::new(), 0, Vec::new());
        for (f, v) in path.iter().enumerate() {
            let Some(v) = v else { continue };
            let t = self.truth[f];
            errors.push(((v[0] as f64 - t[0]).powi(2) + (v[1] as f64 - t[1]).powi(2)).sqrt());
            if t[0] >= v[2] as f64 && t[0] <= v[4] as f64 && t[1] >= v[3] as f64 && t[1] <= v[5] as f64 {
                inside += 1;
            } else {
                outside.push(f);
            }
        }
        let n = errors.len();
        errors.sort_by(f64::total_cmp);
        let q = |f: f64| errors.get(((n.max(1) - 1) as f64 * f).round() as usize).copied().unwrap_or(f64::NAN);
        tracing::info!(
            "sketch demo: recorded {n} frames · point error median {:.2} px, p95 {:.2} px, max {:.2} px · sprite inside region on {:.1}% of frames",
            q(0.5),
            q(0.95),
            q(1.0),
            inside as f64 * 100.0 / n.max(1) as f64
        );
        if !outside.is_empty() {
            tracing::info!("sketch demo: sprite outside the region on frames {outside:?}");
        }
    }

    fn report_edit(&self, world: &World) {
        let after = self.path(world, EDIT_FRAME - 30..EDIT_FRAME + 30);
        let strokes = world.resource::<Selection>().primary().and_then(|s| world.get::<Inputs>(s)).map_or(0, |i| i.0.len());
        let moved = |k: i64| -> Option<f64> {
            let i = (k + 30) as usize;
            Some(after.get(i).copied()??[0] as f64 - self.before.get(i).copied()??[0] as f64)
        };
        let falloff = world.resource::<tt_core::capture::SketchDefaults>().stroke.falloff as f64 * 60.0;
        let line: Vec<String> = [0i64, 3, 6, 9, 12, 15]
            .iter()
            .map(|k| {
                let expect = moved(0).unwrap_or(f64::NAN) * falloff_weight(*k as f64, falloff);
                format!("±{k}: {:+.1}/{:+.1} (falloff {:+.1})", moved(-*k).unwrap_or(f64::NAN), moved(*k).unwrap_or(f64::NAN), expect)
            })
            .collect();
        tracing::info!("sketch demo: edit at frame {EDIT_FRAME} ({strokes} strokes on the sketch) · x moved {}", line.join(" · "));
    }
}
