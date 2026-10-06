//! Trackers in the panels: their points, paths, looks and reset points over
//! the video, the Track and Draw tools' previews and hints, their progress
//! in the top bar, and the inspector (which way to track, what it is doing,
//! its looks or reset points, what was drawn on it).
//!
//! In the visual language (`style`): automatic results cyan, frames drawn by
//! hand orange (a solid dot; where it replaces an automatic result, that one
//! shows faint, joined to it by a dashed line), looks and reset points white
//! (a template tracker's patterns as rectangles, a CoTracker's points as
//! diamonds), lost frames red, stale ones dim. A spinner says what a
//! tracker is doing (amber while CoTracker loads its model).

use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::name::Name;
use bevy_ecs::prelude::*;
use egui::{Align2, Color32, FontId, Painter, Pos2, Rect, Shape, Stroke, StrokeKind, Vec2};
use tt_core::input::{Action, PendingActions};
use tt_core::op::{OpError, Operator, Output};
use tt_core::selection::Selection;
use tt_core::signal::{FrameState, SignalId, SignalStore};
use tt_core::time::FrameIndex;
use tt_core::tool::{ActiveTool, Tool};
use tt_core::transport::Transport;
use tt_core::view::SpaceMap;
use tt_track::human::{DrawTool, auto_signal, drawn_at, drawn_frames, erase_drawn, human_signal};
use tt_track::look::{Look, looks_of};
use tt_track::runner::{SideStatus, TrackStatus};
use tt_track::tool::TrackTool;
use tt_track::{LOST as LOST_FLAG, Method, NewTrackers, TrackRun, Tracker, guide_of, is_tracker, run_of, set_run, trackers_of};

use super::viewport::ViewportMapping;
use crate::icons::{self, Activity, Glyph};
use crate::style;

pub const TRACK: Color32 = style::AUTO;
pub const LOST: Color32 = style::LOST;
/// Frames of path drawn either side of the playhead: selected trackers, others.
const PATH_FRAMES: FrameIndex = 90;
const SHORT_PATH: FrameIndex = 12;

/// What a tracker can be asked to do: (run, button, menu item, tip). It
/// tracks from where you showed it the subject (its first look).
pub const RUNS: [(TrackRun, &str, &str, &str); 4] = [
    (TrackRun::Backward, "\u{25c0} Back", "Track backward", "Track backward from its first look, to the start of its sketch"),
    (TrackRun::Both, "\u{25c0} Both \u{25b6}", "Track both ways", "Track both ways from its first look"),
    (TrackRun::Forward, "Forward \u{25b6}", "Track forward", "Track forward from its first look, to the end of its sketch"),
    (TrackRun::Paused, "Pause", "Pause tracking", "Stop tracking: what it tracked stays, and a direction goes on from there"),
];

/// Ask `trackers` to track one way, both, or pause (the Inspector's buttons, the menu).
pub fn ask(world: &mut World, trackers: &[Entity], run: TrackRun) {
    for t in trackers {
        set_run(world, *t, run);
    }
}

/// What the Track tool makes: "template tracker" or "CoTracker".
pub fn kind_name(method: Method) -> &'static str {
    match method {
        Method::Template => "template tracker",
        Method::CoTracker => "CoTracker",
        Method::Manual => "manual dot",
    }
}

/// Live trackers with their output signals.
pub fn list(world: &mut World) -> Vec<(Entity, SignalId)> {
    let mut q = world.query_filtered::<(Entity, &Operator, &Output), Without<Disabled>>();
    q.iter(world).filter(|(_, o, _)| o.kind == "track").map(|(e, _, o)| (e, o.0)).collect()
}

/// How a frame of a tracker's path counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mark {
    Auto,
    Lost,
    Drawn,
}

impl Mark {
    fn color(self) -> Color32 {
        match self {
            Mark::Auto => style::AUTO,
            Mark::Lost => style::LOST,
            Mark::Drawn => style::HAND,
        }
    }
}

fn dashed(painter: &Painter, a: Pos2, b: Pos2, stroke: Stroke) {
    painter.add(Shape::dashed_line(&[a, b], stroke, 3.0, 3.0));
}

/// Every tracker's point at the playhead and its path (selected trackers
/// ±90 frames, others a short tail), in the visual language (module docs).
/// A selected tracker's looks or reset points show on the path, and in full
/// on their own frame.
pub fn draw(painter: &Painter, map: &ViewportMapping, world: &World, list: &[(Entity, SignalId)], frame: FrameIndex, space: &dyn Fn(FrameIndex) -> SpaceMap) {
    let selected = &world.resource::<Selection>().entities;
    let store = world.resource::<SignalStore>();
    let now = world.resource::<tt_core::time::WallClock>().now;
    let mut moving = false;
    for &(e, id) in list {
        let Some(sig) = store.get(id) else { continue };
        let looks = looks_of(world, e);
        let method = world.get::<Tracker>(e).map_or(Method::Template, |t| t.method);
        let lit = selected.contains(&e) || looks.iter().any(|l| selected.contains(l));
        let human = human_signal(world, e);
        let auto = if method == Method::Manual { None } else { auto_signal(world, e) };
        let fade = if lit { 1.0 } else { 0.55 };
        let reach = if lit { PATH_FRAMES } else { SHORT_PATH };
        // Only within its lifetime (its span on the timeline).
        let span = tt_core::span::span_of(world, e);
        let screen = |f: FrameIndex, x: f32, y: f32| map.to_screen(space(f).from_source([x as f64, y as f64]));
        let at = |f: FrameIndex| {
            sig.get(f).filter(|_| span.contains(f)).map(|v| {
                let mark = if human.is_some_and(|h| h.get(f).is_some()) {
                    Mark::Drawn
                } else if tt_track::flags(v) != 0 {
                    Mark::Lost
                } else {
                    Mark::Auto
                };
                (screen(f, v[0], v[1]), mark)
            })
        };
        // The path, split where it breaks, changes kind, or passes the playhead (ahead is fainter).
        let line = |run: &[Pos2], (mark, ahead): (Mark, bool)| {
            if run.len() > 1 {
                let width = if mark == Mark::Drawn { 1.6 } else { 1.0 };
                painter.add(Shape::line(run.to_vec(), Stroke::new(width, mark.color().gamma_multiply(fade * if ahead { 0.4 } else { 0.9 }))));
            }
        };
        let (mut run, mut kind): (Vec<Pos2>, Option<(Mark, bool)>) = (Vec::new(), None);
        for f in frame - reach..=frame + reach {
            match at(f) {
                Some((p, mark)) => {
                    let k = (mark, f > frame);
                    if kind != Some(k) {
                        let last = run.last().copied();
                        if let Some(old) = kind {
                            line(&run, old);
                        }
                        run.clear();
                        run.extend(last);
                        kind = Some(k);
                    }
                    run.push(p);
                }
                None => {
                    if let Some(old) = kind.take() {
                        line(&run, old);
                    }
                    run.clear();
                }
            }
        }
        if let Some(k) = kind {
            line(&run, k);
        }

        // Its looks or reset points: small marks along the path, in full on their own frame.
        if lit {
            for (l, look) in looks.iter().filter_map(|l| Some((*l, world.get::<Look>(*l)?))).filter(|(_, l)| (l.frame - frame).abs() <= reach) {
                let chosen = selected.contains(&l);
                let p = screen(look.frame, look.x, look.y);
                let pin = style::PIN.gamma_multiply(if look.frame == frame { 0.95 } else { 0.55 });
                let name = world.get::<Name>(l).map_or_else(|| method.look_word().to_string(), |n| n.to_string());
                match (method, look.frame == frame) {
                    (Method::CoTracker, true) => {
                        icons::diamond(painter, p, if chosen { 9.0 } else { 7.5 }, Stroke::new(if chosen { 2.0 } else { 1.5 }, pin), None);
                        painter.text(p + Vec2::new(10.0, 6.0), Align2::LEFT_TOP, name, FontId::proportional(10.0), pin);
                    }
                    (Method::CoTracker, false) => icons::diamond(painter, p, 3.5, Stroke::new(1.0, pin), Some(pin.gamma_multiply(0.5))),
                    (_, true) => {
                        let b = space(frame).box_from_source(look.rect());
                        let r = Rect::from_min_max(map.to_screen([b[2], b[3]]), map.to_screen([b[4], b[5]]));
                        painter.rect_stroke(r, 0.0, Stroke::new(if chosen { 2.0 } else { 1.0 }, pin), StrokeKind::Outside);
                        painter.text(r.left_bottom() + Vec2::new(0.0, 2.0), Align2::LEFT_TOP, name, FontId::proportional(10.0), pin);
                    }
                    (_, false) => {
                        painter.rect_stroke(Rect::from_center_size(p, Vec2::splat(5.0)), 0.0, Stroke::new(1.0, pin), StrokeKind::Middle);
                    }
                }
            }
        }

        let activity = if method == Method::Manual { None } else { Some(Activity::of(world, e)) };
        moving |= activity.is_some_and(Activity::moving);
        let name = world.get::<Name>(e).map_or("Tracker".to_string(), |n| n.to_string());
        let Some(v) = sig.get(frame).filter(|_| span.contains(frame)) else {
            // Nothing here yet: while it starts or tracks, its spinner sits on its first look.
            if let Some(a) = activity.filter(|a| a.moving())
                && let Some(look) = looks.first().and_then(|l| world.get::<Look>(*l))
            {
                let p = screen(frame, look.x, look.y);
                icons::activity(painter, p + Vec2::new(12.0, -12.0), 5.0, a, now);
                if lit {
                    painter.text(p + Vec2::new(21.0, -18.0), Align2::LEFT_TOP, format!("{name} \u{b7} {}", a.words()), FontId::proportional(11.0), a.color());
                }
            }
            continue;
        };
        let stale = sig.state(frame) == FrameState::Stale;
        let (label_at, text, color) = match human.and_then(|h| h.get(frame)) {
            Some(h) => {
                let p = screen(frame, h[0], h[1]);
                // What the algorithm found here, faint, joined to the drawn point that replaces it.
                if let Some(a) = auto.and_then(|a| a.get(frame)) {
                    let q = screen(frame, a[0], a[1]);
                    if (q - p).length() > 1.5 {
                        let c = if tt_track::flags(a) != 0 { style::LOST } else { style::AUTO };
                        dashed(painter, q, p, Stroke::new(1.0, c.gamma_multiply(0.55 * fade)));
                        icons::crosshair(painter, q, 3.5, Stroke::new(1.0, c.gamma_multiply(0.6 * fade)));
                    }
                }
                let c = style::HAND.gamma_multiply(fade);
                painter.circle_filled(p, if lit { 4.5 } else { 3.5 }, c);
                painter.circle_stroke(p, if lit { 4.5 } else { 3.5 }, Stroke::new(1.0, Color32::from_black_alpha(170)));
                if lit {
                    painter.circle_stroke(p, 8.5, Stroke::new(1.0, c.gamma_multiply(0.7)));
                }
                (p + Vec2::new(11.0, -14.0), format!("{name} \u{b7} drawn"), c)
            }
            None => {
                let b = space(frame).box_from_source(std::array::from_fn(|c| v[c] as f64));
                let flags = tt_track::flags(v);
                let c = if flags != 0 { style::LOST } else { style::AUTO }.gamma_multiply(fade * if stale { 0.5 } else { 1.0 });
                let r = Rect::from_min_max(map.to_screen([b[2], b[3]]), map.to_screen([b[4], b[5]]));
                let stroke = Stroke::new(if lit { 1.5 } else { 1.0 }, c);
                painter.rect_stroke(r, 2.0, stroke, StrokeKind::Middle);
                icons::crosshair(painter, map.to_screen([b[0], b[1]]), 3.0, stroke);
                let text = match flags {
                    0 => format!("{name} \u{b7} {:.2}", v[6]),
                    f if f & LOST_FLAG != 0 => format!("{name} \u{b7} lost ({:.2})", v[6]),
                    _ => format!("{name} \u{b7} outside the sketch ({:.2})", v[6]),
                };
                (r.right_top() + Vec2::new(4.0, 0.0), text, c)
            }
        };
        if lit {
            let galley = painter.layout_no_wrap(text, FontId::proportional(11.0), color);
            let w = galley.size().x;
            painter.galley(label_at, galley, color);
            if let Some(a) = activity.filter(|a| *a != Activity::Done) {
                icons::activity(painter, label_at + Vec2::new(w + 9.0, 6.5), 4.5, a, now);
            }
        } else if let Some(a) = activity.filter(|a| a.moving() || *a == Activity::Failed) {
            icons::activity(painter, label_at + Vec2::new(4.0, 6.0), 4.0, a, now);
        }
    }
    // (30 a second is enough for a spinner: the card has tracking to do.)
    if moving {
        painter.ctx().request_repaint_after(std::time::Duration::from_millis(33));
    }
}

fn side_line(s: &SideStatus, forward: bool) -> String {
    let arrow = if forward { "forward" } else { "backward" };
    let state = if s.waiting {
        " \u{b7} waiting for the playhead".to_string()
    } else if s.phase == tt_track::job::Phase::Loading {
        " \u{b7} loading CoTracker's model".to_string()
    } else if s.fps > 0.0 {
        format!(" \u{b7} {:.0} fps", s.fps)
    } else {
        String::new()
    };
    format!("{arrow}: at frame {}, to {}{state}", s.at, s.to)
}

/// The top bar's summary while trackers run: (label, details).
pub fn summary(world: &mut World) -> Option<(String, String)> {
    let list = list(world);
    let busy: Vec<(Entity, TrackStatus)> = list.iter().filter_map(|(e, _)| world.get::<TrackStatus>(*e).filter(|s| s.busy()).map(|s| (*e, s.clone()))).collect();
    if busy.is_empty() {
        return None;
    }
    let fps: f64 = busy.iter().flat_map(|(_, s)| [s.forward, s.backward]).flatten().map(|s| s.fps).sum();
    let waiting = busy.iter().flat_map(|(_, s)| [s.forward, s.backward]).flatten().all(|s| s.waiting);
    let loading = busy.iter().filter(|(_, s)| s.starting() == Some(true)).count();
    let text = if loading > 0 {
        "loading CoTracker\u{2026}".to_string()
    } else if waiting {
        format!("{} waiting for the playhead", busy.len())
    } else {
        format!("tracking {} \u{b7} {:.0} fps", busy.len(), fps)
    };
    let tip: Vec<String> = busy
        .iter()
        .map(|(e, s)| {
            let name = world.get::<Name>(*e).map_or("Tracker".to_string(), |n| n.to_string());
            let sides: Vec<String> = [s.forward.map(|f| side_line(&f, true)), s.backward.map(|b| side_line(&b, false))].into_iter().flatten().collect();
            format!("{name} ({}): {}", s.rendition, sides.join(", "))
        })
        .collect();
    Some((text, tip.join("\n")))
}

/// A hint row under the frame counter and the breadcrumb, and a second for a refusal.
fn hint(painter: &Painter, map: &ViewportMapping, text: String, color: Color32, refused: Option<&String>) {
    let galley = painter.layout_no_wrap(text, FontId::proportional(13.0), color);
    let r = Align2::LEFT_TOP.anchor_size(map.panel.left_top() + Vec2::new(15.0, 65.0), galley.size()).expand(5.0);
    painter.rect_filled(r, 4.0, Color32::from_black_alpha(190));
    painter.galley(r.min + Vec2::splat(5.0), galley, color);
    if let Some(why) = refused {
        let g = painter.layout_no_wrap(why.clone(), FontId::proportional(13.0), LOST);
        let r2 = Align2::LEFT_TOP.anchor_size(r.left_bottom() + Vec2::new(5.0, 8.0), g.size()).expand(5.0);
        painter.rect_filled(r2, 4.0, Color32::from_black_alpha(190));
        painter.galley(r2.min + Vec2::splat(5.0), g, LOST);
    }
}

/// The Track and Draw tools on the video: what a press would make (dashed
/// and light), what is being made (solid), and a row of hints.
pub fn draw_tool(ui: &egui::Ui, painter: &Painter, response: &egui::Response, world: &mut World, map: &ViewportMapping) {
    match world.resource::<ActiveTool>().0 {
        Tool::Track => track_tool(ui, painter, response, world, map),
        Tool::Draw => draw_by_hand(ui, painter, response, world, map),
        _ => {}
    }
}

fn track_tool(ui: &egui::Ui, painter: &Painter, response: &egui::Response, world: &mut World, map: &ViewportMapping) {
    let tool = world.resource::<TrackTool>().clone();
    let shift = ui.input(|i| i.modifiers.shift);
    let point = tt_track::tool::method_for(world, shift) == Method::CoTracker;
    let dashed_rect = |r: Rect, c: Color32| {
        let s = Stroke::new(1.0, c);
        for (a, b) in [(r.left_top(), r.right_top()), (r.right_top(), r.right_bottom()), (r.right_bottom(), r.left_bottom()), (r.left_bottom(), r.left_top())] {
            painter.add(Shape::dashed_line(&[a, b], s, 4.0, 3.0));
        }
    };
    if let Some(pos) = response.hover_pos() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
        match (point, tool.drag) {
            // CoTracker: the pixel it follows, where the press is let go.
            (true, Some(_)) => {
                icons::diamond(painter, pos, 8.0, Stroke::new(2.0, style::PIN), None);
                icons::crosshair(painter, pos, 4.0, Stroke::new(1.0, style::PIN));
            }
            (true, None) => {
                icons::diamond(painter, pos, 8.0, Stroke::new(1.0, style::PIN.gamma_multiply(0.6)), None);
            }
            (false, Some((start, _, _))) => dashed_rect(Rect::from_two_pos(map.to_screen(start), pos), TRACK),
            (false, None) => dashed_rect(Rect::from_center_size(pos, Vec2::splat(2.0 * tool.brush)), TRACK.gamma_multiply(0.7)),
        }
    }
    let world_ref: &World = world;
    let selected = world_ref.resource::<Selection>().primary().filter(|e| is_tracker(world_ref, *e));
    let name = selected.and_then(|e| world_ref.get::<Name>(e)).map_or("the tracker".to_string(), |n| n.to_string());
    let method = selected.and_then(|e| world_ref.get::<Tracker>(e)).map(|t| t.method);
    let kind = kind_name(world_ref.resource::<NewTrackers>().method).to_uppercase();
    let text = match (selected, tool.reseed, method) {
        (Some(t), Some(r), Some(Method::CoTracker)) if t == r => format!("TRACK \u{b7} click the pixel to follow on this frame: {name} starts again from it \u{b7} T/Esc exits"),
        (Some(t), Some(r), _) if t == r => format!("TRACK \u{b7} drag around the subject on this frame: {name} starts again from it \u{b7} T/Esc exits"),
        (Some(_), _, Some(Method::Manual)) => format!("TRACK \u{b7} {name} is a manual dot: draw it with the Draw tool (M) \u{b7} Shift+drag: a new tracker \u{b7} T/Esc exits"),
        (Some(_), _, Some(Method::CoTracker)) => {
            format!("TRACK \u{b7} click where {name}'s pixel really is: its reset point on this frame (it follows that pixel from here) \u{b7} Shift+click: a new tracker \u{b7} T/Esc exits")
        }
        (Some(t), _, _) if run_of(world_ref, t) == TrackRun::Paused => {
            format!("TRACK \u{b7} {name} waits: Back, Both or Forward in the Inspector tracks it \u{b7} drag where it missed: a new look \u{b7} Shift+drag: a new tracker \u{b7} T/Esc exits")
        }
        (Some(_), _, _) => format!("TRACK \u{b7} drag around the subject where {name} missed it: a new look, pinned here \u{b7} Shift+drag: a new tracker \u{b7} T/Esc exits"),
        _ if point => format!("NEW {kind} \u{b7} click the pixel to follow (inside a sketch it searches there; elsewhere the whole frame) \u{b7} T/Esc exits"),
        _ => format!("NEW {kind} \u{b7} drag around what to follow (its pattern) \u{b7} click: a point, the dashed box's size (Ctrl+wheel) \u{b7} T/Esc exits"),
    };
    hint(painter, map, text, TRACK, tool.refused.as_ref());
}

/// The Draw tool: a light dashed ring where a press would draw, a solid dot while drawing.
fn draw_by_hand(ui: &egui::Ui, painter: &Painter, response: &egui::Response, world: &mut World, map: &ViewportMapping) {
    let tool = world.resource::<DrawTool>().clone();
    let alt = ui.input(|i| i.modifiers.alt);
    let erase = tool.stroke.map_or(alt, |s| s.erase);
    if let Some(pos) = response.hover_pos() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
        match (tool.stroke, erase) {
            (Some(_), false) => {
                painter.circle_filled(pos, 4.5, style::HAND);
                painter.circle_stroke(pos, 8.5, Stroke::new(2.0, style::HAND));
            }
            (Some(_), true) => {
                painter.circle_stroke(pos, 9.0, Stroke::new(2.0, style::LOST));
                painter.line_segment([pos - Vec2::splat(5.0), pos + Vec2::splat(5.0)], Stroke::new(2.0, style::LOST));
            }
            (None, _) => {
                let c = if erase { style::LOST } else { style::HAND }.gamma_multiply(0.7);
                let ring: Vec<Pos2> = (0..=24).map(|i| pos + 8.5 * Vec2::angled(i as f32 / 24.0 * std::f32::consts::TAU)).collect();
                painter.add(Shape::dashed_line(&ring, Stroke::new(1.0, c), 2.5, 2.5));
                painter.circle_filled(pos, 1.5, c);
            }
        }
    }
    let world_ref: &World = world;
    let selected = world_ref.resource::<Selection>().primary().filter(|e| is_tracker(world_ref, *e));
    let name = selected.and_then(|e| world_ref.get::<Name>(e)).map_or("the tracker".to_string(), |n| n.to_string());
    let t = world_ref.resource::<Transport>();
    let playing = if t.playing { "playing: every frame it passes" } else { "paused: this frame (Space plays)" };
    let text = match (tool.stroke, selected) {
        (Some(s), _) if s.erase => format!("ERASING \u{b7} {playing} \u{b7} let go to finish \u{b7} Esc cancels"),
        (Some(_), _) => format!("DRAWING \u{b7} {playing} \u{b7} let go to finish \u{b7} Esc cancels"),
        (None, Some(_)) => format!("DRAW \u{b7} hold on the video: {name}'s point is where you hold, over what it tracked \u{b7} Alt+hold: erase \u{b7} Shift+hold: a new manual dot \u{b7} M/Esc exits"),
        (None, None) => "DRAW \u{b7} hold on the video: a new manual dot, its point where you hold on each frame shown \u{b7} select a tracker to draw over it \u{b7} M/Esc exits".to_string(),
    };
    hint(painter, map, text, style::HAND, tool.refused.as_ref());
}

/// Frames a tracker covers, and how many of them are flagged: counted again
/// only when its output changes (not on every repaint).
fn counts(ui: &egui::Ui, world: &World, e: Entity) -> (usize, usize) {
    let Some(sig) = world.get::<Output>(e).and_then(|o| world.resource::<SignalStore>().get(o.0)) else { return (0, 0) };
    let key = (sig.version(), 0u32);
    let id = egui::Id::new(("tracker counts", e));
    if let Some((k, c)) = ui.data(|d| d.get_temp::<((u64, u32), (usize, usize))>(id))
        && k == key
    {
        return c;
    }
    let n = world.resource::<Transport>().frame_count;
    let c = (0..n).filter_map(|f| sig.get(f)).fold((0, 0), |(c, l), v| (c + 1, l + (tt_track::flags(v) != 0) as usize));
    ui.data_mut(|d| d.insert_temp(id, (key, c)));
    c
}

/// A small key to the colours, in the visual language's own marks.
fn legend(ui: &mut egui::Ui, method: Method) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let mark = |ui: &mut egui::Ui, paint: &dyn Fn(&Painter, Pos2), text: &str| {
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(12.0), egui::Sense::hover());
            paint(ui.painter(), rect.center());
            ui.label(egui::RichText::new(text).small().color(style::MUTED));
            ui.add_space(6.0);
        };
        mark(
            ui,
            &|p, o| {
                p.circle_filled(o, 3.5, style::HAND);
            },
            "drawn by hand",
        );
        if method != Method::Manual {
            mark(ui, &|p, o| icons::crosshair(p, o, 4.0, Stroke::new(1.5, style::AUTO)), "automatic");
            mark(ui, &|p, o| icons::crosshair(p, o, 4.0, Stroke::new(1.5, style::LOST)), "lost");
            if method == Method::CoTracker {
                mark(ui, &|p, o| icons::diamond(p, o, 4.5, Stroke::new(1.5, style::PIN), None), "reset point");
            } else {
                mark(ui, &|p, o| {
                    p.rect_stroke(Rect::from_center_size(o, Vec2::splat(8.0)), 0.0, Stroke::new(1.5, style::PIN), StrokeKind::Middle);
                }, "look");
            }
        }
    });
}

/// Inspector: a tracker's progress, errors, looks or reset points, and what
/// was drawn on it; a look's mask; a sketch's Track buttons.
pub fn inspector(ui: &mut egui::Ui, world: &mut World, e: Entity) {
    if world.get::<Look>(e).is_some() {
        let owner = tt_track::look::owner_of(world, e);
        if owner.and_then(|t| world.get::<Tracker>(t)).is_some_and(|t| t.method == Method::CoTracker) {
            let frame = world.get::<Look>(e).map_or(0, |l| l.frame);
            ui.label(
                egui::RichText::new(format!(
                    "A reset point: the pixel this CoTracker follows from frame {frame} on (one pixel at a time). \
                     To move it, go to frame {frame}, choose the Track tool and click where the pixel really is."
                ))
                .color(style::MUTED),
            );
        } else {
            ui.label(egui::RichText::new("A look: what the tracker's subject looks like here. Paint which pixels are the subject.").color(style::MUTED));
            super::look_editor::ui(ui, world, e);
        }
        ui.separator();
        return;
    }
    if is_tracker(world, e) {
        tracker_section(ui, world, e);
        ui.separator();
        return;
    }
    if !tt_core::sketch::is_sketch(world, e) {
        return;
    }
    let existing = trackers_of(world, e);
    let cotracker = tt_track::job::cotracker_availability();
    ui.horizontal_wrapped(|ui| {
        for (method, text, tip) in [
            (Method::Template, "Template tracker", "Drag a rectangle around what to follow in this sketch (or click a point): a tracker matching that pattern on every frame (fast, sub-pixel). It searches inside this sketch's box, in its view."),
            (Method::CoTracker, "CoTracker", "Click the pixel to follow in this sketch: Meta's CoTracker3, a learned point tracker, run in Python. It searches inside this sketch's box, in its view."),
        ] {
            let usable = method == Method::Template || cotracker.is_ok();
            let r = ui.button(text).on_hover_text(if usable { tip } else { "CoTracker is not set up on this computer. Click CoTracker. The doctor shows what CoTracker needs and sets it up." });
            if r.clicked() && !usable {
                world.resource_mut::<crate::setup::Doctor>().show();
            } else if r.clicked() {
                world.resource_mut::<NewTrackers>().method = method;
                world.resource_mut::<ActiveTool>().0 = Tool::Track;
            }
        }
        if ui.button("Track its centre").on_hover_text("A quick tracker on this sketch's own point at the playhead (a square of its box; the kind last chosen); re-centred on the sketch when done").clicked() {
            world.resource_mut::<PendingActions>().push(Action::Track);
        }
        if !existing.is_empty() {
            ui.label(egui::RichText::new(format!("{} tracker(s)", existing.len())).color(style::MUTED));
        }
    });
}

fn tracker_section(ui: &mut egui::Ui, world: &mut World, e: Entity) {
    if let Some(err) = world.get::<OpError>(e) {
        ui.colored_label(LOST, err.0.clone());
    }
    let method = world.get::<Tracker>(e).map_or(Method::Template, |t| t.method);
    let here = world.resource::<Transport>().frame();
    let name = world.get::<Name>(e).map_or("the tracker".to_string(), |n| n.to_string());
    let about = match method {
        Method::Manual => "A manual dot: only what you draw. Draw tool (M): hold on the video, and its point is where you hold on every frame shown.".to_string(),
        _ => {
            let guide = guide_of(world, e).and_then(|g| world.get::<Name>(g)).map(|n| n.to_string());
            let r#where = guide.map_or("searching the whole frame".to_string(), |g| format!("searching inside {g}"));
            let fix = if method == Method::CoTracker {
                "where it loses the pixel: Track tool, click where it really is (a reset point)"
            } else {
                "where it misses: Track tool, drag around the subject (a new look, pinned there)"
            };
            format!("A {} {} \u{b7} {fix} \u{b7} or draw over its results (Draw tool, M): what you draw is its output there", kind_name(method), r#where)
        }
    };
    ui.horizontal_top(|ui| {
        icons::icon(ui, Glyph::tracker(method), true);
        ui.add(egui::Label::new(egui::RichText::new(about).color(style::MUTED)).wrap());
    });
    legend(ui, method);
    let status = world.get::<TrackStatus>(e).cloned().unwrap_or_default();
    if method != Method::Manual {
        // What it is doing, with its spinner.
        let activity = Activity::of(world, e);
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(14.0), egui::Sense::hover());
            icons::activity(ui.painter(), rect.center(), 5.0, activity, ui.input(|i| i.time));
            ui.label(egui::RichText::new(activity.words()).color(activity.color()));
            if activity.moving() {
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
            }
        });
        // Which way to track (for every selected tracker): nothing runs until asked.
        let trackers: Vec<Entity> = world.resource::<Selection>().entities.iter().copied().filter(|t| is_tracker(world, *t)).collect();
        let run = run_of(world, e);
        let mut asked = None;
        ui.horizontal(|ui| {
            for (r, button, _, tip) in RUNS {
                if ui.selectable_label(run == r, button).on_hover_text(tip).clicked() {
                    asked = Some(r);
                }
            }
        });
        if let Some(r) = asked {
            ask(world, &trackers, r);
        }
        if run == TrackRun::Paused && !status.busy() {
            ui.label(egui::RichText::new("Paused: Back, Both or Forward tracks it (what it has tracked stays)").color(style::LIVE));
            // CoTracker doesn't start by itself when a project opens: say what it was asked.
            if let Some(was) = world.get::<tt_track::PausedOnOpen>(e).map(|p| p.0).filter(|r| *r != TrackRun::Paused) {
                let word = RUNS.iter().find(|r| r.0 == was).map_or("Both", |r| r.2);
                ui.label(egui::RichText::new(format!("Paused when the project opened. Before, it was set to: {word}.")).color(style::MUTED));
            }
        }
        let (covered, lost) = counts(ui, world, e);
        ui.add(
            egui::Label::new(format!("{covered} frames \u{b7} {lost} flagged (lost, or outside the sketch){}", if status.rendition.is_empty() { String::new() } else { format!(" \u{b7} reads the {}", status.rendition) }))
                .wrap(),
        );
    }
    // What was drawn on it.
    let (drawn, hull) = drawn_frames(world, e);
    let mut erase: Option<Option<std::ops::Range<FrameIndex>>> = None;
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(12.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 3.5, style::HAND);
        let text = match hull {
            Some((a, b)) => egui::RichText::new(format!("{drawn} frame{} drawn by hand (frames {a}\u{2013}{b})", if drawn == 1 { "" } else { "s" })).color(style::HAND),
            None => egui::RichText::new("Nothing drawn by hand yet (Draw tool, M)").color(style::MUTED),
        };
        ui.add(egui::Label::new(text).wrap());
    });
    if hull.is_some() {
        ui.horizontal(|ui| {
            if drawn_at(world, e, here).is_some() && ui.small_button(format!("Erase frame {here}")).on_hover_text("Its own result shows here again").clicked() {
                erase = Some(Some(here..here + 1));
            }
            if ui.small_button("Erase all").on_hover_text(if method == Method::Manual { "A manual dot is only what was drawn: this empties it" } else { "Its automatic results show everywhere again" }).clicked() {
                erase = Some(None);
            }
        });
    }
    if let Some(r) = erase {
        erase_drawn(world, e, r);
    }
    if method == Method::Manual {
        return;
    }
    // Its looks (select one to paint its mask), or a CoTracker's reset points.
    let looks = looks_of(world, e);
    let mut remove = None;
    ui.horizontal_wrapped(|ui| {
        ui.label(if method == Method::CoTracker { "Reset points:" } else { "Looks:" });
        for l in &looks {
            let Some(look) = world.get::<Look>(*l) else { continue };
            let f = look.frame;
            let label = if method == Method::CoTracker {
                format!("frame {} \u{b7} {:.0}, {:.0}", look.frame, look.x, look.y)
            } else {
                let painted = if look.painted().is_some() { " \u{b7} masked" } else { " \u{b7} unpainted" };
                format!("frame {} \u{b7} {:.0}\u{d7}{:.0}{painted}", look.frame, 2.0 * look.half_w, 2.0 * look.half_h)
            };
            let tip = if method == Method::CoTracker { "Go to its frame (to move it: Track tool, click there)" } else { "Select it to see and paint which pixels are the subject; the playhead goes to its frame" };
            if ui.button(label).on_hover_text(tip).clicked() {
                world.resource_mut::<Selection>().select_only(*l);
                world.resource_mut::<PendingActions>().push(Action::Seek(f));
            }
            if ui.small_button("x").on_hover_text(format!("Remove this {} (it re-tracks without it)", method.look_word())).clicked() {
                remove = Some(*l);
            }
        }
    });
    if let Some(l) = remove {
        tt_core::commands::delete(world, &[l]);
    }
    let reseed_tip = if method == Method::CoTracker {
        "Start it again from the playhead, from its reset point on this frame (with none here, click the pixel first)"
    } else {
        "Start it again from the playhead, from your look on this frame (with none here, drag around the subject first)"
    };
    if ui.button("Re-seed here").on_hover_text(reseed_tip).clicked() {
        world.resource_mut::<PendingActions>().push(Action::Track);
    }
    for (s, forward) in [(status.forward, true), (status.backward, false)] {
        let Some(s) = s else { continue };
        // (The anchor as tracked: moved into the guide's frames.)
        let total = (s.to - status.anchor).abs().max(1) as f32;
        let done = (s.at - status.anchor).abs() as f32 / total;
        ui.add(egui::ProgressBar::new(done.clamp(0.0, 1.0)).text(side_line(&s, forward)));
    }
    let _ = name;
}
