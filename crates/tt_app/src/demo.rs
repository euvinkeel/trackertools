//! Dev/benchmark: `TT_SKETCH_DEMO=<sprite_truth.json>` with the sprite fixture
//! open. A scripted noisy hand (tremor, 250 ms lag, 1 kHz) sketches the sprite
//! at ¼ speed, with a 1 s freeze in the middle, through the real app path:
//! pointer frames, the tool, the live preview, commit, evaluation and the
//! overlays. It then logs the error against the truth and quits.

use bevy_ecs::prelude::*;
use tt_core::input::{Action, Key, PendingActions};
use tt_core::op::Output;
use tt_core::selection::Selection;
use tt_core::signal::SignalStore;
use tt_core::tool::{ActiveTool, PointerFrame, Tool};
use tt_core::transport::Transport;

const LAG: f64 = 0.25;
const START_FRAME: i64 = 120;
/// Wall seconds from the press: freeze from .. to, release at.
const FREEZE: (f64, f64) = (4.0, 5.0);
const RELEASE: f64 = 9.0;

enum Phase {
    Wait,
    Capturing { t0: f64 },
    Settle { t0: f64 },
}

pub struct SketchDemo {
    truth: Vec<[f64; 2]>,
    phase: Phase,
    /// (wall time, frame shown) per app frame: what the hand is looking at.
    shown: Vec<(f64, i64)>,
    rng: u64,
    last: f64,
}

impl SketchDemo {
    pub fn start() -> Option<Self> {
        let path = std::env::var_os("TT_SKETCH_DEMO")?;
        let text = std::fs::read_to_string(&path).map_err(|e| tracing::error!("sketch demo: {e}")).ok()?;
        let json: serde_json::Value = serde_json::from_str(&text).ok()?;
        let truth = json["centers"].as_array()?.iter().filter_map(|c| Some([c[0].as_f64()?, c[1].as_f64()?])).collect();
        Some(Self { truth, phase: Phase::Wait, shown: Vec::new(), rng: 0x9e37_79b9_7f4a_7c15, last: 0.0 })
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

    /// Where the hand is at wall time `w`: on the frame it saw `LAG` ago, shaking.
    fn hand(&mut self, w: f64) -> [f64; 2] {
        let i = self.shown.partition_point(|(t, _)| *t <= w - LAG).saturating_sub(1);
        let f = self.shown.get(i).map_or(START_FRAME, |s| s.1);
        let p = self.truth[(f.max(0) as usize).min(self.truth.len() - 1)];
        let shake = |ph: f64| 2.0 * ((9.0 * std::f64::consts::TAU * w + ph).sin() * 0.6 + (12.3 * std::f64::consts::TAU * w + 2.0 * ph).sin() * 0.4);
        [p[0] + shake(0.0) + self.normal(), p[1] + shake(1.3) + self.normal()]
    }

    /// Replace this frame's pointer and keys. Returns true when finished.
    pub fn drive(&mut self, world: &mut World, now: f64, frame: &mut PointerFrame, keys: &mut Vec<Key>) -> bool {
        let t = world.resource::<Transport>().clone();
        self.shown.push((now, t.frame()));
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
        match self.phase {
            Phase::Wait => {
                if world.get_resource::<crate::media::Media>().is_none() || now < 1.5 {
                    return false;
                }
                if t.frame() != START_FRAME {
                    world.resource_mut::<PendingActions>().push(Action::Seek(START_FRAME));
                    world.resource_mut::<PendingActions>().push(Action::SetRate(1)); // 0.25×
                    return false;
                }
                world.resource_mut::<ActiveTool>().0 = Tool::Sketch;
                *frame = PointerFrame { samples, hover: None, pressed: Some(now - 0.002), down: true, released: None };
                self.phase = Phase::Capturing { t0: now };
                tracing::info!("sketch demo: capture started at frame {START_FRAME}, 0.25× speed");
            }
            Phase::Capturing { t0 } => {
                let el = now - t0;
                let release = el >= RELEASE;
                *frame = PointerFrame { samples, hover: None, pressed: None, down: !release, released: release.then_some(now - 0.001) };
                if (FREEZE.0..FREEZE.1).contains(&el) {
                    keys.push(Key::Space);
                }
                if release {
                    self.phase = Phase::Settle { t0: now };
                }
            }
            Phase::Settle { t0 } => {
                *frame = PointerFrame::default();
                if now - t0 > 0.5 {
                    self.report(world);
                    return true;
                }
            }
        }
        false
    }

    fn report(&self, world: &mut World) {
        let Some(op) = world.resource::<Selection>().primary() else {
            tracing::error!("sketch demo: no sketch was committed");
            return;
        };
        let out = world.get::<Output>(op).expect("sketch output").0;
        let store = world.resource::<SignalStore>();
        let sig = store.get(out).expect("sketch signal");
        let (mut errors, mut inside, mut stale, mut outside) = (Vec::new(), 0, 0, Vec::new());
        for (f, truth) in self.truth.iter().enumerate() {
            let Some(v) = sig.get(f as i64) else { continue };
            if sig.get_valid(f as i64).is_none() {
                stale += 1;
            }
            errors.push(((v[0] as f64 - truth[0]).powi(2) + (v[1] as f64 - truth[1]).powi(2)).sqrt());
            if truth[0] >= v[2] as f64 && truth[0] <= v[4] as f64 && truth[1] >= v[3] as f64 && truth[1] <= v[5] as f64 {
                inside += 1;
            } else {
                outside.push(f);
            }
        }
        let n = errors.len();
        // The first and last lag of a capture trail the hand; report the interior too.
        let trim = 6.min(n / 4);
        let mut interior: Vec<f64> = errors[trim..n - trim].to_vec();
        interior.sort_by(f64::total_cmp);
        let q = |v: &[f64], f: f64| v.get(((v.len().max(1) - 1) as f64 * f).round() as usize).copied().unwrap_or(f64::NAN);
        tracing::info!(
            "sketch demo: {n} frames ({stale} stale) · interior point error median {:.2} px, p95 {:.2} px, max {:.2} px · sprite inside region on {:.1}% of frames",
            q(&interior, 0.5),
            q(&interior, 0.95),
            interior.last().copied().unwrap_or(f64::NAN),
            inside as f64 * 100.0 / n.max(1) as f64
        );
        if !outside.is_empty() {
            tracing::info!("sketch demo: sprite outside the region on frames {outside:?}");
        }
    }
}
