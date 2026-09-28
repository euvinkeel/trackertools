//! Viewport overlays (DESIGN §8.1 live feedback): every sketch's region and
//! point at the playhead (the selected one bright, with its path; the rest
//! faint), the stroke in progress laid over the sketch it edits, and the
//! Sketch tool's cursor and hints. (A click on a region selects its sketch:
//! tt_core's tool.rs.)

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

pub const LIVE: Color32 = Color32::from_rgb(0xfb, 0xbf, 0x24);
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
        let own = |f: FrameIndex| sig.get(f).map(|v| std::array::from_fn::<f64, 6, _>(|c| v[c] as f64));
        let value = |f: FrameIndex| if editing == Some(*e) { live.and_then(|l| l.preview_at(f)).or_else(|| own(f)) } else { own(f) };
        let color = if editing == Some(*e) {
            LIVE
        } else if lit.contains(e) && live.is_none() {
            style::ACCENT
        } else {
            Color32::from_white_alpha(70)
        };
        if editing == Some(*e) || (lit.contains(e) && live.is_none()) {
            path(painter, map, frame, |f| value(f).map(|v| to_canvas(f, v)).map(|v| [v[0], v[1]]), color);
        }
        let Some(v) = value(frame) else { continue };
        let stale = editing != Some(*e) && sig.state(frame) == FrameState::Stale;
        region(painter, map, to_canvas(frame, v), color, stale);
    }

    super::tracks::draw(painter, map, world, &trackers, frame, &space);

    if let Some(live) = live {
        if live.target.is_none() {
            path(painter, map, frame, |f| live.preview_at(f).map(|b| to_canvas(f, b)).map(|b| [b[0], b[1]]), LIVE);
            if let Some(b) = live.preview_at(frame) {
                region(painter, map, to_canvas(frame, b), LIVE, false);
            }
        }
        // Playing, the hand is `lag` behind the shown frame: its newest box, dimmed.
        if live.preview_at(frame).is_none()
            && let Some((g, b)) = (frame - 120..frame).rev().find_map(|g| live.visits(g).then(|| live.preview_at(g)).flatten().map(|b| (g, b)))
        {
            region(painter, map, to_canvas(g, b), LIVE, true);
        }
        // The raw hand over the last half second (in the pixels it was drawn in).
        if live.drawn_in == view {
            let end = live.samples.last().map_or(0.0, |s| s[0]);
            let from = live.samples.partition_point(|s| s[0] < end - 0.5);
            let trail: Vec<Pos2> = live.samples[from..].iter().map(|s| map.to_screen([s[1], s[2]])).collect();
            painter.add(Shape::line(trail, Stroke::new(1.0, Color32::from_white_alpha(110))));
        }
    }

    // The Sketch tool: a crosshair and what the keys do.
    if tool == Tool::Sketch || live.is_some() {
        if let Some(pos) = response.hover_pos() {
            ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
            painter.circle_stroke(pos, 7.0, Stroke::new(1.0, if live.is_some() { LIVE } else { style::TEXT }));
        }
        let t = world.resource::<Transport>();
        let fps = t.fps.as_f64();
        let name = |e: Option<Entity>| e.and_then(|e| world.get::<Name>(e)).map(|n| n.to_string());
        let text = match live {
            Some(l) => format!(
                "⏺ recording into {}{} · frame {} · {} · size ×{:.2} · falloff {:.2} s ({:.0} frames) · wheel: {} · Esc cancels",
                name(l.target).unwrap_or_else(|| "a new sketch".into()),
                if l.stroke.size == 0.0 { " (move only)" } else { "" },
                t.frame(),
                if t.playing { "playing: recording across frames" } else { "paused: editing this instant" },
                l.stroke.scale,
                l.stroke.falloff,
                l.stroke.falloff as f64 * fps,
                match world.resource::<tt_core::capture::SketchDefaults>().wheel {
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

/// `[x, y, left, top, right, bottom]`: the region's outline and the point.
fn region(painter: &Painter, map: &ViewportMapping, b: [f64; 6], color: Color32, stale: bool) {
    let r = Rect::from_min_max(map.to_screen([b[2], b[3]]), map.to_screen([b[4], b[5]]));
    let stroke = Stroke::new(if stale { 1.0 } else { 1.5 }, color.gamma_multiply(if stale { 0.5 } else { 1.0 }));
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
