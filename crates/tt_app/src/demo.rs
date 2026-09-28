//! Dev/benchmark: `TT_SKETCH_DEMO=<sprite_truth.json>` with the sprite fixture
//! open. Through the real app path (pointer frames, the tool, the live
//! preview, commit, evaluation, overlays, lanes), a scripted noisy hand
//! (tremor, 250 ms lag, 1 kHz):
//! 1. records a new sketch: holds the button at frame 120, plays at ¼ speed
//!    (Space taps), pauses for a second mid-way, plays on, pauses, releases;
//! 2. edits it: at frame 180, holds 40 px to the right of the sprite;
//! 3. enters its view (Tab) and records a nested sketch there, the hand
//!    seeing the sprite as the view shows it.
//!
//! 4. drives the UI through egui itself (injected pointer events): duplicates
//!    the sketches until the timeline's lanes overflow, box-selects in the
//!    timeline, middle-drags its lanes, right-clicks an outliner row,
//!    box-selects in the outliner, then clicks the ruler, clicks a lane and
//!    double-clicks it;
//! 5. records one more sketch (from frame 300, from ¼×) with anticipatory
//!    speed on, pointer distances in real screen points;
//! 6. tracks the sprite with the Track tool (a square dragged around it on
//!    frame 340, searching inside that sketch) and waits for the tracker;
//! 7. retakes three of its frames one by one: holds 30 px right of the
//!    sprite, steps forward while holding, releases.
//!
//! It logs the error against the truth, the edit's falloff, how central the
//! sprite stays in the view, the nested sketch's error, what the UI steps
//! selected and the rates auto speed chose, then quits. Screenshots go to
//! `<data dir>/screens/`.

use bevy_ecs::prelude::*;
use tt_core::autospeed::{AutoSpeed, AutoSpeedState};
use tt_core::input::{Action, PendingActions};
use tt_core::op::{Inputs, Output};
use tt_core::selection::Selection;
use tt_core::signal::SignalStore;
use tt_core::sketch::falloff_weight;
use tt_core::tool::{ActiveTool, PointerFrame, Tool};
use tt_core::transport::Transport;
use tt_core::view::{ActiveView, map_at};

const LAG: f64 = 0.25;

fn push_action(world: &mut World, a: Action) {
    world.resource_mut::<PendingActions>().push(a);
}
const START_FRAME: i64 = 120;
/// Wall seconds from the press: Space taps (play, pause, play, pause), release.
const TAPS: [f64; 4] = [0.05, 4.0, 5.0, 9.0];
const RELEASE: f64 = 9.4;
const EDIT_FRAME: i64 = 180;
const EDIT_DX: f64 = 40.0;
const EDIT_HOLD: f64 = 0.6;
const NEST_FRAME: i64 = 130;
/// Wall seconds from the nested press: Space taps (play, pause), release.
const NEST_TAPS: [f64; 2] = [0.05, 6.0];
const NEST_RELEASE: f64 = 6.4;
/// Anticipatory speed: a new sketch from this frame, from ¼×; wall seconds from
/// the press: Space taps (play, pause), release.
const AUTO_FRAME: i64 = 300;
const AUTO_TAPS: [f64; 2] = [0.05, 6.0];
const AUTO_RELEASE: f64 = 6.4;
/// The tracker's anchor, and the frames retaken one by one (from here, 3 frames, `RETAKE_DX` right).
const TRACK_FRAME: i64 = 340;
const RETAKE_FRAME: i64 = 360;
const RETAKE_DX: f64 = 30.0;
const RETAKE_HOLD: f64 = 0.35;

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Wait,
    /// On the start frame; the hand settles on the sprite before pressing.
    Ready { since: f64 },
    Record { t0: f64, taps: usize },
    Settle { t0: f64 },
    Edit { t0: f64 },
    Done { t0: f64 },
    /// In the sketch's view; the hand settles on the sprite as shown there.
    InView { since: f64 },
    Nest { t0: f64, taps: usize },
    NestSettle { t0: f64 },
    Ui { t0: f64, step: usize },
    /// Auto speed on, on the start frame; the hand settles on the sprite before pressing.
    AutoReady { since: f64, set: bool },
    Auto { t0: f64, taps: usize },
    AutoSettle { t0: f64 },
    /// T on the sketch just drawn; waiting for its tracker.
    Track { since: f64, started: bool },
    /// Holding on `RETAKE_FRAME + step`, stepping forward while holding.
    Retake { since: f64, step: i64, pressed: bool },
    Finish { at: f64 },
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
    /// Inside the view: the sprite as the view shows it, per frame (view pixels).
    in_view: Vec<[f64; 2]>,
    parent: Option<Entity>,
    /// Screenshots to take (the shell asks the window for them): name, when.
    shots: Vec<(&'static str, f64)>,
    /// A screenshot due now.
    pub shot: Option<&'static str>,
    /// Input events for egui's next frame (the UI script).
    pub inject: Vec<egui::Event>,
    /// While recording with auto speed: the slowest and fastest rate, and what limited it (app frames).
    auto_rates: (f64, f64),
    auto_reasons: Vec<(&'static str, usize)>,
    /// The sketch recorded with auto speed (tracked, then retaken).
    auto_sketch: Option<Entity>,
}

impl SketchDemo {
    pub fn start() -> Option<Self> {
        let path = std::env::var_os("TT_SKETCH_DEMO")?;
        let text = std::fs::read_to_string(&path).map_err(|e| tracing::error!("sketch demo: {e}")).ok()?;
        let json: serde_json::Value = serde_json::from_str(&text).ok()?;
        let truth = json["centers"].as_array()?.iter().filter_map(|c| Some([c[0].as_f64()?, c[1].as_f64()?])).collect();
        Some(Self { truth, phase: Phase::Wait, shown: Vec::new(), rng: 0x9e37_79b9_7f4a_7c15, last: 0.0, before: Vec::new(), in_view: Vec::new(), parent: None, shots: Vec::new(), shot: None, inject: Vec::new(), auto_rates: (f64::INFINITY, 0.0), auto_reasons: Vec::new(), auto_sketch: None })
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
        } else if let Phase::Retake { step, .. } = self.phase {
            // A retake: right of the sprite on the frame being held (no lag: the video is paused).
            let t = self.truth_at(RETAKE_FRAME + step);
            [t[0] + RETAKE_DX, t[1]]
        } else if !self.in_view.is_empty() {
            let i = self.shown.partition_point(|(t, _)| *t <= w - LAG).saturating_sub(1);
            let f = self.shown.get(i).map_or(NEST_FRAME, |s| s.1);
            self.in_view[(f.max(0) as usize).min(self.in_view.len() - 1)]
        } else {
            let i = self.shown.partition_point(|(t, _)| *t <= w - LAG).saturating_sub(1);
            self.truth_at(self.shown.get(i).map_or(START_FRAME, |s| s.1))
        };
        let shake = |ph: f64| 2.0 * ((9.0 * std::f64::consts::TAU * w + ph).sin() * 0.6 + (12.3 * std::f64::consts::TAU * w + 2.0 * ph).sin() * 0.4);
        [p[0] + shake(0.0) + self.normal(), p[1] + shake(1.3) + self.normal()]
    }

    /// Replace this frame's pointer input. Returns true when finished.
    pub fn drive(&mut self, world: &mut World, now: f64, frame: &mut PointerFrame) -> bool {
        if let Some(i) = self.shots.iter().position(|(_, at)| now >= *at) {
            self.shot = Some(self.shots.remove(i).0);
        }
        let t = world.resource::<Transport>().clone();
        // What this app frame presents: the transport after its advance (the
        // state now is what the previous frame showed, one app frame earlier).
        let mut presented = t.clone();
        presented.advance(world.resource::<tt_core::time::WallClock>().dt);
        self.shown.push((now, presented.frame()));
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
                // The scripted strokes play at fixed rates; phase 5 turns auto speed on.
                world.resource_mut::<AutoSpeed>().enabled = false;
                if world.get_resource::<crate::media::Media>().is_none() || now < 1.5 {
                    return false;
                }
                if t.frame() != START_FRAME || t.playing {
                    push(world, Action::Seek(START_FRAME));
                    push(world, Action::SetRate(1)); // 0.25×
                    return false;
                }
                self.phase = Phase::Ready { since: now };
                self.shots.push(("0-speed-flash", now + 0.15));
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
                self.shots.push(("1-recording", now + 2.5));
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
                    self.parent = world.resource::<Selection>().primary();
                    push(world, Action::EnterView);
                    push(world, Action::Seek(NEST_FRAME));
                    self.phase = Phase::InView { since: now };
                }
            }
            Phase::InView { since } => {
                *frame = PointerFrame::default();
                let Some(view) = world.resource::<ActiveView>().0 else { return false };
                if self.in_view.is_empty() && now - since > 0.2 {
                    self.report_view(world, view);
                    self.shots.push(("2-in-view", now));
                    self.in_view = (0..self.truth.len() as i64).map(|f| map_at(world, Some(view), f).from_source(self.truth_at(f))).collect();
                    world.resource_mut::<Selection>().clear(); // the next stroke starts a new sketch, in this view
                }
                if !self.in_view.is_empty() && now - since > 0.2 + LAG + 0.3 {
                    *frame = hold(samples, Some(now - 0.002), None);
                    self.phase = Phase::Nest { t0: now, taps: 0 };
                    self.shots.push(("3-nested-live", now + 3.0));
                    tracing::info!("sketch demo: recording a nested sketch inside the view from frame {NEST_FRAME}");
                }
            }
            Phase::Nest { t0, taps } => {
                let el = now - t0;
                if taps < NEST_TAPS.len() && el >= NEST_TAPS[taps] {
                    push(world, Action::TogglePlay);
                    self.phase = Phase::Nest { t0, taps: taps + 1 };
                }
                let release = el >= NEST_RELEASE;
                *frame = hold(samples, None, release.then_some(now - 0.001));
                if release {
                    self.phase = Phase::NestSettle { t0: now };
                }
            }
            Phase::NestSettle { t0 } => {
                *frame = PointerFrame::default();
                if now - t0 > 0.3 {
                    self.report_nested(world);
                    self.shots.push(("4-nested-done", now));
                    self.phase = Phase::Ui { t0: now + 0.4, step: 0 };
                }
            }
            Phase::Ui { t0, step } => {
                *frame = PointerFrame::default();
                if now >= t0 + 0.15 * step as f64 {
                    match self.ui_step(world, step) {
                        true => self.phase = Phase::Ui { t0, step: step + 1 },
                        false => self.phase = Phase::AutoReady { since: now + 0.8, set: false }, // let the last screenshot arrive
                    }
                }
            }
            Phase::AutoReady { since, set } => {
                *frame = PointerFrame::default();
                if now < since {
                    return false;
                }
                if !set {
                    // A new sketch on the source, followed where the source shows it:
                    // nothing to read ahead in, so the hand drives it.
                    *world.resource_mut::<AutoSpeed>() = AutoSpeed { react_to_hand: true, ..AutoSpeed::default() };
                    world.resource_mut::<ActiveView>().0 = None;
                    world.resource_mut::<ActiveTool>().0 = Tool::Sketch;
                    self.in_view.clear();
                    push(world, Action::DeselectAll);
                    push(world, Action::Seek(AUTO_FRAME));
                    push(world, Action::SetRate(1)); // 0.25×
                    self.phase = Phase::AutoReady { since: now, set: true };
                } else if now - since > LAG + 0.5 && t.frame() == AUTO_FRAME && !t.playing {
                    *frame = PointerFrame { scale: world.resource::<crate::panels::viewport::ViewportMapping>().points_per_canvas(), ..hold(samples, Some(now - 0.002), None) };
                    self.phase = Phase::Auto { t0: now, taps: 0 };
                    self.shots.push(("11-auto-speed", now + 2.5));
                    tracing::info!("sketch demo: recording with anticipatory speed from frame {AUTO_FRAME}, from ×{:.2}", t.rate);
                }
            }
            Phase::Auto { t0, taps } => {
                let el = now - t0;
                if taps < AUTO_TAPS.len() && el >= AUTO_TAPS[taps] {
                    push(world, Action::TogglePlay);
                    self.phase = Phase::Auto { t0, taps: taps + 1 };
                }
                if t.playing {
                    self.auto_rates = (self.auto_rates.0.min(t.rate), self.auto_rates.1.max(t.rate));
                    let reason = world.resource::<AutoSpeedState>().reason;
                    match self.auto_reasons.iter_mut().find(|(r, _)| *r == reason) {
                        Some((_, n)) => *n += 1,
                        None => self.auto_reasons.push((reason, 1)),
                    }
                }
                let release = el >= AUTO_RELEASE;
                // Pointer distances in real screen points (the knobs are in points).
                let scale = world.resource::<crate::panels::viewport::ViewportMapping>().points_per_canvas();
                *frame = PointerFrame { scale, ..hold(samples, None, release.then_some(now - 0.001)) };
                if release {
                    self.phase = Phase::AutoSettle { t0: now };
                }
            }
            Phase::AutoSettle { t0 } => {
                *frame = PointerFrame::default();
                if now - t0 > 0.3 {
                    let total: usize = self.auto_reasons.iter().map(|(_, n)| n).sum();
                    let reasons: Vec<String> = self.auto_reasons.iter().map(|(r, n)| format!("{r} {:.0}%", *n as f64 * 100.0 / total.max(1) as f64)).collect();
                    tracing::info!(
                        "sketch demo: anticipatory speed chose ×{:.2}–×{:.2} while playing (limited by: {}); ×{:.2} again after the release",
                        self.auto_rates.0,
                        self.auto_rates.1,
                        reasons.join(", "),
                        t.rate
                    );
                    self.report_recording(world);
                    world.resource_mut::<AutoSpeed>().enabled = false;
                    self.auto_sketch = world.resource::<Selection>().primary();
                    push(world, Action::Seek(TRACK_FRAME));
                    self.phase = Phase::Track { since: now, started: false };
                }
            }
            Phase::Track { since, started } => {
                *frame = PointerFrame::default();
                if !started {
                    // The Track tool: drag a 24 px square around the sprite (the demo's canvas is the source).
                    if t.frame() == TRACK_FRAME && now - since > 0.2 {
                        world.resource_mut::<ActiveTool>().0 = Tool::Track;
                        let c = self.truth_at(TRACK_FRAME);
                        let (a, b) = ([c[0] - 12.0, c[1] - 12.0], [c[0] + 12.0, c[1] + 12.0]);
                        let samples = vec![[now - 0.03, a[0], a[1]], [now - 0.02, (a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0], [now - 0.01, b[0], b[1]]];
                        *frame = PointerFrame { samples, hover: Some(b), pressed: Some(now - 0.03), down: false, released: Some(now - 0.005), scale: 1.0, ..PointerFrame::default() };
                        self.phase = Phase::Track { since: now, started: true };
                    }
                    return false;
                }
                let busy = world.resource::<tt_track::runner::TrackJobs>().busy() > 0;
                if now - since > 0.5 && !busy {
                    world.resource_mut::<ActiveTool>().0 = Tool::Sketch;
                    self.report_tracker(world);
                    self.shots.push(("12-tracker", now + 0.1));
                    push(world, Action::Seek(RETAKE_FRAME));
                    self.phase = Phase::Retake { since: now + 0.4, step: 0, pressed: false };
                } else if now - since > 120.0 {
                    tracing::warn!("sketch demo: the tracker did not finish in 2 minutes");
                    self.phase = Phase::Finish { at: now };
                }
            }
            Phase::Retake { since, step, pressed } => {
                if !pressed {
                    *frame = PointerFrame::default();
                    if now >= since && t.frame() == RETAKE_FRAME {
                        world.resource_mut::<Selection>().entities = self.auto_sketch.into_iter().collect();
                        self.before = self.path(world, RETAKE_FRAME - 30..RETAKE_FRAME + 30);
                        *frame = hold(samples, Some(now - 0.002), None);
                        self.phase = Phase::Retake { since: now, step: 0, pressed: true };
                    }
                    return false;
                }
                let done = now - since >= RETAKE_HOLD;
                let release = done && step == 2;
                *frame = hold(samples, None, release.then_some(now - 0.001));
                if release {
                    self.shots.push(("13-retake", now + 0.2));
                    self.phase = Phase::Finish { at: now + 0.6 };
                } else if done {
                    push(world, Action::StepForward);
                    self.phase = Phase::Retake { since: now, step: step + 1, pressed: true };
                }
            }
            Phase::Finish { at } => {
                *frame = PointerFrame::default();
                if now >= at {
                    if !self.before.is_empty() && self.auto_sketch.is_some() {
                        self.report_retake(world);
                    }
                    return true;
                }
            }
        }
        false
    }

    /// One step of the scripted UI input. Returns false when the script is over.
    fn ui_step(&mut self, world: &mut World, step: usize) -> bool {
        use egui::{Event, Modifiers, PointerButton, Pos2, Vec2};
        let Some(lanes) = world.resource::<crate::panels::timeline::TimelineUi>().lanes_area else { return true };
        let (Some(list), row) = ({
            let o = world.resource::<crate::panels::outliner::OutlinerState>();
            (o.list_area, o.first_row)
        }) else {
            return true;
        };
        let moved = Event::PointerMoved;
        let button = |pos: Pos2, button: PointerButton, pressed: bool| Event::PointerButton { pos, button, pressed, modifiers: Modifiers::NONE };
        let lerp = |a: Pos2, b: Pos2, u: f32| a + (b - a) * u;
        // A box over the timeline's lanes, a middle-drag on them, a box over the outliner.
        let (ta, tb) = (lanes.min + Vec2::new(200.0, 4.0), lanes.min + Vec2::new(1100.0, 64.0));
        let (ma, mb) = (lanes.center(), lanes.center() - Vec2::new(0.0, 150.0));
        let row_y = row.map_or(list.min.y + 10.0, |r| r.min.y);
        let (oa, ob) = (Pos2::new(list.min.x + 40.0, row_y + 2.0), Pos2::new(list.min.x + 170.0, row_y + 90.0));
        let selected = |world: &World| world.resource::<Selection>().entities.len();
        match step {
            0..=2 => {
                push_action(world, Action::SelectAll);
                push_action(world, Action::Duplicate);
            }
            3 => {
                push_action(world, Action::ExitView);
                push_action(world, Action::DeselectAll);
            }
            4 => {
                let n = crate::panels::outliner::sketch_tree(world).len();
                tracing::info!("sketch demo · UI: {n} sketches (duplicated), lanes area {:.0}×{:.0}", lanes.width(), lanes.height());
                self.shot = Some("5-many-sketches");
            }
            5 => self.inject.extend([moved(ta), button(ta, PointerButton::Primary, true)]),
            6..=9 => self.inject.push(moved(lerp(ta, tb, (step - 5) as f32 / 4.0))),
            10 => self.shot = Some("6-timeline-box"),
            11 => self.inject.push(button(tb, PointerButton::Primary, false)),
            12 => tracing::info!("sketch demo · UI: a box over the timeline's lanes selected {}", selected(world)),
            13 => self.inject.extend([moved(ma), button(ma, PointerButton::Middle, true)]),
            14..=17 => self.inject.push(moved(lerp(ma, mb, (step - 13) as f32 / 4.0))),
            18 => self.inject.push(button(mb, PointerButton::Middle, false)),
            19 => {
                let scroll = world.resource::<crate::panels::timeline::TimelineView>().lane_scroll;
                tracing::info!("sketch demo · UI: a middle-drag up scrolled the lanes to {scroll:.0} pt");
                self.shot = Some("7-timeline-scrolled");
            }
            20 => {
                let p = row.map_or(list.center(), |r| r.min + Vec2::new(60.0, r.height() / 2.0));
                self.inject.extend([moved(p), button(p, PointerButton::Secondary, true), button(p, PointerButton::Secondary, false)]);
            }
            21 => self.shot = Some("8-outliner-menu"),
            22 => tracing::info!("sketch demo · UI: right-click on the first outliner row selected {} ({:?})", selected(world), world.resource::<Selection>().primary().map(|e| crate::panels::outliner::label(world, e))),
            23 => {
                // Close the menu with a click on an empty part of the outliner.
                let p = Pos2::new(list.min.x + 100.0, list.max.y - 20.0);
                self.inject.extend([moved(p), button(p, PointerButton::Primary, true), button(p, PointerButton::Primary, false)]);
            }
            24 => self.inject.extend([moved(oa), button(oa, PointerButton::Primary, true)]),
            25..=28 => self.inject.push(moved(lerp(oa, ob, (step - 24) as f32 / 4.0))),
            29 => self.shot = Some("9-outliner-box"),
            30 => self.inject.push(button(ob, PointerButton::Primary, false)),
            31 => tracing::info!("sketch demo · UI: a box over the outliner selected {}", selected(world)),
            32 => {
                // The Settings tab, next to the Inspector's above the right column.
                let panel = world.resource::<crate::panels::viewport::ViewportMapping>().panel;
                let p = Pos2::new(panel.max.x + 100.0, panel.min.y - 12.0);
                self.inject.extend([moved(p), button(p, PointerButton::Primary, true), button(p, PointerButton::Primary, false)]);
            }
            33 => self.shot = Some("10-settings"),
            // Clicks on the timeline: the ruler seeks, a lane selects, a double-click enters its view.
            34 => {
                tracing::info!("sketch demo · UI: the playhead is on frame {}", world.resource::<Transport>().frame());
                let p = Pos2::new(lanes.min.x + 400.0, lanes.min.y - 20.0);
                self.inject.extend([moved(p), button(p, PointerButton::Primary, true), button(p, PointerButton::Primary, false)]);
            }
            35 => tracing::info!("sketch demo · UI: a click on the ruler moved it to frame {}", world.resource::<Transport>().frame()),
            36 => {
                world.resource_mut::<Selection>().clear();
                let p = Pos2::new(lanes.min.x + 300.0, lanes.min.y + 9.0);
                self.inject.extend([moved(p), button(p, PointerButton::Primary, true), button(p, PointerButton::Primary, false)]);
            }
            37 => {
                let primary = world.resource::<Selection>().primary().map(|e| crate::panels::outliner::label(world, e));
                tracing::info!("sketch demo · UI: a click on a lane selected {} ({primary:?})", selected(world));
            }
            // (Long enough after that click for egui not to count a triple click.)
            38..=41 => {}
            42 => {
                let p = Pos2::new(lanes.min.x + 300.0, lanes.min.y + 9.0);
                let click = [button(p, PointerButton::Primary, true), button(p, PointerButton::Primary, false)];
                self.inject.push(moved(p));
                self.inject.extend(click.clone());
                self.inject.extend(click);
            }
            43 => {}
            44 => {
                let view = world.resource::<ActiveView>().0.map(|v| crate::panels::outliner::label(world, v));
                tracing::info!("sketch demo · UI: a double-click on that lane entered {view:?}");
                self.shot = Some("11-lane-double-click");
            }
            _ => return false,
        }
        true
    }

    fn path(&self, world: &World, frames: std::ops::Range<i64>) -> Vec<Option<[f32; 6]>> {
        let Some(sketch) = world.resource::<Selection>().primary() else { return Vec::new() };
        let Some(sig) = world.get::<Output>(sketch).and_then(|o| world.resource::<SignalStore>().get(o.0)) else { return Vec::new() };
        frames.map(|f| sig.get(f).map(|v| v.try_into().unwrap())).collect()
    }

    /// The tracker T made on the auto-speed sketch, against the truth.
    fn report_tracker(&self, world: &World) {
        let Some(tracker) = world.resource::<Selection>().primary().filter(|e| tt_track::is_tracker(world, *e)) else {
            tracing::warn!("sketch demo: no tracker was made");
            return;
        };
        let Some(sig) = world.get::<Output>(tracker).and_then(|o| world.resource::<SignalStore>().get(o.0)) else { return };
        let guide = self.auto_sketch.and_then(|s| world.get::<Output>(s)).and_then(|o| world.resource::<SignalStore>().get(o.0));
        let (mut e, mut g, mut lost) = (Vec::new(), Vec::new(), 0);
        for f in 0..self.truth.len() as i64 {
            let Some(v) = sig.get(f) else { continue };
            let t = self.truth[f as usize];
            e.push((v[0] as f64 - t[0]).hypot(v[1] as f64 - t[1]));
            lost += (tt_track::flags(v) != 0) as usize;
            if let Some(gv) = guide.and_then(|s| s.get(f)) {
                g.push((gv[0] as f64 - t[0]).hypot(gv[1] as f64 - t[1]));
            }
        }
        let q = |v: &mut Vec<f64>, p: f64| {
            v.sort_by(f64::total_cmp);
            v.get(((v.len().max(1) - 1) as f64 * p).round() as usize).copied().unwrap_or(f64::NAN)
        };
        let (n, m, p95, max) = (e.len(), q(&mut e, 0.5), q(&mut e, 0.95), q(&mut e, 1.0));
        tracing::info!(
            "sketch demo: tracker from a square dragged around the sprite on frame {TRACK_FRAME}: {n} frames · error median {m:.2} px, p95 {p95:.2} px, max {max:.2} px · {lost} flagged · its sketch alone: median {:.2} px",
            q(&mut g, 0.5)
        );
    }

    /// The three frames retaken one by one, and their neighbours.
    fn report_retake(&self, world: &World) {
        let Some(sig) = self.auto_sketch.and_then(|s| world.get::<Output>(s)).and_then(|o| world.resource::<SignalStore>().get(o.0)) else { return };
        let line: Vec<String> = (-2..5i64)
            .map(|k| {
                let f = RETAKE_FRAME + k;
                let before = self.before.get((k + 30) as usize).copied().flatten();
                let moved = sig.get(f).zip(before).map_or(f64::NAN, |(a, b)| a[0] as f64 - b[0] as f64);
                let to_truth = sig.get(f).map_or(f64::NAN, |a| a[0] as f64 - self.truth_at(f)[0]);
                format!("{f}: moved {moved:+.1} (now {to_truth:+.1} from the sprite)")
            })
            .collect();
        tracing::info!("sketch demo: retook {RETAKE_FRAME}-{} one by one, {RETAKE_DX} px right · {}", RETAKE_FRAME + 2, line.join(" · "));
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

    fn report_view(&self, world: &World, view: Entity) {
        let Some(sketch) = self.parent else { return };
        let Some(sig) = world.get::<Output>(sketch).and_then(|o| world.resource::<SignalStore>().get(o.0)) else { return };
        let (mut inside, mut n) = (0, 0);
        for f in 0..self.truth.len() as i64 {
            if sig.get(f).is_none() {
                continue;
            }
            let m = map_at(world, Some(view), f);
            let p = m.from_source(self.truth_at(f));
            n += 1;
            if (p[0] - m.canvas[0] / 2.0).abs() <= 0.15 * m.canvas[0] && (p[1] - m.canvas[1] / 2.0).abs() <= 0.15 * m.canvas[1] {
                inside += 1;
            }
        }
        let m = map_at(world, Some(view), NEST_FRAME);
        tracing::info!(
            "sketch demo: in the view (canvas {:.0}×{:.0}, {:.2} source px per view px at frame {NEST_FRAME}) the sprite stays in the central 30% on {:.1}% of {n} frames",
            m.canvas[0],
            m.canvas[1],
            m.a,
            inside as f64 * 100.0 / n.max(1) as f64
        );
        // Size jitter: the region's height from frame to frame vs the view's zoom.
        let frames: Vec<i64> = (0..self.truth.len() as i64).filter(|f| sig.get(*f).is_some()).collect();
        let change = |a: f64, b: f64| (b / a).ln().abs() * 100.0;
        let (mut region, mut zoom) = (Vec::new(), Vec::new());
        for w in frames.windows(2).filter(|w| w[1] == w[0] + 1) {
            let (r0, r1) = (sig.get(w[0]).unwrap(), sig.get(w[1]).unwrap());
            region.push(change((r0[5] - r0[3]) as f64, (r1[5] - r1[3]) as f64));
            zoom.push(change(map_at(world, Some(view), w[0]).a, map_at(world, Some(view), w[1]).a));
        }
        let stats = |mut v: Vec<f64>| {
            v.sort_by(f64::total_cmp);
            (v.get(v.len() / 2).copied().unwrap_or(0.0), v.last().copied().unwrap_or(0.0))
        };
        let ((rm, rx), (zm, zx)) = (stats(region), stats(zoom));
        tracing::info!("sketch demo: size change per frame — region height median {rm:.2}%, max {rx:.2}%; view zoom median {zm:.3}%, max {zx:.3}%");
    }

    fn report_nested(&self, world: &World) {
        let path = self.path(world, 0..self.truth.len() as i64);
        let mut errors: Vec<f64> = path
            .iter()
            .enumerate()
            .filter_map(|(f, v)| {
                let v = (*v)?;
                let t = self.truth[f];
                Some((v[0] as f64 - t[0]).hypot(v[1] as f64 - t[1]))
            })
            .collect();
        errors.sort_by(f64::total_cmp);
        let n = errors.len();
        let q = |f: f64| errors.get(((n.max(1) - 1) as f64 * f).round() as usize).copied().unwrap_or(f64::NAN);
        let home = world.resource::<Selection>().primary().and_then(|s| tt_core::view::home_of(world, s));
        tracing::info!(
            "sketch demo: nested sketch drawn in the view ({}): {n} frames, point error in source pixels median {:.2} px, p95 {:.2} px",
            if home.is_some() { "home = the view" } else { "home = source?!" },
            q(0.5),
            q(0.95)
        );
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
