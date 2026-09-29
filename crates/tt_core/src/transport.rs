//! The transport (DESIGN §3): playhead, rate, play/pause.
//!
//! The playhead is continuous: while playing it advances by wall-clock time ×
//! rate, and the displayed frame is `floor(playhead)`. Frames are chosen per
//! app frame from the clock — never "one decode per UI frame" — so playback
//! speed does not depend on the display's refresh rate.
//!
//! Shuttle (J / K / L, as in DaVinci Resolve and most NLEs): L plays forward
//! and J backward, at 1×; pressed again in the same direction, twice as
//! fast each time, up to [`SHUTTLE_MAX`]; the other one turns around at 1×.
//! K plays or pauses. When playback stops, the rate you had set before the
//! shuttle (the capture speed, Q / E) comes back, and so does forward.

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
/// The fastest shuttle speed (J / L pressed again and again).
pub const SHUTTLE_MAX: f64 = 8.0;

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
    /// Playing backward (J).
    pub reverse: bool,
    /// While shuttling (J / L): the rate to go back to when playback stops.
    pub shuttle: Option<f64>,
}

impl Default for Transport {
    fn default() -> Self {
        Self { fps: Rational::default(), frame_count: 0, playhead: 0.0, playing: false, rate: 1.0, looping: true, reverse: false, shuttle: None }
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

    /// Shuttle forward (L) or backward (J): see the module docs.
    pub fn shuttle(&mut self, forward: bool) {
        if !self.has_media() {
            return;
        }
        let same = self.playing && self.reverse != forward;
        self.shuttle = Some(self.shuttle.unwrap_or(self.rate));
        self.rate = if same { (self.rate * 2.0).clamp(1.0, SHUTTLE_MAX) } else { 1.0 };
        if !self.playing {
            // From an end, it starts over from the other one.
            if forward && self.frame() >= self.last_frame() {
                self.playhead = 0.0;
            } else if !forward && self.playhead <= 0.0 {
                self.playhead = self.frame_count as f64 - 1e-6;
            }
        }
        self.reverse = !forward;
        self.playing = true;
    }

    /// Stopped: the shuttle ends; the rate set before it and forward come back.
    fn settle(&mut self) {
        if !self.playing {
            self.reverse = false;
            if let Some(r) = self.shuttle.take() {
                self.rate = r;
            }
        }
    }

    /// The next rate in [`RATES`] up or down from the current one (which auto
    /// speed, or the shuttle, may have set anywhere in between).
    pub fn change_rate(&mut self, faster: bool) {
        let r = self.rate;
        self.rate = if faster {
            RATES.iter().copied().find(|x| *x > r + 1e-9).unwrap_or(r.max(RATES[RATES.len() - 1]))
        } else {
            RATES.iter().rev().copied().find(|x| *x < r - 1e-9).unwrap_or(RATES[0])
        };
    }

    /// Advance by `dt` seconds of wall time.
    pub fn advance(&mut self, dt: f64) {
        if !self.playing || !self.has_media() {
            return;
        }
        let sign = if self.reverse { -1.0 } else { 1.0 };
        self.playhead += sign * dt * self.fps.as_f64() * self.rate;
        let end = self.frame_count as f64;
        if self.playhead >= end || self.playhead < 0.0 {
            if self.looping {
                self.playhead = self.playhead.rem_euclid(end);
            } else {
                self.playhead = if self.reverse { 0.0 } else { self.last_frame() as f64 };
                self.playing = false;
                self.settle();
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
                | GoToStart | GoToEnd | FasterPlayback | SlowerPlayback | ShuttleForward | ShuttleBackward
        )
    });
    if mine.is_empty() {
        return;
    }
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
            ShuttleForward => t.shuttle(true),
            ShuttleBackward => t.shuttle(false),
            _ => {}
        }
        t.settle();
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
    use bevy_ecs::system::RunSystemOnce;

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

    /// J / K / L as in DaVinci Resolve: each press in a direction doubles the
    /// speed (from 1×, up to 8×), the other direction turns around at 1×, and
    /// stopping brings back the rate set before (the capture speed) and forward.
    #[test]
    fn shuttle_speeds_up_turns_around_and_restores_the_rate() {
        let mut t = transport(600);
        t.rate = 0.25; // the capture speed, set with Q
        t.seek(300);
        let mut press = |a: Action| {
            let mut q = PendingActions::default();
            q.push(a);
            let mut w = World::new();
            w.insert_resource(q);
            w.insert_resource(t.clone());
            w.run_system_once(apply_transport_actions).expect("ran");
            t = w.resource::<Transport>().clone();
            (t.playing, t.reverse, t.rate)
        };
        assert_eq!(press(Action::ShuttleForward), (true, false, 1.0), "L: forward at 1×");
        assert_eq!(press(Action::ShuttleForward), (true, false, 2.0));
        assert_eq!(press(Action::ShuttleForward), (true, false, 4.0));
        assert_eq!(press(Action::ShuttleForward), (true, false, 8.0));
        assert_eq!(press(Action::ShuttleForward), (true, false, 8.0), "at most 8×");
        assert_eq!(press(Action::ShuttleBackward), (true, true, 1.0), "J turns around at 1×");
        assert_eq!(press(Action::ShuttleBackward), (true, true, 2.0));
        assert_eq!(press(Action::TogglePlay), (false, false, 0.25), "K stops: your rate and forward are back");
        assert_eq!(press(Action::TogglePlay), (true, false, 0.25), "K again plays at it");
        // L while plain playback runs speeds it up (from at least 1×).
        assert_eq!(press(Action::ShuttleForward), (true, false, 1.0));
        assert_eq!(press(Action::StepForward), (false, false, 0.25), "a step pauses, which ends the shuttle too");
    }

    #[test]
    fn plays_backward_and_loops_or_stops_at_the_start() {
        let mut t = transport(100);
        t.shuttle(false);
        assert!(t.playing && t.reverse);
        t.playhead = 10.0;
        t.advance(0.1); // 6 frames back at 60 fps
        assert_eq!(t.frame(), 4);
        t.advance(0.1);
        assert_eq!(t.frame(), 98, "loops round to the end");
        t.looping = false;
        t.playhead = 3.0;
        t.advance(0.1);
        assert_eq!((t.frame(), t.playing, t.reverse, t.rate), (0, false, false, 1.0), "stops at the start; the shuttle ends");
        // From the start, J starts over from the end.
        t.shuttle(false);
        assert_eq!(t.frame(), 99);
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
