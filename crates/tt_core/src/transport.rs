//! The transport (DESIGN §3): playhead, rate, play/pause.
//!
//! The playhead is continuous: while playing it advances by wall-clock time ×
//! rate, and the displayed frame is `floor(playhead)`. Frames are chosen per
//! app frame from the clock — never "one decode per UI frame" — so playback
//! speed does not depend on the display's refresh rate.

use bevy_ecs::prelude::*;
use bevy_reflect::Reflect;

use crate::app::{AppBuilder, Module, PostSet, Set};
use crate::input::{Action, PendingActions};
use crate::meta::Class;
use crate::time::{FrameIndex, Rational, WallClock};

/// Playback rates offered by `[` / `]` (fractions of real time).
pub const RATES: [f64; 6] = [0.1, 0.25, 0.5, 1.0, 2.0, 4.0];
/// Frames moved by JumpForward/JumpBackward.
pub const JUMP: FrameIndex = 10;

#[derive(Resource, Debug, Clone, Reflect)]
#[reflect(Resource)]
pub struct Transport {
    pub fps: Rational,
    /// Frames in the media; 0 when nothing is loaded.
    pub frame_count: FrameIndex,
    /// Continuous frame position; the displayed frame is `floor(playhead)`.
    pub playhead: f64,
    pub playing: bool,
    /// Fraction of real time (1.0 = normal speed).
    pub rate: f64,
    pub looping: bool,
}

impl Default for Transport {
    fn default() -> Self {
        Self { fps: Rational::default(), frame_count: 0, playhead: 0.0, playing: false, rate: 1.0, looping: true }
    }
}

impl Transport {
    pub fn has_media(&self) -> bool {
        self.frame_count > 0
    }

    pub fn last_frame(&self) -> FrameIndex {
        (self.frame_count - 1).max(0)
    }

    /// The frame shown right now.
    pub fn frame(&self) -> FrameIndex {
        (self.playhead.floor() as FrameIndex).clamp(0, self.last_frame())
    }

    /// Jump to a frame (clamped). Pauses, like every explicit seek.
    pub fn seek(&mut self, f: FrameIndex) {
        self.playing = false;
        self.playhead = f.clamp(0, self.last_frame()) as f64;
    }

    pub fn step(&mut self, delta: FrameIndex) {
        let f = self.frame() + delta;
        self.seek(f);
    }

    pub fn toggle_play(&mut self) {
        if !self.has_media() {
            return;
        }
        if !self.playing && self.frame() >= self.last_frame() {
            self.playhead = 0.0; // replay from the start
        }
        self.playing = !self.playing;
    }

    /// The next rate in [`RATES`] up or down from the current one (which auto
    /// speed may have set anywhere in between).
    pub fn change_rate(&mut self, faster: bool) {
        let r = self.rate;
        self.rate = if faster {
            RATES.iter().copied().find(|x| *x > r + 1e-9).unwrap_or(RATES[RATES.len() - 1])
        } else {
            RATES.iter().rev().copied().find(|x| *x < r - 1e-9).unwrap_or(RATES[0])
        };
    }

    /// Advance by `dt` seconds of wall time.
    pub fn advance(&mut self, dt: f64) {
        if !self.playing || !self.has_media() {
            return;
        }
        self.playhead += dt * self.fps.as_f64() * self.rate;
        let end = self.frame_count as f64;
        if self.playhead >= end {
            if self.looping {
                self.playhead = self.playhead.rem_euclid(end);
            } else {
                self.playhead = self.last_frame() as f64;
                self.playing = false;
            }
        }
    }
}

fn apply_transport_actions(mut actions: ResMut<PendingActions>, mut t: ResMut<Transport>) {
    use Action::*;
    let mine = actions.take(|a| {
        matches!(
            a,
            Seek(_) | SetRate(_) | ToggleLoop | TogglePlay | StepForward | StepBackward | JumpForward | JumpBackward
                | GoToStart | GoToEnd | FasterPlayback | SlowerPlayback
        )
    });
    for a in mine {
        match a {
            Seek(f) => t.seek(f),
            SetRate(i) => t.rate = RATES[(i as usize).min(RATES.len() - 1)],
            ToggleLoop => t.looping = !t.looping,
            TogglePlay => t.toggle_play(),
            StepForward => t.step(1),
            StepBackward => t.step(-1),
            JumpForward => t.step(JUMP),
            JumpBackward => t.step(-JUMP),
            GoToStart => t.seek(0),
            GoToEnd => {
                let last = t.last_frame();
                t.seek(last)
            }
            FasterPlayback => t.change_rate(true),
            SlowerPlayback => t.change_rate(false),
            _ => {}
        }
    }
}

fn advance_playhead(clock: Res<WallClock>, mut t: ResMut<Transport>) {
    if t.playing {
        t.advance(clock.dt);
    }
}

pub struct TransportModule;

impl Module for TransportModule {
    fn build(&self, app: &mut AppBuilder) {
        app.resource_type::<Transport>(Class::Session)
            .init_resource::<Transport>()
            // Actions first so a Space press and the clock tick land in the same frame.
            .add_systems((apply_transport_actions, advance_playhead).chain().in_set(Set::Transport))
            // Panel gestures (scrubbing) apply before the frame is presented.
            .add_post_ui_systems(apply_transport_actions.in_set(PostSet::Intents));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transport(frames: FrameIndex) -> Transport {
        Transport { frame_count: frames, fps: Rational::new(60, 1), ..Default::default() }
    }

    #[test]
    fn advances_by_wall_time_and_rate() {
        let mut t = transport(600);
        t.toggle_play();
        t.advance(0.5);
        assert_eq!(t.frame(), 30);
        t.rate = 0.25;
        t.advance(1.0);
        assert_eq!(t.frame(), 45);
    }

    #[test]
    fn loops_or_stops_at_end() {
        let mut t = transport(100);
        t.playhead = 99.0;
        t.playing = true;
        t.advance(1.0 / 30.0); // +2 frames
        assert_eq!(t.frame(), 1);
        assert!(t.playing);

        t.looping = false;
        t.playhead = 99.0;
        t.advance(1.0);
        assert_eq!(t.frame(), 99);
        assert!(!t.playing);
    }

    #[test]
    fn seek_and_step_clamp_and_pause() {
        let mut t = transport(100);
        t.playing = true;
        t.seek(500);
        assert_eq!(t.frame(), 99);
        assert!(!t.playing);
        t.step(-1000);
        assert_eq!(t.frame(), 0);
    }

    #[test]
    fn play_at_end_restarts() {
        let mut t = transport(100);
        t.seek(99);
        t.toggle_play();
        assert!(t.playing);
        assert_eq!(t.frame(), 0);
    }

    #[test]
    fn rate_steps_through_table() {
        let mut t = transport(10);
        t.change_rate(false);
        assert_eq!(t.rate, 0.5);
        for _ in 0..10 {
            t.change_rate(false);
        }
        assert_eq!(t.rate, 0.1);
        // From a rate between the steps (auto speed), the next step either way.
        t.rate = 0.35;
        t.change_rate(true);
        assert_eq!(t.rate, 0.5);
        t.rate = 0.35;
        t.change_rate(false);
        assert_eq!(t.rate, 0.25);
    }
}
