//! Dev: `TT_SCENE_DEMO=1` with the sprite fixture open
//! (`trackertools fixtures/sprite_1080p60.mp4`, a scratch `TT_DATA_DIR`): a
//! scene with one of everything the visual language draws, for screenshots
//! and review. A sketch over the sprite, a template tracker in it, one with
//! no sketch (frames drawn by hand over its results), a CoTracker with two
//! reset points (where it can run here), a manual dot and a subject; the
//! playhead on a drawn frame, the tracker with the drawing selected. Built
//! once the video is open: the sketch first, the rest once it has frames.

use bevy_ecs::name::Name;
use bevy_ecs::prelude::*;
use tt_core::history::edit;
use tt_core::op::{Inputs, Operator, Output};
use tt_core::signal::SignalStore;
use tt_core::sketch::{BOX_CHANNELS, Capture, ClockMap, STREAM_CHANNELS, SketchParams, Stroke};
use tt_core::time::FrameIndex;
use tt_track::look::Look;
use tt_track::{Method, NewTrackers, TrackRun};

/// The scene being built. `TT_SCENE_DEMO=settings` then shows the Settings
/// tab (a click egui sees, at `tab`), `=export` opens the stabilized export
/// window on the subject once it has frames (`=export-framed`: zoomed in and
/// moved).
pub struct Scene {
    sketch: Option<Entity>,
    subject: Option<Entity>,
    mode: String,
    /// Built: frames since (the Settings tab is clicked a few frames later).
    after: Option<u32>,
    /// Input events for egui's next frame.
    pub inject: Vec<egui::Event>,
    /// Where the Settings tab is (points).
    pub tab: Option<egui::Pos2>,
}

impl Scene {
    pub fn start() -> Option<Self> {
        let mode = std::env::var("TT_SCENE_DEMO").ok()?;
        // (The Settings tab in the default layout of a 1600 × 950 window; TT_SCENE_TAB=x,y for another.)
        let tab = std::env::var("TT_SCENE_TAB").ok().and_then(|s| s.split_once(',').and_then(|(x, y)| Some(egui::pos2(x.trim().parse().ok()?, y.trim().parse().ok()?))));
        Some(Self { sketch: None, subject: None, mode, after: None, inject: Vec::new(), tab: tab.or(Some(egui::pos2(1427.0, 35.0))) })
    }

    /// One app frame (the video is open). True: done.
    pub fn drive(&mut self, world: &mut World) -> bool {
        if let Some(n) = self.after.as_mut() {
            // The export waits for the subject's frames (its members tracking).
            if self.mode.starts_with("export")
                && let Some(s) = self.subject
            {
                if tt_track::export::subject_path(world, s).len() < 100 && *n < 3000 {
                    *n += 1;
                    return false;
                }
                // `=export-framed`: zoomed in and moved, for the preview.
                if self.mode == "export-framed" {
                    let mut d = world.resource_mut::<tt_track::export::StabilizerDefaults>();
                    (d.fill, d.zoom, d.offset) = (false, 1.6, [0.08, -0.05]);
                }
                crate::panels::export::open(world, crate::panels::export::Kind::Stabilized, crate::panels::export::Source::Subject(s));
                self.subject = None;
                return false;
            }
            *n += 1;
            use egui::{Event, Modifiers, PointerButton};
            if *n == 5
                && self.mode == "settings"
                && let Some(p) = self.tab
            {
                self.inject.extend([
                    Event::PointerMoved(p),
                    Event::PointerButton { pos: p, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE },
                    Event::PointerButton { pos: p, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE },
                ]);
            }
            // Then down the panel to the views' preview.
            if (10..24).contains(n)
                && self.mode == "settings"
                && let Some(p) = self.tab
            {
                let over = p + egui::vec2(0.0, 500.0);
                self.inject.extend([
                    Event::PointerMoved(over),
                    Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: egui::vec2(0.0, -40.0), modifiers: Modifiers::NONE, phase: egui::TouchPhase::Move },
                ]);
            }
            return *n > 45;
        }
        match self.sketch {
            None => {
                tracing::info!("scene demo: the sketch");
                self.sketch = sketch(world, 450..750);
                if self.sketch.is_none() {
                    self.after = Some(0);
                }
            }
            Some(s) => {
                let ready = world.get::<Output>(s).and_then(|o| world.resource::<SignalStore>().get(o.0)).is_some_and(|sig| sig.get(600).is_some());
                if ready {
                    self.subject = rest(world, s);
                    self.after = Some(0);
                }
            }
        }
        false
    }
}

/// The sprite's centre at a frame (the fixture's formula, as the tests have it).
fn sprite(f: f64) -> [f64; 2] {
    let t = f / 60.0;
    [
        (950.0 + 500.0 * (0.9 * t).sin() + 60.0 * (5.3 * t).sin()).floor() + 10.5,
        (530.0 + 300.0 * (1.3 * t + 0.7).sin() + 40.0 * (4.1 * t).sin()).floor() + 10.5,
    ]
}

/// A hand-drawn sketch over `frames`: one stroke following the sprite with a wobble, as the Sketch tool records one.
fn sketch(world: &mut World, frames: std::ops::Range<FrameIndex>) -> Option<Entity> {
    let secs = (frames.end - frames.start) as f64 / 60.0;
    let samples: Vec<f32> = (0..=(secs * 1000.0) as usize)
        .flat_map(|i| {
            let t = i as f64 / 1000.0;
            let p = sprite(frames.start as f64 + 60.0 * t - 0.5);
            [t as f32, (p[0] + 4.0 * (7.0 * t).sin()) as f32, (p[1] + 3.0 * (5.0 * t).cos()) as f32]
        })
        .collect();
    let mut clock = ClockMap::default();
    for i in 0..=(secs * 100.0) as usize {
        let t = i as f64 / 100.0;
        clock.push(t, frames.start as f64 + 60.0 * t, true);
    }
    let mut made = None;
    edit(world, "Scene: a sketch", |tx| {
        let stream = tx.create_signal(STREAM_CHANNELS);
        tx.signal(stream).write(0, &samples);
        let n = samples.len() / STREAM_CHANNELS;
        let s = tx.spawn((Name::new("Stroke 1"), Capture { rate: 1.0, samples: n as u32 }, clock, Stroke { lag: 0.0, ..Stroke::default() }, Output(stream)));
        let out = tx.create_signal(BOX_CHANNELS);
        made = Some(tx.spawn((Name::new("Sketch 1"), Operator { kind: "sketch".into() }, Inputs(vec![("stroke".into(), s)]), Output(out), SketchParams { lag: 0.0, ..SketchParams::default() })));
    });
    made
}

/// Everything else, once the sketch has frames. The subject made.
fn rest(world: &mut World, sketch: Entity) -> Option<Entity> {
    tracing::info!("scene demo: trackers, drawing, a manual dot, a subject");
    world.resource_mut::<NewTrackers>().run = TrackRun::Both;
    let at = |f: FrameIndex, d: [f64; 2]| {
        let p = sprite(f as f64);
        [p[0] + d[0], p[1] + d[1]]
    };
    let look = |f: FrameIndex| Look::new(f, sprite(f as f64), [10.5, 10.5]);
    // A template tracker inside the sketch.
    let guided = tt_track::add_tracker_with_look(world, sketch, look(600));
    // A tracker with no sketch, and frames drawn by hand over its results (a person moved its point there).
    let loose = tt_track::add_unguided_tracker(world, look(600));
    if let Some(t) = loose {
        let points: Vec<(FrameIndex, Option<[f64; 2]>)> = (610..=640).map(|f| (f, Some(at(f, [18.0, -12.0])))).collect();
        edit(world, "Scene: drawn frames", |tx| tt_track::human::write_drawn(tx, t, &points));
    }
    // A CoTracker with two reset points, where it can run.
    if tt_track::job::cotracker_availability().is_ok() {
        world.resource_mut::<NewTrackers>().method = Method::CoTracker;
        if let Some(c) = tt_track::add_unguided_tracker(world, look(600)) {
            tt_track::set_reset_point(world, c, Look::new(640, at(640, [0.0, 0.0]), [6.0, 6.0]));
            tt_core::span::set_span(world, c, tt_core::span::Span::new(560, 700));
        }
        world.resource_mut::<NewTrackers>().method = Method::Template;
    }
    // A manual dot: a path drawn by hand.
    let mut dot = None;
    edit(world, "Scene: a manual dot", |tx| {
        let d = tt_track::human::spawn_manual_dot(tx, "Manual dot 1".into(), 560);
        let points: Vec<(FrameIndex, Option<[f64; 2]>)> = (560..=680).map(|f| (f, Some(at(f, [45.0 + 3.0 * (f as f64 * 0.3).sin(), 35.0])))).collect();
        tt_track::human::write_drawn(tx, d, &points);
        dot = Some(d);
    });
    // A subject carried by the guided tracker and the dot.
    let subject = match (guided, dot) {
        (Some(g), Some(d)) => tt_core::subject::make_subject(world, &[g, d], 600),
        _ => None,
    };
    world.resource_mut::<tt_core::transport::Transport>().seek(625);
    if let Some(t) = loose {
        world.resource_mut::<tt_core::selection::Selection>().select_only(t);
    }
    subject
}
