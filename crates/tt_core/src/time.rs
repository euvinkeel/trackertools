//! Time (DESIGN §3): rational rates, the frame grid, and the wall clock.

use bevy_ecs::prelude::*;
use bevy_reflect::Reflect;
use serde::{Deserialize, Serialize};

use crate::app::{AppBuilder, Module};
use crate::meta::Class;

/// A position on a media's constant-rate frame grid.
pub type FrameIndex = i64;

/// An exact rate such as 60000/1001 frames per second. Always normalized, `den > 0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Reflect, Serialize, Deserialize)]
pub struct Rational {
    pub num: i64,
    pub den: i64,
}

impl Rational {
    pub fn new(num: i64, den: i64) -> Self {
        assert!(den != 0, "rational with zero denominator");
        let g = gcd(num.unsigned_abs(), den.unsigned_abs()).max(1) as i64;
        let sign = if den < 0 { -1 } else { 1 };
        Self { num: sign * num / g, den: sign * den / g }
    }

    pub fn as_f64(self) -> f64 {
        self.num as f64 / self.den as f64
    }

    /// Seconds from the start of the grid to the start of frame `f`.
    pub fn frame_to_seconds(self, f: FrameIndex) -> f64 {
        (f as f64 * self.den as f64) / self.num as f64
    }

    /// Continuous frame position of time `t` (seconds).
    pub fn seconds_to_frames(self, t: f64) -> f64 {
        t * self.num as f64 / self.den as f64
    }
}

impl Default for Rational {
    fn default() -> Self {
        Self::new(60, 1)
    }
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// Wall-clock time, set by the host once per app frame. Seconds since the
/// session epoch. Raw input samples are stamped in this domain.
#[derive(Resource, Debug, Clone, Copy, Default, Reflect)]
#[reflect(Resource)]
pub struct WallClock {
    pub now: f64,
    /// Seconds since the previous app frame (clamped by the host).
    pub dt: f64,
}

impl WallClock {
    /// Advance to `now`; `dt` is clamped so a stall (debugger, window drag)
    /// never makes playback jump by seconds.
    pub fn tick(&mut self, now: f64) {
        self.dt = (now - self.now).clamp(0.0, 0.25);
        self.now = now;
    }
}

pub struct TimeModule;

impl Module for TimeModule {
    fn build(&self, app: &mut AppBuilder) {
        app.register_type::<Rational>()
            .resource_type::<WallClock>(Class::Derived)
            .init_resource::<WallClock>();
    }
}

/// Format a frame as `HH:MM:SS:FF` on a rate's grid (FF = frame within the second).
pub fn timecode(f: FrameIndex, fps: Rational) -> String {
    let fps_round = fps.as_f64().round().max(1.0) as i64;
    let secs = fps.frame_to_seconds(f).floor() as i64;
    let ff = f - (fps.seconds_to_frames(secs as f64).round() as i64);
    format!("{:02}:{:02}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60, ff.clamp(0, fps_round - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rational_normalizes() {
        assert_eq!(Rational::new(120, 2), Rational { num: 60, den: 1 });
        assert_eq!(Rational::new(60000, -1001), Rational { num: -60000, den: 1001 });
    }

    #[test]
    fn frame_seconds_roundtrip_ntsc() {
        let r = Rational::new(60000, 1001);
        for f in [0, 1, 59, 60, 69_469] {
            let t = r.frame_to_seconds(f);
            assert!((r.seconds_to_frames(t) - f as f64).abs() < 1e-6);
        }
    }

    #[test]
    fn timecode_formats() {
        let r = Rational::new(60, 1);
        assert_eq!(timecode(0, r), "00:00:00:00");
        assert_eq!(timecode(61, r), "00:00:01:01");
        assert_eq!(timecode(69_469, r), "00:19:17:49");
    }

    #[test]
    fn wall_clock_clamps_stalls() {
        let mut c = WallClock::default();
        c.tick(0.016);
        assert!((c.dt - 0.016).abs() < 1e-12);
        c.tick(5.0);
        assert_eq!(c.dt, 0.25);
    }
}
