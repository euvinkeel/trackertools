//! Viewport overlays (DESIGN §8.1 live feedback): every sketch's region and
//! point at the playhead (the selected one bright, with its path; the rest
//! faint), the stroke in progress laid over the sketch it edits, and the
//! Sketch tool's cursor and hints. (A click on a region selects its sketch:
//! tt_core's tool.rs.)
//!
//! In the visual language (`style`), sketches are drawn by hand: orange. The
//! Sketch tool shows, before a press, the box a hold would get, dashed and
//! light, sized by the hand's jiggle right now (as a held frame is); once
//! pressed, the box being recorded is solid and thick (amber: live).

use bevy_ecs::name::Name;
use bevy_ecs::prelude::*;
use egui::{Align2, Color32, CursorIcon, FontId, Painter, Pos2, Rect, Shape, Stroke, StrokeKind, Vec2};
use tt_core::capture::LiveCapture;
use tt_core::op::{Operator, Output};
use tt_core::selection::Selection;
use tt_core::signal::{FrameState, SignalStore};
use tt_core::sketch::sketch_of;
use tt_core::time::FrameIndex;
use tt_core::tool::{ActiveTool, Tool};
use tt_core::transport::Transport;
use tt_core::view::{SpaceMap, map_at};

use super::viewport::ViewportMapping;
use crate::style;

pub const LIVE: Color32 = style::LIVE;
/// Subjects (tt_core::subject).
pub const SUBJECT: Color32 = style::SUBJECT;
/// Frames of path drawn either side of the playhead for the selected sketch.
const PATH_FRAMES: FrameIndex = 90;

/// Sketches live in source pixels; `view` is the space shown (None = the
/// source): every frame's value goes through that view as it framed that frame.
#[allow(clippy::too_many_arguments)]
pub fn draw(ui: &egui::Ui, painter: &Painter, response: &egui::Response, world: &mut World, map: &ViewportMapping, frame: FrameIndex, view: Option<Entity>, shown: SpaceMap) {
    // Every selected sketch is lit (a selected stroke lights its sketch); the primary one is edited.
    let picked: Vec<Entity> = world.resource::<Selection>().entities.clone();
    let lit: Vec<Entity> = picked.iter().filter_map(|e| sketch_of(world, *e)).collect();
    let selected = picked.last().and_then(|e| sketch_of(world, *e));
    let mut q = world.query::<(Entity, &Operator, &Output)>();
    let sketches: Vec<(Entity, tt_core::signal::SignalId)> = q.iter(world).filter(|(_, o, _)| o.kind == "sketch").map(|(e, _, o)| (e, o.0)).collect();
    let subject_list: Vec<(Entity, tt_core::signal::SignalId)> = q.iter(world).filter(|(_, o, _)| o.kind == "subject").map(|(e, _, o)| (e, o.0)).collect();
    let trackers = super::tracks::list(world);
    let world: &World = world;
    // The shown space's framing around the playhead, looked up once per frame drawn.
    // (The playhead's frame uses the framing as shown, which may be easing in.)
    let spaces: Vec<SpaceMap> = (frame - PATH_FRAMES..=frame + PATH_FRAMES).map(|f| if f == frame { shown } else { map_at(world, view, f) }).collect();
    let space = |f: FrameIndex| spaces.get((f - (frame - PATH_FRAMES)) as usize).copied().unwrap_or_else(|| map_at(world, view, f));
    let to_canvas = |f: FrameIndex, b: [f64; 6]| space(f).box_from_source(b);
    let store = world.resource::<SignalStore>();
    let live = world.resource::<LiveCapture>().0.as_ref();
    let editing = live.and_then(|l| l.target);
    let tool = world.resource::<ActiveTool>().0;

    for (e, signal) in &sketches {
        let Some(sig) = store.get(*signal) else { continue };
        // Only within its lifetime (its span on the timeline).
        let span = tt_core::span::span_of(world, *e);
        let own = |f: FrameIndex| sig.get(f).filter(|_| span.contains(f)).map(|v| std::array::from_fn::<f64, 6, _>(|c| v[c] as f64));
        let value = |f: FrameIndex| if editing == Some(*e) { live.and_then(|l| l.preview_at(f)).or_else(|| own(f)) } else { own(f) };
        let color = if editing == Some(*e) {
            LIVE
        } else if lit.contains(e) && live.is_none() {
            style::HAND
        } else {
            style::HAND.gamma_multiply(0.35)
        };
        if editing == Some(*e) || (lit.contains(e) && live.is_none()) {
            path(painter, map, frame, |f| value(f).map(|v| to_canvas(f, v)).map(|v| [v[0], v[1]]), color);
        }
        let Some(v) = value(frame) else { continue };
        let stale = editing != Some(*e) && sig.state(frame) == FrameState::Stale;
        region(painter, map, to_canvas(frame, v), color, stale, editing == Some(*e) && live.is_some_and(|l| l.preview_at(frame).is_some()));
    }

    super::tracks::draw(painter, map, world, &trackers, frame, &space);
    subjects(painter, map, world, &subject_list, &picked, frame, &space);

    if let Some(live) = live {
        if live.target.is_none() {
            path(painter, map, frame, |f| live.preview_at(f).map(|b| to_canvas(f, b)).map(|b| [b[0], b[1]]), LIVE);
            if let Some(b) = live.preview_at(frame) {
                region(painter, map, to_canvas(frame, b), LIVE, false, true);
            }
        }
        // Playing, the hand is `lag` behind the shown frame: its newest box, dimmed.
        if live.preview_at(frame).is_none()
            && let Some((g, b)) = (frame - 120..frame).rev().find_map(|g| live.visits(g).then(|| live.preview_at(g)).flatten().map(|b| (g, b)))
        {
            region(painter, map, to_canvas(g, b), LIVE, true, false);
        }
        // The raw hand over the last half second (in the pixels it was drawn in).
        if live.drawn_in == view {
            let end = live.samples.last().map_or(0.0, |s| s[0]);
            let from = live.samples.partition_point(|s| s[0] < end - 0.5);
            let trail: Vec<Pos2> = live.samples[from..].iter().map(|s| map.to_screen([s[1], s[2]])).collect();
            painter.add(Shape::line(trail, Stroke::new(1.0, Color32::from_white_alpha(110))));
        }
    }

    // The Sketch tool: a crosshair (none while holding, if so set) and what the keys do.
    if tool == Tool::Sketch || live.is_some() {
        if let Some(pos) = response.hover_pos() {
            let hide = live.is_some() && world.resource::<super::viewport::PointerView>().hide_pointer;
            ui.ctx().set_cursor_icon(if hide { CursorIcon::None } else { CursorIcon::Crosshair });
            painter.circle_stroke(pos, 7.0, Stroke::new(1.0, if live.is_some() { LIVE } else { style::TEXT }));
            if live.is_none() {
                brush_outline(painter, map, world, selected, pos);
            }
        }
        let t = world.resource::<Transport>();
        let fps = t.fps.as_f64();
        let name = |e: Option<Entity>| e.and_then(|e| world.get::<Name>(e)).map(|n| n.to_string());
        let text = match live {
            Some(l) => format!(
                "⏺ recording into {}{} · frame {} · {} · {}{}wheel: {} · Esc cancels",
                name(l.target).unwrap_or_else(|| "a new sketch".into()),
                if l.stroke.size == 0.0 { " (move only)" } else { "" },
                t.frame(),
                if t.playing { "playing: recording across frames" } else { "paused: retaking this frame (arrow keys: the next)" },
                if l.stroke.size == 0.0 { String::new() } else { format!("size ×{:.2} · ", l.stroke.scale) },
                if l.stroke.falloff > 0.0 { format!("falloff {:.2} s ({:.0} frames) · ", l.stroke.falloff, l.stroke.falloff as f64 * fps) } else { String::new() },
                match tt_core::capture::wheel_target(world.resource::<tt_core::capture::SketchDefaults>().wheel, &l.stroke) {
                    tt_core::capture::WheelMode::Still => "off (the view holds still)",
                    tt_core::capture::WheelMode::Zoom => "zoom",
                    tt_core::capture::WheelMode::Size => "size",
                    tt_core::capture::WheelMode::Falloff => "falloff",
                    tt_core::capture::WheelMode::Both => "size + falloff",
                },
            ),
            None => match name(selected) {
                Some(n) => format!("SKETCH · hold to record into {n} at the shown frame · Space plays at {} (Q/E) · Ctrl+hold: move only · Shift+hold: new sketch · click: select · D/Esc exits", super::timeline::rate_label(t.rate)),
                None => format!("SKETCH · hold to start a sketch at the shown frame · Space plays at {} (Q/E) · click a box to select its sketch · D/Esc exits", super::timeline::rate_label(t.rate)),
            },
        };
        let color = if live.is_some() { LIVE } else { style::TEXT };
        let galley = painter.layout_no_wrap(text, FontId::proportional(13.0), color);
        // Its own row under the frame counter and the breadcrumb, so nothing overlaps.
        let r = Align2::LEFT_TOP.anchor_size(map.panel.left_top() + Vec2::new(15.0, 65.0), galley.size()).expand(5.0);
        painter.rect_filled(r, 4.0, Color32::from_black_alpha(190));
        painter.galley(r.min + Vec2::splat(5.0), galley, color);
    }
}

/// Subjects (tt_core::subject): a diamond turned with each one's angle, a tick
/// the way it faces, and its name. The selected one also shows its path, its
/// members joined to it, and (when an offset moves it from there) where its
/// members' motion alone puts it, dashed.
#[allow(clippy::too_many_arguments)]
fn subjects(painter: &Painter, map: &ViewportMapping, world: &World, list: &[(Entity, tt_core::signal::SignalId)], picked: &[Entity], frame: FrameIndex, space: &dyn Fn(FrameIndex) -> SpaceMap) {
    let store = world.resource::<SignalStore>();
    // A source point on frame f, in the shown space's pixels.
    let shown = |f: FrameIndex, x: f32, y: f32| {
        let (x, y) = (x as f64, y as f64);
        let b = space(f).box_from_source([x, y, x, y, x, y]);
        [b[0], b[1]]
    };
    for (e, sig) in list {
        let Some(sig) = store.get(*sig) else { continue };
        let value = |f: FrameIndex| sig.get(f).filter(|v| v.len() >= tt_core::subject::SUBJECT_CHANNELS);
        let selected = picked.contains(e);
        let color = if selected { SUBJECT } else { SUBJECT.gamma_multiply(0.6) };
        if selected {
            path(painter, map, frame, |f| value(f).map(|v| shown(f, v[0], v[1])), color);
        }
        let Some(v) = value(frame) else { continue };
        let p = map.to_screen(shown(frame, v[0], v[1]));
        if selected {
            for m in tt_core::subject::members_of(world, *e) {
                if let Some(q) = world.get::<Output>(m).and_then(|o| store.get(o.0)).and_then(|s| s.get(frame)).filter(|q| q.len() >= 2) {
                    painter.add(Shape::dashed_line(&[p, map.to_screen(shown(frame, q[0], q[1]))], Stroke::new(1.0, color.gamma_multiply(0.45)), 3.0, 3.0));
                }
            }
            let pushed = map.to_screen(shown(frame, v[8], v[9]));
            if (pushed - p).length() > 2.0 {
                painter.add(Shape::dashed_line(&[pushed, p], Stroke::new(1.0, color.gamma_multiply(0.7)), 2.0, 3.0));
                painter.circle_stroke(pushed, 3.0, Stroke::new(1.0, color.gamma_multiply(0.7)));
            }
        }
        let (s, c) = v[6].sin_cos();
        let r = if selected { 9.0 } else { 7.0 };
        let turned = |x: f32, y: f32| p + Vec2::new(c * x - s * y, s * x + c * y);
        painter.add(Shape::closed_line(vec![turned(r, 0.0), turned(0.0, r), turned(-r, 0.0), turned(0.0, -r)], Stroke::new(if selected { 2.0 } else { 1.5 }, color)));
        painter.line_segment([turned(r, 0.0), turned(r + 7.0, 0.0)], Stroke::new(1.5, color));
        if let Some(name) = world.get::<Name>(*e) {
            painter.text(p + Vec2::new(r + 4.0, -r - 2.0), Align2::LEFT_BOTTOM, name.as_str(), FontId::proportional(12.0), color);
        }
    }
}

/// Before a stroke: the box a hold here would get now, dashed and light:
/// sized by the hand's jiggle over the last moments (as a held frame is;
/// `tt_core::sketch::hold_box`), times the stroke's size, so moving or
/// steadying the hand shows what it does before you press. With the size
/// and falloff the next stroke starts with.
fn brush_outline(painter: &Painter, map: &ViewportMapping, world: &World, target: Option<Entity>, pos: Pos2) {
    let defaults = world.resource::<tt_core::capture::SketchDefaults>();
    let params = target.and_then(|e| world.get::<tt_core::sketch::SketchParams>(e)).unwrap_or(&defaults.params);
    let sized = defaults.stroke.sized(params);
    let trail: Vec<[f64; 3]> = world.resource::<tt_core::tool::PointerTrail>().0.iter().copied().collect();
    let now = world.resource::<tt_core::time::WallClock>().now;
    let ppc = map.points_per_canvas() as f32;
    // (Its size from the jiggle; centred on the pointer itself, which the preview follows exactly.)
    let half = match tt_core::sketch::hold_box(&trail, now, &sized) {
        Some(b) => Vec2::new(((b[4] - b[2]) / 2.0) as f32, ((b[5] - b[3]) / 2.0) as f32) * ppc,
        None => Vec2::new(sized.pad.max(sized.min_half), sized.pad.max(sized.min_half_y)) * ppc,
    };
    let r = Rect::from_center_size(pos, 2.0 * half);
    let c = style::HAND.gamma_multiply(0.6);
    let stroke = Stroke::new(1.0, c);
    for (a, b) in [(r.left_top(), r.right_top()), (r.right_top(), r.right_bottom()), (r.right_bottom(), r.left_bottom()), (r.left_bottom(), r.left_top())] {
        painter.add(Shape::dashed_line(&[a, b], stroke, 4.0, 4.0));
    }
    let s = &defaults.stroke;
    let fps = world.resource::<Transport>().fps.as_f64();
    let falloff = if s.falloff > 0.0 { format!("falloff {:.2} s ({:.0} frames)", s.falloff, s.falloff as f64 * fps) } else { "no falloff".to_string() };
    painter.text(r.left_bottom() + Vec2::new(0.0, 3.0), Align2::LEFT_TOP, format!("size \u{d7}{:.2} \u{b7} {falloff}", s.scale), FontId::proportional(10.0), c);
    // Keep it moving with the hand's jiggle as it settles.
    painter.ctx().request_repaint_after(std::time::Duration::from_millis(33));
}

/// `[x, y, left, top, right, bottom]`: the region's outline and the point;
/// `thick` while it is being recorded.
fn region(painter: &Painter, map: &ViewportMapping, b: [f64; 6], color: Color32, stale: bool, thick: bool) {
    let r = Rect::from_min_max(map.to_screen([b[2], b[3]]), map.to_screen([b[4], b[5]]));
    let width = if thick { 3.0 } else if stale { 1.0 } else { 1.5 };
    let stroke = Stroke::new(width, color.gamma_multiply(if stale { 0.5 } else { 1.0 }));
    painter.rect_stroke(r, 0.0, stroke, StrokeKind::Middle);
    let p = map.to_screen([b[0], b[1]]);
    painter.line_segment([p - Vec2::new(5.0, 0.0), p + Vec2::new(5.0, 0.0)], stroke);
    painter.line_segment([p - Vec2::new(0.0, 5.0), p + Vec2::new(0.0, 5.0)], stroke);
}

/// The point's path around the playhead: behind solid, ahead faint; gaps break it.
fn path(painter: &Painter, map: &ViewportMapping, frame: FrameIndex, at: impl Fn(FrameIndex) -> Option<[f64; 2]>, color: Color32) {
    for (range, c) in [((frame - PATH_FRAMES)..=frame, color.gamma_multiply(0.8)), (frame..=(frame + PATH_FRAMES), color.gamma_multiply(0.3))] {
        let mut run: Vec<Pos2> = Vec::new();
        for f in range {
            match at(f) {
                Some(p) => run.push(map.to_screen(p)),
                None if run.len() > 1 => {
                    painter.add(Shape::line(std::mem::take(&mut run), Stroke::new(1.0, c)));
                }
                None => run.clear(),
            }
        }
        if run.len() > 1 {
            painter.add(Shape::line(run, Stroke::new(1.0, c)));
        }
    }
}
