//! Trackers in the panels: their points, paths and looks over the video, the
//! Track tool's preview and hints, their progress in the top bar, and the
//! inspector (looks and their masks).

use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::name::Name;
use bevy_ecs::prelude::*;
use egui::{Align2, Color32, FontId, Painter, Pos2, Rect, Shape, Stroke, StrokeKind, Vec2};
use tt_core::input::{Action, PendingActions};
use tt_core::op::{OpError, Operator, Output};
use tt_core::selection::Selection;
use tt_core::signal::{FrameState, SignalId, SignalStore};
use tt_core::time::FrameIndex;
use tt_core::transport::Transport;
use tt_core::view::SpaceMap;
use tt_core::tool::{ActiveTool, Tool};
use tt_track::look::{Look, looks_of};
use tt_track::runner::{SideStatus, TrackStatus};
use tt_track::tool::TrackTool;
use tt_track::{LOST as LOST_FLAG, guide_of, is_tracker, trackers_of};

use super::viewport::ViewportMapping;
use crate::style;

pub const TRACK: Color32 = Color32::from_rgb(0x22, 0xd3, 0xee);
pub const LOST: Color32 = Color32::from_rgb(0xf4, 0x3f, 0x5e);
/// Frames of path drawn either side of the playhead: selected trackers, others.
const PATH_FRAMES: FrameIndex = 90;
const SHORT_PATH: FrameIndex = 12;

/// Live trackers with their output signals.
pub fn list(world: &mut World) -> Vec<(Entity, SignalId)> {
    let mut q = world.query_filtered::<(Entity, &Operator, &Output), Without<Disabled>>();
    q.iter(world).filter(|(_, o, _)| o.kind == "track").map(|(e, _, o)| (e, o.0)).collect()
}

/// Every tracker's point at the playhead (its pattern's box around it), and
/// its path: selected trackers ±90 frames, others a short tail. Flagged
/// frames (lost, or outside the guide's box) are red. A selected tracker's
/// looks show on their own frames.
pub fn draw(painter: &Painter, map: &ViewportMapping, world: &World, list: &[(Entity, SignalId)], frame: FrameIndex, space: &dyn Fn(FrameIndex) -> SpaceMap) {
    let selected = &world.resource::<Selection>().entities;
    let store = world.resource::<SignalStore>();
    for &(e, id) in list {
        let Some(sig) = store.get(id) else { continue };
        let looks = looks_of(world, e);
        let lit = selected.contains(&e) || looks.iter().any(|l| selected.contains(l));
        let base = if lit { TRACK } else { TRACK.gamma_multiply(0.55) };
        let reach = if lit { PATH_FRAMES } else { SHORT_PATH };
        let at = |f: FrameIndex| {
            sig.get(f).map(|v| {
                let [x, y] = space(f).from_source([v[0] as f64, v[1] as f64]);
                (map.to_screen([x, y]), tt_track::flags(v) != 0)
            })
        };
        // The path, split where it breaks, turns lost, or passes the playhead (ahead is fainter).
        let line = |run: &[Pos2], (lost, ahead): (bool, bool)| {
            if run.len() > 1 {
                let c = if lost { LOST } else { base };
                painter.add(Shape::line(run.to_vec(), Stroke::new(1.0, c.gamma_multiply(if ahead { 0.35 } else { 0.85 }))));
            }
        };
        let (mut run, mut kind): (Vec<Pos2>, Option<(bool, bool)>) = (Vec::new(), None);
        for f in frame - reach..=frame + reach {
            match at(f) {
                Some((p, lost)) => {
                    let k = (lost, f > frame);
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

        // Its looks on this frame: the patterns it follows, as drawn.
        if lit {
            for l in looks.iter().filter_map(|l| Some((*l, world.get::<Look>(*l)?))).filter(|(_, l)| l.frame == frame) {
                let b = space(frame).box_from_source(l.1.rect());
                let r = Rect::from_min_max(map.to_screen([b[2], b[3]]), map.to_screen([b[4], b[5]]));
                let chosen = selected.contains(&l.0);
                painter.rect_stroke(r, 0.0, Stroke::new(if chosen { 2.0 } else { 1.0 }, Color32::WHITE.gamma_multiply(0.8)), StrokeKind::Outside);
                let name = world.get::<Name>(l.0).map_or("look".to_string(), |n| n.to_string());
                painter.text(r.left_bottom() + Vec2::new(0.0, 2.0), Align2::LEFT_TOP, name, FontId::proportional(10.0), Color32::WHITE.gamma_multiply(0.8));
            }
        }

        let Some(v) = sig.get(frame) else { continue };
        let b = space(frame).box_from_source(std::array::from_fn(|c| v[c] as f64));
        let flags = tt_track::flags(v);
        let lost = flags != 0;
        let stale = sig.state(frame) == FrameState::Stale;
        let color = if lost { LOST } else { base }.gamma_multiply(if stale { 0.5 } else { 1.0 });
        let r = Rect::from_min_max(map.to_screen([b[2], b[3]]), map.to_screen([b[4], b[5]]));
        let stroke = Stroke::new(if lit { 1.5 } else { 1.0 }, color);
        painter.rect_stroke(r, 2.0, stroke, StrokeKind::Middle);
        let p = map.to_screen([b[0], b[1]]);
        painter.line_segment([p - Vec2::new(3.0, 0.0), p + Vec2::new(3.0, 0.0)], stroke);
        painter.line_segment([p - Vec2::new(0.0, 3.0), p + Vec2::new(0.0, 3.0)], stroke);
        if lit {
            let name = world.get::<Name>(e).map_or("Tracker".to_string(), |n| n.to_string());
            let text = match flags {
                0 => format!("{name} · {:.2}", v[6]),
                f if f & LOST_FLAG != 0 => format!("{name} · lost ({:.2})", v[6]),
                _ => format!("{name} · outside the sketch ({:.2})", v[6]),
            };
            painter.text(r.right_top() + Vec2::new(4.0, 0.0), Align2::LEFT_TOP, text, FontId::proportional(11.0), color);
        }
    }
}

fn side_line(s: &SideStatus, forward: bool) -> String {
    let arrow = if forward { "▸" } else { "◂" };
    let state = if s.waiting { " · waiting for the playhead".to_string() } else if s.fps > 0.0 { format!(" · {:.0} fps", s.fps) } else { String::new() };
    format!("{arrow} at frame {}, to {}{state}", s.at, s.to)
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
    let text = if waiting { format!("⌖ {} tracker(s) waiting for the playhead", busy.len()) } else { format!("⌖ tracking {} · {:.0} fps", busy.len(), fps) };
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

/// The Track tool on the video: its pattern (dashed: the brush at the
/// pointer, or the rectangle being dragged) and a row of hints.
pub fn draw_tool(ui: &egui::Ui, painter: &Painter, response: &egui::Response, world: &mut World, map: &ViewportMapping) {
    if world.resource::<ActiveTool>().0 != Tool::Track {
        return;
    }
    let tool = world.resource::<TrackTool>().clone();
    let dashed = |r: Rect, c: Color32| {
        let s = Stroke::new(1.0, c);
        for (a, b) in [(r.left_top(), r.right_top()), (r.right_top(), r.right_bottom()), (r.right_bottom(), r.left_bottom()), (r.left_bottom(), r.left_top())] {
            painter.add(Shape::dashed_line(&[a, b], s, 4.0, 3.0));
        }
    };
    if let Some(pos) = response.hover_pos() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
        match tool.drag {
            Some((start, _, _)) => dashed(Rect::from_two_pos(map.to_screen(start), pos), TRACK),
            None => dashed(Rect::from_center_size(pos, Vec2::splat(2.0 * tool.brush)), TRACK.gamma_multiply(0.7)),
        }
    }
    let world_ref: &World = world;
    let selected = world_ref.resource::<Selection>().primary().filter(|e| is_tracker(world_ref, *e));
    let name = selected.and_then(|e| world_ref.get::<Name>(e)).map_or("the tracker".to_string(), |n| n.to_string());
    let text = match (selected, tool.reseed) {
        (Some(t), Some(r)) if t == r => format!("TRACK · drag around the subject on this frame: {name} starts again from it · T/Esc exits"),
        (Some(_), _) => format!("TRACK · drag around the subject where {name} missed it: a new look, pinned here · Shift+drag: a new tracker · T/Esc exits"),
        _ => "TRACK · drag around what to follow (its pattern) · click: a point, the dashed box's size (Ctrl+wheel) · T/Esc exits".to_string(),
    };
    let galley = painter.layout_no_wrap(text, FontId::proportional(13.0), TRACK);
    let r = Align2::LEFT_TOP.anchor_size(map.panel.left_top() + Vec2::new(15.0, 65.0), galley.size()).expand(5.0);
    painter.rect_filled(r, 4.0, Color32::from_black_alpha(190));
    painter.galley(r.min + Vec2::splat(5.0), galley, TRACK);
    if let Some(why) = &tool.refused {
        let g = painter.layout_no_wrap(why.clone(), FontId::proportional(13.0), LOST);
        let r2 = Align2::LEFT_TOP.anchor_size(r.left_bottom() + Vec2::new(5.0, 8.0), g.size()).expand(5.0);
        painter.rect_filled(r2, 4.0, Color32::from_black_alpha(190));
        painter.galley(r2.min + Vec2::splat(5.0), g, LOST);
    }
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

/// Inspector: a tracker's progress, errors and looks; a look's mask; a sketch's Track buttons.
pub fn inspector(ui: &mut egui::Ui, world: &mut World, e: Entity) {
    if world.get::<Look>(e).is_some() {
        ui.label(egui::RichText::new("A look: what the tracker's subject looks like here. Paint which pixels are the subject.").color(style::MUTED));
        super::look_editor::ui(ui, world, e);
        ui.separator();
        return;
    }
    if is_tracker(world, e) {
        if let Some(err) = world.get::<OpError>(e) {
            ui.colored_label(LOST, format!("⚠ {}", err.0));
        }
        let guide = guide_of(world, e).and_then(|g| world.get::<Name>(g)).map(|n| n.to_string());
        ui.label(egui::RichText::new(format!("searches inside {} · where it misses: Track tool, drag around the subject (a new look, pinned there) · \"Re-seed here\" starts it again from the playhead", guide.as_deref().unwrap_or("nothing"))).color(style::MUTED));
        let status = world.get::<TrackStatus>(e).cloned().unwrap_or_default();
        let (covered, lost) = counts(ui, world, e);
        ui.label(format!("{covered} frames tracked · {lost} flagged (lost, or outside the sketch){}", if status.rendition.is_empty() { String::new() } else { format!(" · reads the {}", status.rendition) }));
        // Its looks: select one to paint its mask.
        let looks = looks_of(world, e);
        let mut remove = None;
        ui.horizontal_wrapped(|ui| {
            ui.label("Looks:");
            for l in &looks {
                let Some(look) = world.get::<Look>(*l) else { continue };
                let painted = if look.painted().is_some() { " · masked" } else { " · unpainted" };
                let label = format!("frame {} · {:.0}×{:.0}{painted}", look.frame, 2.0 * look.half_w, 2.0 * look.half_h);
                let f = look.frame;
                if ui.button(label).on_hover_text("Select it to see and paint which pixels are the subject; the playhead goes to its frame").clicked() {
                    world.resource_mut::<Selection>().select_only(*l);
                    world.resource_mut::<PendingActions>().push(Action::Seek(f));
                }
                if ui.small_button("✕").on_hover_text("Remove this look (it re-tracks without it)").clicked() {
                    remove = Some(*l);
                }
            }
        });
        if let Some(l) = remove {
            tt_core::commands::delete(world, &[l]);
        }
        if ui.button("Re-seed here").on_hover_text("Start it again from the playhead, from your look on this frame (with none here, drag around the subject first)").clicked() {
            world.resource_mut::<PendingActions>().push(Action::Track);
        }
        for (s, forward) in [(status.forward, true), (status.backward, false)] {
            let Some(s) = s else { continue };
            // (The anchor as tracked: moved into the guide's frames.)
            let total = (s.to - status.anchor).abs().max(1) as f32;
            let done = (s.at - status.anchor).abs() as f32 / total;
            ui.add(egui::ProgressBar::new(done.clamp(0.0, 1.0)).text(side_line(&s, forward)));
        }
        ui.separator();
        return;
    }
    if !tt_core::sketch::is_sketch(world, e) {
        return;
    }
    let chord = world.resource::<tt_core::input::Keymap>().chord_for(Action::Tool(Tool::Track)).unwrap_or_default();
    let existing = trackers_of(world, e);
    ui.horizontal(|ui| {
        if ui.button(format!("⌖ Track tool ({chord})")).on_hover_text("Drag a rectangle around what to follow in this sketch (or click a point). The tracker searches inside this sketch's box, in its view.").clicked() {
            world.resource_mut::<ActiveTool>().0 = Tool::Track;
        }
        if ui.button("Track its centre").on_hover_text("A quick tracker on this sketch's own point at the playhead (a square of its box); re-centred on the sketch when done").clicked() {
            world.resource_mut::<PendingActions>().push(Action::Track);
        }
        if !existing.is_empty() {
            ui.label(egui::RichText::new(format!("{} tracker(s)", existing.len())).color(style::MUTED));
        }
    });
}
