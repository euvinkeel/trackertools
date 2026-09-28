//! Viewport overlays (DESIGN §8.1 live feedback): every sketch's region and
//! point at the playhead (the selected one bright, with its path; the rest
//! faint), the capture in progress, and the Sketch tool's cursor and hints.

use bevy_ecs::prelude::*;
use egui::{Align2, Color32, CursorIcon, FontId, Painter, Pos2, Rect, Shape, Stroke, StrokeKind, Vec2};
use tt_core::capture::LiveCapture;
use tt_core::input::Keymap;
use tt_core::op::{Inputs, Operator, Output};
use tt_core::selection::Selection;
use tt_core::signal::{FrameState, SignalStore};
use tt_core::time::FrameIndex;
use tt_core::tool::{ActiveTool, Tool};
use tt_core::transport::Transport;

use super::viewport::ViewportMapping;
use crate::style;

const LIVE: Color32 = Color32::from_rgb(0xfb, 0xbf, 0x24);
/// Frames of path drawn either side of the playhead for the selected sketch.
const PATH_FRAMES: FrameIndex = 90;

pub fn draw(ui: &egui::Ui, painter: &Painter, response: &egui::Response, world: &mut World, map: &ViewportMapping, frame: FrameIndex) {
    let selection = world.resource::<Selection>().clone();
    let mut q = world.query::<(Entity, &Operator, &Output, Option<&Inputs>)>();
    let sketches: Vec<(Entity, tt_core::signal::SignalId, bool)> = q
        .iter(world)
        .filter(|(_, o, _, _)| o.kind == "sketch")
        .map(|(e, _, out, inputs)| {
            let selected = selection.is_selected(e) || inputs.is_some_and(|i| i.0.iter().any(|(_, p)| selection.is_selected(*p)));
            (e, out.0, selected)
        })
        .collect();
    let store = world.resource::<SignalStore>();
    let live = world.resource::<LiveCapture>().0.as_ref();

    for (_, signal, selected) in &sketches {
        let Some(sig) = store.get(*signal) else { continue };
        if *selected && live.is_none() {
            path(painter, map, frame, |f| sig.get(f).map(|v| [v[0] as f64, v[1] as f64]), style::ACCENT);
        }
        let Some(v) = sig.get(frame) else { continue };
        let stale = sig.state(frame) == FrameState::Stale;
        let color = match (selected, live.is_some()) {
            (true, false) => style::ACCENT,
            _ => Color32::from_white_alpha(70),
        };
        region(painter, map, [v[0], v[1], v[2], v[3], v[4], v[5]].map(|x| x as f64), color, stale);
    }

    if let Some(live) = live {
        // The raw hand over the last half second, the smoothed path so far, and the live region.
        let recent: Vec<Pos2> = {
            let end = live.samples.last().map_or(0.0, |s| s[0]);
            let from = live.samples.partition_point(|s| s[0] < end - 0.5);
            live.samples[from..].iter().map(|s| map.to_screen([s[1], s[2]])).collect()
        };
        painter.add(Shape::line(recent, Stroke::new(1.0, Color32::from_white_alpha(110))));
        path(painter, map, frame, |f| live.preview_at(f).map(|b| [b[0], b[1]]), LIVE);
        if let Some(b) = live.preview_at(frame) {
            region(painter, map, b, LIVE, false);
        }
    }

    // The Sketch tool: a crosshair and what the keys do.
    let tool = world.resource::<ActiveTool>().0;
    if tool == Tool::Sketch || live.is_some() {
        if let Some(pos) = response.hover_pos() {
            ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
            let c = if live.is_some() { LIVE } else { style::TEXT };
            painter.circle_stroke(pos, 7.0, Stroke::new(1.0, c));
        }
        let t = world.resource::<Transport>();
        let key = world.resource::<Keymap>().simulate;
        let key = format!("{key:?}");
        let text = match live {
            Some(l) => format!(
                "● sketching at {:.0}% · {:.1} s · {} — hold {key} to freeze · Esc cancels",
                l.rate * 100.0,
                l.samples.last().map_or(0.0, |s| s[0]),
                if t.playing { "playing" } else { "frozen" }
            ),
            None => format!("SKETCH · press and hold to follow at {:.0}% speed ([ / ] change it) · hold {key} to freeze · D or Esc exits", t.rate * 100.0),
        };
        let color = if live.is_some() { LIVE } else { style::TEXT };
        let galley = painter.layout_no_wrap(text, FontId::proportional(13.0), color);
        let r = Align2::CENTER_TOP.anchor_size(map.panel.center_top() + Vec2::new(0.0, 8.0), galley.size()).expand(5.0);
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
