//! Trackers in the panels: their points and paths over the video, their
//! progress in the top bar and the inspector, and the Track command.

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
use tt_track::runner::{SideStatus, TrackStatus};
use tt_track::{Tracker, guide_of, is_tracker, trackers_of};

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

/// Every tracker's point at the playhead (the feature's box around it), and
/// its path: selected trackers ±90 frames, others a short tail. Frames below
/// the tracker's score threshold (lost: following the guide) are red.
pub fn draw(painter: &Painter, map: &ViewportMapping, world: &World, list: &[(Entity, SignalId)], frame: FrameIndex, space: &dyn Fn(FrameIndex) -> SpaceMap) {
    let selected = &world.resource::<Selection>().entities;
    let store = world.resource::<SignalStore>();
    for &(e, id) in list {
        let Some(sig) = store.get(id) else { continue };
        let lit = selected.contains(&e);
        let min_score = world.get::<Tracker>(e).map_or(0.5, |t| t.min_score);
        let base = if lit { TRACK } else { TRACK.gamma_multiply(0.55) };
        let reach = if lit { PATH_FRAMES } else { SHORT_PATH };
        let at = |f: FrameIndex| {
            sig.get(f).map(|v| {
                let [x, y] = space(f).from_source([v[0] as f64, v[1] as f64]);
                (map.to_screen([x, y]), v[6] < min_score)
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

        let Some(v) = sig.get(frame) else { continue };
        let b = space(frame).box_from_source(std::array::from_fn(|c| v[c] as f64));
        let lost = v[6] < min_score;
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
            let text = if lost { format!("{name} · lost ({:.2})", v[6]) } else { format!("{name} · {:.2}", v[6]) };
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

/// Inspector: a tracker's progress and errors; a sketch's Track button and trackers.
pub fn inspector(ui: &mut egui::Ui, world: &mut World, e: Entity) {
    if is_tracker(world, e) {
        if let Some(err) = world.get::<OpError>(e) {
            ui.colored_label(LOST, format!("⚠ {}", err.0));
        }
        let guide = guide_of(world, e).and_then(|g| world.get::<Name>(g)).map(|n| n.to_string());
        ui.label(egui::RichText::new(format!("follows {} · T re-seeds it at the playhead", guide.as_deref().unwrap_or("nothing"))).color(style::MUTED));
        let status = world.get::<TrackStatus>(e).cloned().unwrap_or_default();
        let id = world.get::<Output>(e).map(|o| o.0);
        let (covered, lost) = id.and_then(|id| world.resource::<SignalStore>().get(id)).map_or((0, 0), |sig| {
            let min = world.get::<Tracker>(e).map_or(0.5, |t| t.min_score);
            let n = world.resource::<Transport>().frame_count;
            (0..n).filter_map(|f| sig.get(f)).fold((0, 0), |(c, l), v| (c + 1, l + (v[6] < min) as usize))
        });
        ui.label(format!("{covered} frames tracked · {lost} lost (followed the guide){}", if status.rendition.is_empty() { String::new() } else { format!(" · reads the {}", status.rendition) }));
        for (s, forward) in [(status.forward, true), (status.backward, false)] {
            let Some(s) = s else { continue };
            let anchor = world.get::<Tracker>(e).map_or(0, |t| t.anchor);
            let total = (s.to - anchor).abs().max(1) as f32;
            let done = (s.at - anchor).abs() as f32 / total;
            ui.add(egui::ProgressBar::new(done.clamp(0.0, 1.0)).text(side_line(&s, forward)));
        }
        ui.separator();
        return;
    }
    if !tt_core::sketch::is_sketch(world, e) {
        return;
    }
    let chord = world.resource::<tt_core::input::Keymap>().chord_for(Action::Track).unwrap_or_default();
    let existing = trackers_of(world, e);
    ui.horizontal(|ui| {
        if ui.button(format!("⌖ Track from here ({chord})")).on_hover_text("A tracker that follows this sketch's subject pixel by pixel, forward and backward from the playhead, in the view you're looking at. The sketch tells it where to look.").clicked() {
            world.resource_mut::<PendingActions>().push(Action::Track);
        }
        if !existing.is_empty() {
            ui.label(egui::RichText::new(format!("{} tracker(s)", existing.len())).color(style::MUTED));
        }
    });
}
