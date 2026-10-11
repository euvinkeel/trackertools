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

/// `=export-fill`: how long the window stays as it opened before the drag (s).
const DRAG_AFTER: f64 = 15.0;

/// The scene being built. `TT_SCENE_DEMO=settings` then shows the Settings
/// tab (a click egui sees, at `tab`), `=export` opens the stabilized export
/// window on the subject once it has frames (`=export-framed`: zoomed in and
/// moved; `=export-fill`: zoomed in to hide the black edges and moved, then,
/// [`DRAG_AFTER`] seconds later, the picture dragged in the preview).
pub struct Scene {
    sketch: Option<Entity>,
    subject: Option<Entity>,
    mode: String,
    /// Built: frames since (the Settings tab is clicked a few frames later).
    after: Option<u32>,
    /// The export window opened (wall clock), and the drag's steps since.
    opened: Option<f64>,
    dragged: u32,
    /// Input events for egui's next frame.
    pub inject: Vec<egui::Event>,
    /// Where the Settings tab is (points).
    pub tab: Option<egui::Pos2>,
    /// `=layer-render`: its export started.
    rendering: bool,
}

impl Scene {
    pub fn start() -> Option<Self> {
        let mode = std::env::var("TT_SCENE_DEMO").ok()?;
        // (The Settings tab in the default layout of a 1600 × 950 window; TT_SCENE_TAB=x,y for another.)
        let tab = std::env::var("TT_SCENE_TAB").ok().and_then(|s| s.split_once(',').and_then(|(x, y)| Some(egui::pos2(x.trim().parse().ok()?, y.trim().parse().ok()?))));
        Some(Self { sketch: None, subject: None, mode, after: None, opened: None, dragged: 0, inject: Vec::new(), tab: tab.or(Some(egui::pos2(1427.0, 35.0))), rendering: false })
    }

    /// One app frame (the video is open). True: done.
    pub fn drive(&mut self, world: &mut World) -> bool {
        // `=layer-render`: the export starts once the layers are worked out.
        if self.mode == "layer-render"
            && self.after.is_some()
            && !self.rendering
            && let Ok(out) = std::env::var("TT_SCENE_OUT")
        {
            self.rendering = crate::panels::layer_export::start_now(world, std::env::var_os("TT_SCENE_ALPHA").is_some(), out.into());
            return false;
        }
        if let Some(n) = self.after.as_mut() {
            // The export waits for the subject's frames (its members tracking).
            if self.mode.starts_with("export")
                && let Some(s) = self.subject
            {
                if tt_track::export::subject_path(world, s).len() < 100 && *n < 3000 {
                    *n += 1;
                    return false;
                }
                // `=export-framed`: zoomed in and moved, for the preview; `=export-fill`: zoomed in just enough.
                let framing = match self.mode.as_str() {
                    "export-framed" => Some((false, 1.6, [0.08, -0.05])),
                    "export-fill" => Some((true, 1.0, [0.08, -0.05])),
                    _ => None,
                };
                if let Some(f) = framing {
                    let mut d = world.resource_mut::<tt_track::export::StabilizerDefaults>();
                    (d.fill, d.zoom, d.offset) = f;
                }
                crate::panels::export::open(world, crate::panels::export::Kind::Stabilized, crate::panels::export::Source::Subject(s));
                tracing::info!("scene demo: the export window");
                self.subject = None;
                return false;
            }
            *n += 1;
            use egui::{Event, Modifiers, PointerButton};
            if self.mode == "export-fill" {
                return self.drag_the_preview(world);
            }
            // (`=settings-top`: the tab opened, not scrolled: Updates.)
            if *n == 5
                && self.mode.starts_with("settings")
                && let Some(p) = self.tab
            {
                self.inject.extend([
                    Event::PointerMoved(p),
                    Event::PointerButton { pos: p, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE },
                    Event::PointerButton { pos: p, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE },
                ]);
            }
            // `=cursor`: the pointer over the video (the brush shows its pattern under it; TT_SCENE_HOVER=x,y).
            if *n >= 5 && self.mode == "cursor" {
                let at = std::env::var("TT_SCENE_HOVER").ok().and_then(|s| s.split_once(',').and_then(|(x, y)| Some(egui::pos2(x.trim().parse().ok()?, y.trim().parse().ok()?))));
                self.inject.push(Event::PointerMoved(at.unwrap_or(egui::pos2(640.0, 420.0))));
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
            // (`=cursor` keeps the pointer there.)
            return *n > 45 && self.mode != "cursor";
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
                    // `=focus`: a SpringFocus on the subject, moving over to the tracker
                    // selected on frame 600 (the playhead is at 625: mid-move), selected.
                    if self.mode == "focus"
                        && let (Some(sub), Some(t)) = (self.subject, world.resource::<tt_core::selection::Selection>().primary())
                    {
                        let f = tt_core::focus::make_focus(world, sub, 450);
                        tt_core::focus::focus_on(world, f, 600, t);
                        tt_core::focus::set_focus(world, f, "slower", |p| p.move_time = 1.5);
                        world.resource_mut::<tt_core::selection::Selection>().select_only(f);
                    }
                    // `=layer`: the files in TT_SCENE_MEDIA (`;` between them) attached, the
                    // first to the subject, the next to the tracker selected; the first selected.
                    // (`=export…` with TT_SCENE_MEDIA too: the export draws them.)
                    if self.mode.starts_with("layer") || (self.mode.starts_with("export") && std::env::var_os("TT_SCENE_MEDIA").is_some()) {
                        let loose = world.resource::<tt_core::selection::Selection>().primary();
                        let media = std::env::var("TT_SCENE_MEDIA").unwrap_or_default();
                        let targets = [self.subject, loose];
                        let mut first = None;
                        for (path, target) in media.split(';').filter(|p| !p.is_empty()).zip(targets) {
                            if let Some(t) = target {
                                let l = crate::layers::attach_file(world, t, std::path::Path::new(path));
                                first = first.or(l);
                            }
                        }
                        if let Some(l) = first {
                            world.resource_mut::<tt_core::selection::Selection>().select_only(l);
                        }
                        // `=layer-export`: and the Export layers window; `=layer-render`: and
                        // its export started (frames 560–620, to TT_SCENE_OUT; with
                        // TT_SCENE_ALPHA, alone on transparency).
                        if self.mode == "layer-export" || self.mode == "layer-render" {
                            crate::panels::layer_export::open(world);
                        }

                    }
                    // `=sketch`: a second sketch (its own colour), the first selected (its moving
                    // outline); `=view`: then inside the first's view (outside it dimmed).
                    if self.mode == "sketch" || self.mode == "view" {
                        sketch_at(world, 450..750, "Sketch 2", [160.0, -90.0]);
                        world.resource_mut::<tt_core::selection::Selection>().select_only(s);
                        if self.mode == "view" {
                            world.resource_mut::<tt_core::input::PendingActions>().push(tt_core::input::Action::EnterView);
                        }
                    }
                }
            }
        }
        false
    }

    /// `=export-fill`, after the window opens: [`DRAG_AFTER`] seconds as it is,
    /// then a drag of the picture in the preview, 40 points right and 20 down. True: done.
    fn drag_the_preview(&mut self, world: &World) -> bool {
        use egui::{Event, Modifiers, PointerButton};
        let now = world.resource::<tt_core::time::WallClock>().now;
        let opened = *self.opened.get_or_insert(now);
        if now - opened < DRAG_AFTER {
            return false;
        }
        let Some(p) = crate::panels::export::preview_rect(world).map(|r| r.center()) else { return true };
        let (k, button) = (self.dragged, |pos, pressed| Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE });
        self.dragged += 1;
        match k {
            0 => {
                tracing::info!("scene demo: dragging the picture in the preview");
                self.inject.extend([Event::PointerMoved(p), button(p, true)]);
            }
            1..=20 => self.inject.push(Event::PointerMoved(p + egui::vec2(2.0 * k as f32, k as f32))),
            21 => self.inject.push(button(p + egui::vec2(40.0, 20.0), false)),
            // (The release goes in with the next frame's input: the scene stays until then.)
            _ => {
                tracing::info!("scene demo: dragged");
                return true;
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
    sketch_at(world, frames, "Sketch 1", [0.0, 0.0])
}

/// [`sketch`], named `name`, `off` (px) from the sprite.
fn sketch_at(world: &mut World, frames: std::ops::Range<FrameIndex>, name: &str, off: [f64; 2]) -> Option<Entity> {
    let secs = (frames.end - frames.start) as f64 / 60.0;
    let samples: Vec<f32> = (0..=(secs * 1000.0) as usize)
        .flat_map(|i| {
            let t = i as f64 / 1000.0;
            let p = sprite(frames.start as f64 + 60.0 * t - 0.5);
            [t as f32, (p[0] + off[0] + 4.0 * (7.0 * t).sin()) as f32, (p[1] + off[1] + 3.0 * (5.0 * t).cos()) as f32]
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
        made = Some(tx.spawn((Name::new(name.to_string()), Operator { kind: "sketch".into() }, Inputs(vec![("stroke".into(), s)]), Output(out), SketchParams { lag: 0.0, ..SketchParams::default() })));
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
    // `=paint`: a paint tracker, painted on frame 600 and again (a reset paint) on 625, selected.
    let mut painted = None;
    if std::env::var("TT_SCENE_DEMO").is_ok_and(|m| m == "paint") && tt_track::job::cotracker_availability().is_ok() {
        world.resource_mut::<NewTrackers>().method = Method::Paint;
        let paint = |f: FrameIndex| {
            let c = at(f, [0.0, 0.0]);
            let (centre, half, mask) = tt_track::tool::paint_look(&[[c[0] - 10.0, c[1] - 4.0], [c[0] + 4.0, c[1] + 6.0], [c[0] + 12.0, c[1] - 2.0]], 9.0);
            let mut l = Look::new(f, centre, half);
            l.mask = mask;
            l
        };
        painted = tt_track::add_unguided_tracker(world, paint(600));
        if let Some(t) = painted {
            tt_track::add_look(world, t, paint(625));
            tt_core::span::set_span(world, t, tt_core::span::Span::new(570, 680));
        }
        world.resource_mut::<NewTrackers>().method = Method::Template;
    }
    // `=cursor`: a cursor tracker on the sprite, painted loosely: pattern 1 on
    // three frames, pattern 2 on one; selected, the Track tool on.
    let mut cursor = None;
    if std::env::var("TT_SCENE_DEMO").is_ok_and(|m| m == "cursor") {
        world.resource_mut::<NewTrackers>().method = Method::Cursor;
        let paint = |f: FrameIndex, pattern: u32| {
            let c = at(f, [0.0, 0.0]);
            let (centre, half, mask) = tt_track::tool::paint_look(&[[c[0] - 12.0, c[1] - 8.0], [c[0] + 10.0, c[1] + 10.0]], 18.0);
            Look { mask, pattern, ..Look::new(f, centre, half) }
        };
        cursor = tt_track::add_unguided_tracker(world, paint(600, 0));
        if let Some(t) = cursor {
            for (f, p) in [(640, 0), (680, 0), (620, 1)] {
                tt_track::add_look(world, t, paint(f, p));
            }
            tt_core::span::set_span(world, t, tt_core::span::Span::new(560, 700));
        }
        world.resource_mut::<NewTrackers>().method = Method::Template;
    }
    // `=icons`: the tracker with no sketch has cursor icons, the first pack
    // found sized from its looks, the second sized by hand.
    if std::env::var("TT_SCENE_DEMO").is_ok_and(|m| m == "icons")
        && let Some(t) = loose
    {
        let packs = tt_track::icons::packs();
        let mut set = tt_track::icons::CursorIcons::default();
        for (i, p) in packs.iter().enumerate() {
            set.icons.extend(p.icons.iter().cloned());
            if i == 1 {
                set.sizes.push(tt_track::icons::PackSize { pack: p.name.to_string(), size: 0.8 });
            }
        }
        edit(world, "Scene: cursor icons", |tx| tx.insert(t, set));
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
    // `=off`: the guided tracker switched off on 610–639.
    if std::env::var("TT_SCENE_DEMO").is_ok_and(|m| m == "off")
        && let Some(g) = guided
    {
        tt_track::off::switch_from(world, &[g], 610, true);
        tt_track::off::switch_from(world, &[g], 640, false);
        world.resource_mut::<tt_core::selection::Selection>().select_only(g);
        return subject;
    }
    if let Some(t) = cursor.or(painted).or(loose) {
        world.resource_mut::<tt_core::selection::Selection>().select_only(t);
    }
    if cursor.is_some() {
        world.resource_mut::<tt_core::tool::ActiveTool>().0 = tt_core::tool::Tool::Track;
    }
    if painted.is_some() {
        world.resource_mut::<tt_core::tool::ActiveTool>().0 = tt_core::tool::Tool::Track;
        // (Before the reset paint, with the points' paths: who makes it and who doesn't.)
        world.resource_mut::<tt_core::transport::Transport>().seek(610);
        world.resource_mut::<crate::panels::viewport::PointerView>().paint_paths = true;
    }
    subject
}
