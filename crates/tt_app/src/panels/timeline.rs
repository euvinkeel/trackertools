//! Timeline: transport controls, a zoomable ruler you scrub on, a strip
//! showing which frames the decode service holds in its cache, and below
//! them one lane per timeline object: each sketch (the frames it covers;
//! under a selected sketch, a tick per stroke), its view, and its trackers
//! (their score, flagged frames and job progress). The visible range and
//! lane scroll are session state.
//!
//! - Every lane's ends are its lifetime (`tt_core::span`): drag either end to
//!   say when the object begins and ends. Nothing is deleted: frames outside
//!   are drawn faint, and dragging back out brings them back. With snapping
//!   on, an end snaps to the playhead and to other objects' ends. One undo
//!   step per drag.
//! - Scrub by dragging on the ruler. On the lanes, a click selects (Ctrl
//!   toggles, Shift adds), a drag draws a box that selects what it touches
//!   (past the top or bottom it scrolls the lanes; Esc drops it), a
//!   double-click enters that sketch's view, a right-click opens the menu.
//! - Drag a manual dot (its name or its frames) onto a tracker's lane to
//!   merge it in: what it drew overrides the tracker's automatic results on
//!   those frames, and the dot goes (one undo step;
//!   `tt_track::human::merge_dots`). Selected dots go together.
//! - A stroke that starts scrolls its lane into view.
//! - In and out points (I / O; Alt+X clears; `tt_core::marks`): what an
//!   export covers, shaded outside it on the ruler and the lanes. Drag a
//!   mark's bracket on the ruler to move it (one undo step, snapping like the
//!   playhead); the playhead goes with it, so the viewport shows the frame
//!   the export starts or ends on.
//! - Wheel on the ruler or Ctrl+wheel zooms time; Shift+wheel pans time; the
//!   wheel on the lanes scrolls them; a middle-drag pans both.
//! - A column on the left names each lane: what it is (an icon in the visual
//!   language's colours, `crate::icons`) and, for a tracker, what it is doing
//!   (a spinner while it starts or tracks). Rows alternate in shade and the
//!   one under the pointer lights up. A tracker's lane shows its two layers:
//!   what was drawn by hand (orange, along the top) over its automatic
//!   results (cyan), its looks or reset points (white), its score and flagged
//!   frames. With the Sketch tool armed, the selected sketch's lane shows how
//!   far a stroke at the playhead would pull its neighbours (its falloff).

use std::ops::Range;

use bevy_ecs::prelude::*;
use egui::{Align2, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use tt_core::input::{Action, Keymap, PendingActions};
use tt_core::marks::{Marks, marks, set_marks};
use tt_core::time::{FrameIndex, Rational, timecode};
use tt_core::transport::{RATES, Transport};
use tt_core::{AppBuilder, Class, Module};

use bevy_ecs::name::Name;
use tt_core::capture::LiveCapture;
use tt_core::op::Output;
use tt_core::selection::Selection;
use tt_core::signal::{FrameState, SignalStore};
use tt_core::sketch::ClockMap;

use tt_core::span::{Edge, extent_of, span_of};
use tt_track::runner::TrackStatus;

use super::{menu, overlay, tracks};
use crate::media::{ActiveSource, Media};
use crate::style;

/// Visible range of the timeline: `span` frames starting at `start`.
/// `None` span = fit the whole clip.
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct TimelineView {
    pub start: f64,
    pub span: Option<f64>,
    /// How far the lanes are scrolled down (points).
    pub lane_scroll: f32,
}

pub struct TimelineModule;

impl Module for TimelineModule {
    fn build(&self, app: &mut AppBuilder) {
        app.declare::<TimelineView>(Class::Session)
            .declare::<TimelineUi>(Class::Derived)
            .init_resource::<TimelineView>()
            .init_resource::<TimelineUi>();
    }
}

/// Frame ↔ screen mapping for one draw.
#[derive(Clone, Copy)]
struct Scale {
    rect: Rect,
    start: f64,
    span: f64,
}

impl Scale {
    fn x(&self, f: f64) -> f32 {
        self.rect.min.x + ((f - self.start) / self.span) as f32 * self.rect.width()
    }

    fn frame_at(&self, x: f32) -> f64 {
        self.start + ((x - self.rect.min.x) / self.rect.width()) as f64 * self.span
    }

    fn px_per_frame(&self) -> f64 {
        self.rect.width() as f64 / self.span
    }
}

pub fn ui(ui: &mut egui::Ui, world: &mut World) {
    let t = world.resource::<Transport>().clone();
    let keymap = world.resource::<Keymap>();
    let marked = marks(world);
    let mut actions = Vec::new();
    let mut fit = false;

    ui.horizontal(|ui| {
        let button = |ui: &mut egui::Ui, actions: &mut Vec<Action>, text: &str, action: Action, what: &str| {
            let tip = match keymap.chord_for(action) {
                Some(chord) => format!("{what} ({chord})"),
                None => what.to_string(),
            };
            if ui.button(text).on_hover_text(tip).clicked() {
                actions.push(action);
            }
        };
        button(ui, &mut actions, "⏮", Action::GoToStart, "Go to start");
        button(ui, &mut actions, "◀", Action::StepBackward, "Previous frame");
        button(ui, &mut actions, "◀", Action::ShuttleBackward, "Play backward; again: faster");
        button(ui, &mut actions, if t.playing { "⏸" } else { "▶" }, Action::TogglePlay, "Play / pause (also K)");
        button(ui, &mut actions, "▶▶", Action::ShuttleForward, "Play forward; again: faster");
        button(ui, &mut actions, "▶|", Action::StepForward, "Next frame");
        button(ui, &mut actions, "⏭", Action::GoToEnd, "Go to end");
        ui.separator();

        let speed_tip = format!(
            "Playback speed, which is also the capture speed while sketching ({} slower, {} faster)",
            keymap.chord_for(Action::SlowerPlayback).unwrap_or_default(),
            keymap.chord_for(Action::FasterPlayback).unwrap_or_default()
        );
        egui::ComboBox::from_id_salt("rate").width(64.0).selected_text(rate_label(t.rate)).show_ui(ui, |ui| {
            for (i, r) in RATES.iter().enumerate() {
                if ui.selectable_label((r - t.rate).abs() < 1e-9, rate_label(*r)).clicked() {
                    actions.push(Action::SetRate(i as u8));
                }
            }
        })
        .response
        .on_hover_text(speed_tip);
        if ui.selectable_label(t.looping, "⟲ loop").clicked() {
            actions.push(Action::ToggleLoop);
        }
        let snapping = world.resource::<tt_core::commands::TimeSnap>().enabled;
        if ui
            .selectable_label(snapping, "snap")
            .on_hover_text(format!(
                "Scrubbing snaps the playhead to the start and end of sketches, strokes and trackers ({}; Ctrl while scrubbing does the opposite)",
                keymap.chord_for(Action::ToggleSnap).unwrap_or_default()
            ))
            .clicked()
        {
            actions.push(Action::ToggleSnap);
        }
        ui.separator();
        button(ui, &mut actions, "In", Action::MarkIn, "Mark the in point here: the first frame an export renders");
        button(ui, &mut actions, "Out", Action::MarkOut, "Mark the out point here: the last frame an export renders");
        if marked.is_set() {
            let r = marked.frames(t.frame_count);
            let n = r.end - r.start;
            ui.label(egui::RichText::new(format!("{}\u{2013}{} \u{b7} {n} fr", r.start, r.end - 1)).monospace().color(style::RANGE)).on_hover_text(format!(
                "What an export covers: frames {} to {}, both included ({n} frames, {:.2} s). Drag the brackets on the ruler to move them; {} and {} go to them.",
                r.start,
                r.end - 1,
                n as f64 / t.fps.as_f64(),
                keymap.chord_for(Action::GoToIn).unwrap_or_default(),
                keymap.chord_for(Action::GoToOut).unwrap_or_default()
            ));
            button(ui, &mut actions, "Clear", Action::ClearMarks, "Clear the in and out points: exports render the whole video");
        }
        ui.separator();
        ui.monospace(format!("{} / {}", t.frame(), t.last_frame()));
        ui.monospace(timecode(t.frame(), t.fps));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            fit = ui.button("Fit").on_hover_text("Show the whole clip").clicked();
            ui.label(
                egui::RichText::new("wheel on the ruler or Ctrl+wheel: zoom · Shift+wheel: pan · wheel on lanes: scroll · middle-drag: pan both · drag lanes: box select · drag a manual dot onto a tracker: merge · right-click: menu")
                    .weak()
                    .small(),
            );
        });
    });

    let height = ui.available_height().max(44.0);
    let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, style::PANEL);
    if !t.has_media() {
        world.resource_mut::<PendingActions>().0.extend(actions);
        world.resource_mut::<TimelineUi>().marquee = None;
        return;
    }
    // A column of lane names on the left; time runs to the right of it.
    let gutter_w = GUTTER.min(rect.width() * 0.35).max(60.0);
    let track = Rect::from_min_max(Pos2::new(rect.min.x + gutter_w, rect.min.y), rect.max);
    let band = Rect::from_min_size(track.min, Vec2::new(track.width(), 22.0));
    let strip = Rect::from_min_size(Pos2::new(track.min.x, band.max.y + 1.0), Vec2::new(track.width(), 3.0));
    let lanes_area = Rect::from_min_max(Pos2::new(track.min.x, strip.max.y + 4.0), track.max);
    // The lanes' rows, names included: what clicks and boxes hit.
    let rows = Rect::from_min_max(Pos2::new(rect.min.x, lanes_area.min.y), rect.max);

    // Visible range and lane scroll (session state), with zoom/pan/follow applied:
    // wheel over the ruler or Ctrl+wheel zooms time; Shift+wheel (or a
    // trackpad's sideways scroll) pans time; the wheel over the lanes scrolls
    // them; a middle-drag pans both.
    let count = t.frame_count as f64;
    let mut view = *world.resource::<TimelineView>();
    if fit {
        view = TimelineView { lane_scroll: view.lane_scroll, ..TimelineView::default() };
    }
    // At most 40 px per frame, unless the clip is shorter than that.
    let max_span = count * 1.05;
    let min_span = (track.width() as f64 / 40.0).max(4.0).min(max_span);
    let mut span = view.span.unwrap_or(count).clamp(min_span, max_span);
    let mut start = if view.span.is_none() { 0.0 } else { view.start };
    let mut lane_scroll = view.lane_scroll;
    if let Some(pos) = response.hover_pos() {
        let (delta, zoom) = ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta() as f64));
        let over_ruler = pos.y < lanes_area.min.y;
        let mut factor = zoom;
        if over_ruler {
            factor *= (delta.y as f64 * 0.003).exp();
        } else {
            lane_scroll -= delta.y;
        }
        if (factor - 1.0).abs() > 1e-9 {
            let under = Scale { rect: track, start, span }.frame_at(pos.x.max(track.min.x));
            let new_span = (span / factor).clamp(min_span, max_span);
            start = under - (under - start) * new_span / span;
            span = new_span;
        }
        start -= delta.x as f64 * span / track.width() as f64;
    }
    if response.dragged_by(egui::PointerButton::Middle) {
        let d = response.drag_delta();
        start -= d.x as f64 * span / track.width() as f64;
        lane_scroll -= d.y;
    }
    // A box dragged past the top or bottom of the lanes scrolls them.
    let pointer = ui.input(|i| i.pointer.interact_pos());
    if world.resource::<TimelineUi>().marquee.is_some()
        && let Some(p) = pointer
    {
        let over = (p.y - lanes_area.min.y).min(0.0) + (p.y - lanes_area.max.y).max(0.0);
        if over != 0.0 {
            lane_scroll += over.clamp(-60.0, 60.0) * 10.0 * ui.input(|i| i.stable_dt).min(0.1);
            ui.ctx().request_repaint();
        }
    }
    // Follow the playhead while playing: page on when it leaves the view (back, playing backward).
    let head = t.playhead;
    if t.playing && (head < start || head >= start + span) {
        start = if t.reverse { head - span * 0.95 } else { head - span * 0.05 };
    }
    start = start.clamp(-span * 0.02, (count - span * 0.98).max(-span * 0.02));
    let scale = Scale { rect: track, start, span };

    // Ruler, and in the corner above the names the playhead's frame.
    painter.rect_filled(band, 0.0, style::RULER);
    let corner = Rect::from_min_max(rect.min, Pos2::new(track.min.x, band.max.y));
    painter.rect_filled(corner, 0.0, style::RULER.gamma_multiply(0.8));
    painter.text(corner.left_center() + Vec2::new(8.0, 0.0), Align2::LEFT_CENTER, format!("frame {}", t.frame()), FontId::monospace(11.0), style::TEXT);
    let step = tick_step(scale.px_per_frame(), t.fps);
    let mut f = (start / step as f64).ceil() as FrameIndex * step;
    while (f as f64) < start + span && f < t.frame_count {
        let x = scale.x(f as f64);
        painter.line_segment([Pos2::new(x, band.max.y - 8.0), Pos2::new(x, band.max.y)], Stroke::new(1.0, style::TICK));
        painter.text(Pos2::new(x + 3.0, band.min.y + 3.0), Align2::LEFT_TOP, tick_label(f, t.fps, step), FontId::monospace(10.0), style::MUTED);
        f += step;
    }
    // Clip end.
    let end_x = scale.x(count);
    if end_x < track.max.x {
        painter.rect_filled(Rect::from_x_y_ranges(end_x.max(track.min.x)..=track.max.x, track.y_range()), 0.0, style::BG);
    }

    // Decode cache coverage (active rendition).
    if let Some(media) = world.get_resource::<Media>() {
        let which = world.resource::<ActiveSource>().0;
        if let Some(src) = media.source(which) {
            for (a, b) in src.player.cached_ranges() {
                let (ga, gb) = (media.index().grid_of[a] as f64, media.index().grid_of[b] as f64 + 1.0);
                let (x0, x1) = (scale.x(ga).max(strip.min.x), scale.x(gb).min(strip.max.x));
                if x1 > x0 {
                    painter.rect_filled(Rect::from_x_y_ranges(x0..=x1.max(x0 + 1.0), strip.y_range()), 0.0, style::ACCENT.gamma_multiply(0.45));
                }
            }
        }
    }

    // Lanes, and what the pointer does on them.
    // Where the press began. egui forgets it on the release frame, which is when
    // clicks register; a click's release is where it was pressed, near enough.
    let origin = ui.input(|i| i.pointer.press_origin()).or_else(|| response.interact_pointer_pos());
    let mods = ui.input(|i| i.modifiers);
    let mut ui_state = std::mem::take(&mut *world.resource_mut::<TimelineUi>());
    // A stroke that just started shows on its lane: scroll to it once.
    let live = world.resource::<LiveCapture>().0.is_some();
    let stroke_started = live && !ui_state.live;
    ui_state.live = live;
    let hot = ui_state.edge.or(ui_state.hover_edge);
    let hover = response.hover_pos().filter(|p| rows.contains(*p));
    let hits = lanes(&painter, world, &scale, lanes_area, rows, &mut lane_scroll, ui_state.marquee.zip(pointer), stroke_started, hot, &mut ui_state.columns, hover, ui.input(|i| i.time));
    if hits.moving {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
    }
    let fitted = span >= count * 0.999 && start.abs() < 1.0;
    *world.resource_mut::<TimelineView>() =
        if fitted { TimelineView { lane_scroll, ..TimelineView::default() } } else { TimelineView { start, span: Some(span), lane_scroll } };

    ui_state.lanes_area = Some(lanes_area);
    let in_lanes = origin.is_some_and(|o| rows.contains(o));
    ui_state.hover_edge = response.hover_pos().filter(|p| lanes_area.contains(*p)).and_then(|p| hits.edge_at(p));
    if ui_state.hover_edge.is_some() || ui_state.edge.is_some() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
    }
    if in_lanes
        && response.drag_started_by(egui::PointerButton::Primary)
        && let Some(o) = origin
    {
        match hits.edge_at(o) {
            // On the end of a lifetime: drag it (one undo step).
            Some((e, edge)) => {
                ui_state.edge = Some((e, edge));
                tt_core::span::begin_drag(world, e);
            }
            // On a manual dot's name or frames: carry it (and the other selected dots) to a tracker.
            None if let Some(d) = hits.grab_at(o).filter(|e| tt_track::human::is_manual(world, *e)) => {
                let sel = world.resource::<Selection>();
                let dots: Vec<Entity> = if sel.is_selected(d) { sel.entities.iter().copied().filter(|e| tt_track::human::is_manual(world, *e)).collect() } else { vec![d] };
                ui_state.carry = Some(dots);
            }
            // Kept in content coordinates, so it stays on its lane while the lanes scroll.
            None => ui_state.marquee = Some(Pos2::new(o.x, o.y + lane_scroll)),
        }
    }
    if let Some((e, edge)) = ui_state.edge {
        if ui.input(|i| i.pointer.primary_down())
            && let Some(p) = pointer
        {
            // The frame the end lands on, snapped (N; Ctrl inverts) to the playhead and the other objects' ends.
            let snapping = world.resource::<tt_core::commands::TimeSnap>().enabled != mods.ctrl;
            let under = scale.frame_at(p.x).round() as FrameIndex;
            let f = if edge == Edge::First { under } else { under - 1 };
            let f = if snapping {
                let mut points = tt_core::commands::snap_points_except(world, Some(e));
                points.push(t.frame());
                points.sort_unstable();
                let reach = ((8.0 / scale.px_per_frame().max(1e-6)).round() as FrameIndex).max(1);
                tt_core::commands::snap(&points, f, reach).unwrap_or(f)
            } else {
                f
            };
            tt_core::span::move_edge(world, e, edge, f.clamp(0, t.last_frame()));
            let what = if edge == Edge::First { "starts" } else { "ends" };
            response.clone().on_hover_text_at_pointer(format!("{what} at frame {}  ·  {}", f.clamp(0, t.last_frame()), timecode(f.clamp(0, t.last_frame()), t.fps)));
        } else {
            tt_core::span::end_drag(world);
            ui_state.edge = None;
        }
    } else if let Some(dots) = ui_state.carry.clone() {
        // Carrying manual dots: the tracker lane under the pointer takes them on release.
        let down = ui.input(|i| i.pointer.primary_down());
        let target = pointer.and_then(|p| hits.at(p)).filter(|t| tt_track::is_tracker(world, *t) && !dots.contains(t));
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        let name = |e: Entity| world.get::<Name>(e).map_or("the dot".to_string(), |n| n.to_string());
        let what = match dots.as_slice() {
            [one] => name(*one),
            _ => format!("{} dots", dots.len()),
        };
        let tip = match target {
            Some(t) => {
                if let Some((_, lane, _, _)) = hits.lanes.iter().find(|(e, _, _, _)| *e == t) {
                    painter.with_clip_rect(rows).rect(*lane, 0.0, style::HAND.gamma_multiply(0.12), Stroke::new(1.5, style::HAND), egui::StrokeKind::Inside);
                }
                format!("Release: merge {what} into {} (its drawn frames override the tracking there)", name(t))
            }
            None => format!("Drop {what} on a tracker's lane to merge it in"),
        };
        response.clone().on_hover_text_at_pointer(tip);
        // Stopped with the button still down (Esc): nothing merged.
        if response.drag_stopped() || !down {
            if !down && let Some(t) = target {
                tt_track::human::merge_dots(world, &dots, t);
            }
            ui_state.carry = None;
        }
    } else if let Some(m) = hits.marquee {
        painter.with_clip_rect(rows).rect(m, 2.0, style::ACCENT.gamma_multiply(0.12), Stroke::new(1.0, style::ACCENT), egui::StrokeKind::Inside);
        let down = ui.input(|i| i.pointer.primary_down());
        if response.drag_stopped() || !down {
            // Stopped with the button still down (Esc): no selection.
            if !down {
                let hit = hits.in_box(m);
                let mut sel = world.resource_mut::<Selection>();
                if !(mods.shift || mods.ctrl || mods.command) {
                    sel.clear();
                }
                for e in hit {
                    if !sel.is_selected(e) {
                        sel.entities.push(e);
                    }
                }
            }
            ui_state.marquee = None;
        }
    } else if in_lanes
        && let Some(pos) = response.interact_pointer_pos()
    {
        let under = hits.at(pos);
        if response.double_clicked() {
            // Double-click a lane: into the view that follows it.
            if let Some(e) = under.and_then(|e| tt_core::view::followable(world, e)) {
                world.resource_mut::<Selection>().select_only(e);
                actions.push(Action::EnterView);
            }
        } else if response.clicked() {
            let mut sel = world.resource_mut::<Selection>();
            match under {
                Some(e) if mods.ctrl || mods.command => sel.toggle(e),
                Some(e) if mods.shift => {
                    if !sel.is_selected(e) {
                        sel.entities.push(e);
                    }
                }
                Some(e) => sel.select_only(e),
                None => sel.clear(),
            }
        }
    }
    *world.resource_mut::<TimelineUi>() = ui_state;
    // Right-click: select what's under it, then the entity menu.
    if response.secondary_clicked()
        && let Some(e) = response.interact_pointer_pos().filter(|p| rows.contains(*p)).and_then(|p| hits.at(p))
    {
        menu::right_clicked(world, e);
    }
    response.context_menu(|ui| menu::entity_menu(ui, world));

    // Snapping (N, Ctrl inverts) pulls the playhead, and the in and out
    // points, to the edges of timeline objects within 8 points.
    let snapping = world.resource::<tt_core::commands::TimeSnap>().enabled != ui.input(|i| i.modifiers.ctrl);
    let mut points = if snapping { tt_core::commands::snap_points(world) } else { Vec::new() };
    let reach = ((8.0 / scale.px_per_frame().max(1e-6)).round() as FrameIndex).max(1);

    // In and out points: a bracket on the ruler for each one marked, to drag
    // (one undo step); the playhead goes with it, so the viewport shows the
    // frame the export starts or ends on.
    let handles = mark_handles(&scale, marked, t.frame_count);
    let handle_at = |p: Pos2| {
        handles
            .iter()
            .filter(|(_, x)| p.y < lanes_area.min.y && p.x >= track.min.x && (p.x - x).abs() <= EDGE_GRAB + 1.0)
            .min_by(|a, b| (p.x - a.1).abs().total_cmp(&(p.x - b.1).abs()))
            .map(|(end, _)| *end)
    };
    let mut tl = std::mem::take(&mut *world.resource_mut::<TimelineUi>());
    tl.hover_mark = response.hover_pos().and_then(handle_at);
    if tl.mark.is_none()
        && response.drag_started_by(egui::PointerButton::Primary)
        && let Some(end) = origin.and_then(handle_at)
    {
        tl.mark = Some(end);
        tt_core::marks::begin_drag(world, end.name());
    }
    if let Some(end) = tl.mark {
        if ui.input(|i| i.pointer.primary_down())
            && let Some(p) = pointer
        {
            let range = marked.frames(t.frame_count);
            let under = scale.frame_at(p.x).round() as FrameIndex;
            let f = if end == MarkEnd::In { under } else { under - 1 };
            let f = tt_core::commands::snap(&points, f, reach).unwrap_or(f);
            let (f, moved) = match end {
                MarkEnd::In => {
                    let f = f.clamp(0, range.end - 1);
                    (f, Marks { mark_in: Some(f), ..marked })
                }
                MarkEnd::Out => {
                    let f = f.clamp(range.start, t.last_frame());
                    (f, Marks { mark_out: Some(f), ..marked })
                }
            };
            set_marks(world, moved, &format!("Move {} point", end.name()));
            if f != t.frame() {
                actions.push(Action::Seek(f));
            }
            let r = moved.frames(t.frame_count);
            response.clone().on_hover_text_at_pointer(format!("{} point: frame {f}  ·  {}  ·  {} frames from in to out", end.title(), timecode(f, t.fps), r.end - r.start));
        } else {
            tt_core::marks::end_drag(world);
            tl.mark = None;
        }
    }
    let dragging_mark = tl.mark.is_some();
    let hot = tl.mark.or(tl.hover_mark);
    if hot.is_some() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
    }
    *world.resource_mut::<TimelineUi>() = tl;
    // Drawn as they are now (a drag may just have moved one).
    let marked = marks(world);
    if marked.is_set() {
        draw_marks(&painter.with_clip_rect(track), &scale, track, band, marked.frames(t.frame_count), &mark_handles(&scale, marked, t.frame_count), hot);
    }

    // Playhead: a frame-wide band when frames are wide enough to see, else a
    // line; a head on the ruler with the frame's number.
    let shown = t.frame() as f64;
    let (x0, x1) = (scale.x(shown), scale.x(shown + 1.0));
    if track.x_range().contains(x0) {
        if x1 - x0 >= 3.0 {
            painter.rect_filled(Rect::from_x_y_ranges(x0..=x1.min(track.max.x), track.y_range()), 0.0, style::ACCENT.gamma_multiply(0.25));
        }
        painter.line_segment([Pos2::new(x0, band.max.y), Pos2::new(x0, track.max.y)], Stroke::new(1.5, style::ACCENT));
        let label = painter.layout_no_wrap(t.frame().to_string(), FontId::monospace(10.0), style::BG);
        let w = label.size().x + 8.0;
        let head = Rect::from_min_size(Pos2::new((x0 - w / 2.0).clamp(track.min.x, track.max.x - w), band.min.y + 2.0), Vec2::new(w, 13.0));
        painter.rect_filled(head, 3.0, style::ACCENT);
        painter.add(egui::Shape::convex_polygon(vec![Pos2::new(x0 - 4.0, head.max.y), Pos2::new(x0 + 4.0, head.max.y), Pos2::new(x0, band.max.y)], style::ACCENT, Stroke::NONE));
        painter.galley(head.min + Vec2::new(4.0, 1.0), label, style::BG);
    }

    // Scrub with the primary button, from the ruler.
    if snapping {
        for p in &points {
            let x = scale.x(*p as f64 + 0.5);
            if track.x_range().contains(x) {
                let y = lanes_area.min.y;
                painter.add(egui::Shape::convex_polygon(vec![Pos2::new(x - 3.0, y - 5.0), Pos2::new(x + 3.0, y - 5.0), Pos2::new(x, y)], style::ACCENT.gamma_multiply(0.6), Stroke::NONE));
            }
        }
        // The playhead snaps to the in and out points too.
        if marked.is_set() {
            let r = marked.frames(t.frame_count);
            points.extend([r.start, r.end - 1]);
            points.sort_unstable();
            points.dedup();
        }
    }
    let to_frame = |x: f32| {
        let f = (scale.frame_at(x).floor() as FrameIndex).clamp(0, t.last_frame());
        tt_core::commands::snap(&points, f, reach).unwrap_or(f)
    };
    if let Some(pos) = response.hover_pos().filter(|p| p.y < lanes_area.min.y && p.x >= track.min.x && !dragging_mark) {
        let tip = match hot {
            Some(end) => {
                let r = marked.frames(t.frame_count);
                let f = if end == MarkEnd::In { r.start } else { r.end - 1 };
                format!("{} point: frame {f}  ·  {}  ·  drag to move it", end.title(), timecode(f, t.fps))
            }
            None => {
                let f = to_frame(pos.x);
                format!("{f}  ·  {}", timecode(f, t.fps))
            }
        };
        response.clone().on_hover_text_at_pointer(tip);
    }
    let from_ruler = origin.is_some_and(|o| o.y < lanes_area.min.y && o.x >= track.min.x);
    if from_ruler
        && !dragging_mark
        && (response.dragged_by(egui::PointerButton::Primary) || response.clicked())
        && let Some(pos) = response.interact_pointer_pos()
    {
        let f = to_frame(pos.x);
        if f != t.frame() || t.playing {
            actions.push(Action::Seek(f));
        }
    }

    world.resource_mut::<PendingActions>().0.extend(actions);
}

const LANE_H: f32 = 18.0;
/// The column of lane names (points; less on a narrow timeline).
const GUTTER: f32 = 176.0;
/// A lane's end can be grabbed this close (points).
const EDGE_GRAB: f32 = 5.0;

/// What a lane shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaneKind {
    Subject,
    Sketch,
    View,
    Tracker,
    Focus,
}

/// The lanes, top to bottom, in the outliner's order (`crate::tree`: each
/// thing under what it was made relative to), each followed by its view's
/// lane (one level in) when it has one.
pub fn lane_list(world: &mut World) -> Vec<(Entity, usize, LaneKind)> {
    use crate::tree::Node;
    let mut out = Vec::new();
    for (e, depth, node) in crate::tree::tree(world) {
        let kind = match node {
            Node::Subject => LaneKind::Subject,
            Node::Sketch => LaneKind::Sketch,
            Node::Tracker => LaneKind::Tracker,
            Node::Focus => LaneKind::Focus,
        };
        out.push((e, depth, kind));
        if let Some(v) = tt_core::view::view_of(world, e).filter(|v| world.get::<bevy_ecs::entity_disabling::Disabled>(*v).is_none()) {
            out.push((v, depth + 1, LaneKind::View));
        }
    }
    out
}

/// What the lanes put where (screen), for clicks and box selection. Clicks
/// only count inside the lanes area (lanes are clipped to it); a box's far
/// corner stays inside it, but its anchor may have scrolled away.
#[derive(Default)]
struct Hits {
    /// `(entity, its lane (clipped), its label area, its coverage bars)`.
    lanes: Vec<(Entity, Rect, Rect, Vec<Rect>)>,
    /// `(stroke, its tick)`, under selected sketches.
    ticks: Vec<(Entity, Rect)>,
    /// `(entity, which end, its x, its lane)`: the ends of lifetimes, to drag.
    edges: Vec<(Entity, Edge, f32, Rect)>,
    /// The box being dragged, as drawn this frame.
    marquee: Option<Rect>,
    /// A spinner turns: keep repainting.
    moving: bool,
}

impl Hits {
    /// The stroke tick or lane at `p`.
    fn at(&self, p: Pos2) -> Option<Entity> {
        if let Some((e, _)) = self.ticks.iter().find(|(_, r)| r.expand(1.0).contains(p)) {
            return Some(*e);
        }
        self.lanes.iter().find(|(_, lane, _, _)| lane.contains(p)).map(|(e, _, _, _)| *e)
    }

    /// The lane whose name or frames are under `p` (what a drag can pick up).
    fn grab_at(&self, p: Pos2) -> Option<Entity> {
        self.lanes.iter().find(|(_, lane, label, bars)| lane.contains(p) && (label.contains(p) || bars.iter().any(|b| b.expand2(Vec2::new(2.0, 3.0)).contains(p)))).map(|(e, _, _, _)| *e)
    }

    /// The end of a lifetime under `p` (the nearer one if both are close).
    fn edge_at(&self, p: Pos2) -> Option<(Entity, Edge)> {
        self.edges
            .iter()
            .filter(|(_, _, x, lane)| lane.y_range().contains(p.y) && (p.x - x).abs() <= EDGE_GRAB)
            .min_by(|a, b| (p.x - a.2).abs().total_cmp(&(p.x - b.2).abs()))
            .map(|(e, edge, _, _)| (*e, *edge))
    }

    /// Everything a box touches: lanes by their label or coverage, strokes by their ticks.
    fn in_box(&self, m: Rect) -> Vec<Entity> {
        let mut out: Vec<Entity> = self.lanes.iter().filter(|(_, _, label, bars)| label.intersects(m) || bars.iter().any(|b| b.intersects(m))).map(|(e, _, _, _)| *e).collect();
        out.extend(self.ticks.iter().filter(|(_, r)| r.intersects(m)).map(|(e, _)| *e));
        out
    }
}

/// Per pixel column of a tracker's lane: its lowest score and whether any
/// frame there is flagged, over frames inside its span. Cached per lane
/// until the output, the span or the visible range changes.
type Columns = Vec<Option<(f32, bool)>>;

fn tracker_columns(cache: &mut std::collections::HashMap<Entity, (u64, Columns)>, e: Entity, sig: &tt_core::signal::Signal, span: std::ops::Range<FrameIndex>, scale: &Scale) -> Columns {
    let width = scale.rect.width().max(1.0) as usize;
    let key = [sig.version(), span.start as u64, span.end as u64, scale.start.to_bits(), scale.span.to_bits(), width as u64]
        .iter()
        .fold(0xcbf2_9ce4_8422_2325u64, |h, v| (h ^ v).wrapping_mul(0x0100_0000_01b3));
    if let Some((k, c)) = cache.get(&e)
        && *k == key
    {
        return c.clone();
    }
    let mut cols: Columns = vec![None; width];
    let first = (scale.start.floor() as FrameIndex).max(span.start);
    let last = ((scale.start + scale.span).ceil() as FrameIndex).min(span.end);
    for (r, _) in sig.runs(first..last) {
        for f in r {
            let Some(v) = sig.get(f) else { continue };
            // Every column the frame covers (a frame wider than a pixel covers several: the line doesn't break).
            let col = |x: f32| ((x - scale.rect.min.x).max(0.0) as usize).min(width - 1);
            let (a, b) = (col(scale.x(f as f64)), col(scale.x(f as f64 + 1.0) - 0.01));
            let (score, flagged) = (v.get(6).copied().unwrap_or(1.0), tt_track::flags(v) != 0);
            for c in &mut cols[a..=b.max(a)] {
                *c = Some(c.map_or((score, flagged), |(s, fl)| (s.min(score), fl || flagged)));
            }
        }
    }
    cache.insert(e, (key, cols.clone()));
    cols
}

/// The lanes ([`lane_list`]), in rows across the timeline: on the left
/// (`rows` minus `area`) each one's name, its icon and (a tracker) its
/// spinner; on the right (`area`) its frames (valid solid, stale dim, outside
/// its lifetime faint) with its lifetime's ends to drag. Under a selected
/// sketch, a tick per stroke. A tracker's lane: what was drawn by hand along
/// the top (orange), its automatic results (cyan), its looks or reset points
/// (white), its score (a line along the bottom, low is down), its flagged
/// frames (red) and its jobs (what is left to track, and where they are). A
/// subject's lane: its frames and its offset keys (purple). A stroke in
/// progress shows on the lane of the sketch it edits (visited frames solid,
/// frames its falloff moves dim), or on a lane of its own for a new sketch;
/// before one, with the Sketch tool armed, the selected sketch's lane shows
/// how far its falloff would reach from the playhead (dashed). The lanes
/// scroll vertically under the fixed ruler; `scroll` is clamped here, and
/// brought to the live stroke's lane when `stroke_started`. `marquee`: a
/// box's anchor (in content coordinates) and the pointer. `hot`: the end
/// being dragged or under the pointer. `hover`: the pointer over the rows.
#[allow(clippy::too_many_arguments)]
fn lanes(
    painter: &egui::Painter,
    world: &mut World,
    scale: &Scale,
    area: Rect,
    rows: Rect,
    scroll: &mut f32,
    marquee: Option<(Pos2, Pos2)>,
    stroke_started: bool,
    hot: Option<(Entity, Edge)>,
    cache: &mut std::collections::HashMap<Entity, (u64, Columns)>,
    hover: Option<Pos2>,
    time: f64,
) -> Hits {
    use crate::icons::{self, Activity, Glyph};
    let tree = lane_list(world);
    let colors = crate::colors::Colors::new(world);
    let selection = world.resource::<Selection>().clone();
    let store = world.resource::<SignalStore>();
    let live = world.resource::<LiveCapture>().0.as_ref();
    let new_lane = live.is_some_and(|l| l.target.is_none());
    let content_h = (tree.len() + usize::from(new_lane)) as f32 * LANE_H + 6.0;
    if stroke_started && let Some(l) = live {
        let i = l.target.and_then(|t| tree.iter().position(|(e, _, _)| *e == t)).unwrap_or(tree.len());
        let y = i as f32 * LANE_H;
        *scroll = scroll.max(y + LANE_H - rows.height()).min(y);
    }
    *scroll = scroll.clamp(0.0, (content_h - rows.height()).max(0.0));
    let marquee = marquee.map(|(a, b)| Rect::from_two_pos(Pos2::new(a.x, a.y - *scroll), b.clamp(rows.min, rows.max)));
    let painter = painter.with_clip_rect(rows);
    let gutter = Rect::from_min_max(rows.min, Pos2::new(area.min.x, rows.max.y));
    painter.rect_filled(gutter, 0.0, style::BG.gamma_multiply(0.6));
    let visible = (scale.start.floor() as FrameIndex).max(0)..(scale.start + scale.span).ceil() as FrameIndex;
    let mut hits = Hits { marquee, ..Hits::default() };
    let bar = |y0: f32, y1: f32, a: FrameIndex, b: FrameIndex| -> Option<Rect> {
        let (x0, x1) = (scale.x(a as f64).max(scale.rect.min.x), scale.x(b as f64).min(scale.rect.max.x));
        (x1 > x0).then(|| Rect::from_x_y_ranges(x0..=x1.max(x0 + 1.0), y0..=y1))
    };
    // Runs of frames where `has(f)` holds.
    let runs = |first: FrameIndex, len: usize, has: &dyn Fn(usize) -> bool| -> Vec<(FrameIndex, FrameIndex)> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < len {
            if !has(i) {
                i += 1;
                continue;
            }
            let a = i;
            while i < len && has(i) {
                i += 1;
            }
            out.push((first + a as FrameIndex, first + i as FrameIndex));
        }
        out
    };
    let live_bars = |y: f32, live: &tt_core::capture::Live| {
        if let Some((first, values)) = &live.preview {
            for (a, b) in runs(*first, values.len(), &|i| values[i].is_some()) {
                if let Some(r) = bar(y + 2.0, y + 11.0, a, b) {
                    painter.rect_filled(r, 1.5, overlay::LIVE.gamma_multiply(0.35));
                }
            }
        }
        if let Some((first, values)) = &live.boxes {
            for (a, b) in runs(*first, values.len(), &|i| values[i].is_some()) {
                if let Some(r) = bar(y + 2.0, y + 11.0, a, b) {
                    painter.rect_filled(r, 1.5, overlay::LIVE);
                }
            }
        }
    };
    // Bars of a signal's runs over the visible frames, split at a lifetime's edges: (bar, state, inside it).
    let bars_of = |sig: &tt_core::signal::Signal, y0: f32, y1: f32, alive: &std::ops::Range<FrameIndex>| -> Vec<(Rect, FrameState, bool)> {
        let mut out = Vec::new();
        for (r, state) in sig.runs(visible.clone()) {
            let inside = r.start.max(alive.start)..r.end.min(alive.end);
            for (part, alive) in [(r.start..inside.start.min(r.end), false), (inside.clone(), true), (inside.end.max(r.start)..r.end, false)] {
                if !part.is_empty()
                    && let Some(b) = bar(y0, y1, part.start, part.end)
                {
                    out.push((b, state, alive));
                }
            }
        }
        out
    };
    let alpha = |alive: bool, state: FrameState| match (alive, state == FrameState::Valid) {
        (false, _) => 0.10,
        (true, true) => 0.75,
        (true, false) => 0.28,
    };
    // Before a stroke: how far its falloff would reach from the playhead, on the sketch it would edit.
    let falloff_on = {
        let armed = world.resource::<tt_core::tool::ActiveTool>().0 == tt_core::tool::Tool::Sketch && live.is_none();
        let target = selection.primary().filter(|e| tt_core::sketch::is_sketch(world, *e));
        let fo = world.resource::<tt_core::capture::SketchDefaults>().stroke.falloff as f64;
        let t = world.resource::<Transport>();
        target.filter(|_| armed).map(|s| (s, t.frame(), (fo * t.fps.as_f64()).round() as FrameIndex))
    };
    for (i, (e, depth, kind)) in tree.iter().copied().enumerate() {
        let y = rows.min.y + i as f32 * LANE_H - *scroll;
        let lane = Rect::from_min_size(Pos2::new(rows.min.x, y), Vec2::new(rows.width(), LANE_H));
        // Off screen, a lane only matters to a box that swept it before the lanes scrolled.
        if (lane.max.y < rows.min.y || lane.min.y > rows.max.y) && marquee.is_none() {
            continue;
        }
        let selected = selection.is_selected(e);
        // Rows alternate in shade; the one under the pointer lights up.
        if i % 2 == 1 {
            painter.rect_filled(lane, 0.0, egui::Color32::from_white_alpha(5));
        }
        if hover.is_some_and(|p| lane.contains(p)) {
            painter.rect_filled(lane, 0.0, egui::Color32::from_white_alpha(9));
        }
        let glyph = Glyph::of(world, e);
        let own = colors.of(world, e, glyph);
        let method = world.get::<tt_track::Tracker>(e).map(|t| t.method);
        let manual = method == Some(tt_track::Method::Manual);
        // The name column: indent, (a tracker's) spinner, icon, name.
        let mut x = gutter.min.x + 6.0 + depth as f32 * 10.0;
        if kind == LaneKind::Tracker && !manual {
            let a = Activity::of(world, e);
            hits.moving |= a.moving();
            icons::activity(&painter, Pos2::new(x + 5.0, y + LANE_H / 2.0), 4.5, a, time);
            x += 13.0;
        }
        icons::paint_in(&painter, Rect::from_center_size(Pos2::new(x + 6.0, y + LANE_H / 2.0), Vec2::splat(12.0)), glyph, selected, own);
        x += 16.0;
        let name = world.get::<Name>(e).map_or("item".into(), |n| n.to_string());
        let galley = painter.layout_no_wrap(name, FontId::proportional(11.0), egui::Color32::PLACEHOLDER);
        let label = Rect::from_min_max(Pos2::new(x - 1.0, y + 1.0), Pos2::new((x + galley.size().x + 2.0).min(gutter.max.x - 2.0), y + LANE_H - 1.0));
        let span = span_of(world, e);
        let alive = span.range();
        // (bar, state, inside its lifetime), in this lane's colour.
        let mut bars: Vec<(Rect, FrameState, bool)> = Vec::new();
        let color = if selected { own } else { own.gamma_multiply(0.6) };
        match kind {
            LaneKind::Sketch | LaneKind::Subject => {
                if let Some(sig) = world.get::<Output>(e).and_then(|o| store.get(o.0)) {
                    bars = bars_of(sig, y + 2.0, y + 11.0, &alive);
                }
            }
            LaneKind::Focus => {
                if let Some(sig) = world.get::<Output>(e).and_then(|o| store.get(o.0)) {
                    bars = bars_of(sig, y + 6.0, y + 11.0, &alive);
                }
            }
            LaneKind::View => {
                if let Some(sig) = world.get::<Output>(e).and_then(|o| store.get(o.0)) {
                    bars = bars_of(sig, y + 5.0, y + 9.0, &alive);
                }
            }
            LaneKind::Tracker => {
                // Its automatic results (a manual dot has none) …
                if !manual && let Some(sig) = tt_track::human::auto_signal(world, e) {
                    bars = bars_of(sig, y + 6.0, y + 12.0, &alive);
                }
            }
        }
        let previewed = marquee.is_some_and(|m| label.intersects(m) || bars.iter().any(|(b, _, _)| b.intersects(m)));
        if selected || previewed {
            painter.rect_filled(lane, 0.0, style::ACCENT.gamma_multiply(if selected { 0.10 } else { 0.05 }));
        }
        let mut hit_bars: Vec<Rect> = bars.iter().map(|(b, _, _)| *b).collect();
        for (b, state, alive) in &bars {
            painter.rect_filled(*b, 1.5, color.gamma_multiply(alpha(*alive, *state)));
        }
        if kind == LaneKind::Tracker {
            // … what was drawn by hand, a layer over them (a manual dot: all of it).
            if let Some(h) = tt_track::human::human_signal(world, e) {
                let (y0, y1) = if manual { (y + 2.0, y + 12.0) } else { (y + 1.5, y + 5.0) };
                let hand = if selected { style::HAND } else { style::HAND.gamma_multiply(0.7) };
                for (b, state, alive) in bars_of(h, y0, y1, &alive) {
                    painter.rect_filled(b, 1.0, hand.gamma_multiply(alpha(alive, state).max(0.1) / 0.75));
                    hit_bars.push(b);
                }
            }
            if let Some(sig) = world.get::<Output>(e).and_then(|o| store.get(o.0)).filter(|_| !manual) {
                // Flagged frames red, and the score along the bottom (1 at the top of the band).
                let cols = tracker_columns(cache, e, sig, alive.clone(), scale);
                let x0 = scale.rect.min.x;
                let mut line: Vec<Pos2> = Vec::new();
                let flush = |line: &mut Vec<Pos2>| {
                    if line.len() > 1 {
                        painter.add(egui::Shape::line(std::mem::take(line), Stroke::new(1.0, style::TEXT.gamma_multiply(0.5))));
                    }
                    line.clear();
                };
                for (i, c) in cols.iter().enumerate() {
                    let x = x0 + i as f32 + 0.5;
                    match c {
                        Some((score, flagged)) => {
                            if *flagged {
                                painter.rect_filled(Rect::from_x_y_ranges(x - 0.5..=x + 0.5, y + 6.0..=y + 12.0), 0.0, tracks::LOST.gamma_multiply(0.85));
                            }
                            line.push(Pos2::new(x, y + 17.0 - 4.5 * score.clamp(0.0, 1.0)));
                        }
                        None => flush(&mut line),
                    }
                }
                flush(&mut line);
            }
            // Its looks (white squares) or a CoTracker's reset points (white diamonds), on their frames.
            for l in tt_track::look::looks_of(world, e) {
                let Some(look) = world.get::<tt_track::look::Look>(l) else { continue };
                if !visible.contains(&look.frame) {
                    continue;
                }
                let c = Pos2::new(scale.x(look.frame as f64 + 0.5), y + 9.0);
                let lit = selection.is_selected(l);
                let pin = style::PIN.gamma_multiply(if lit || selected { 1.0 } else { 0.6 });
                if method == Some(tt_track::Method::CoTracker) {
                    icons::diamond(&painter, c, if lit { 5.0 } else { 4.0 }, Stroke::new(1.0, egui::Color32::from_black_alpha(160)), Some(pin));
                } else {
                    painter.rect_filled(Rect::from_center_size(c, Vec2::splat(if lit { 7.0 } else { 6.0 })), 1.0, pin);
                    painter.rect_stroke(Rect::from_center_size(c, Vec2::splat(if lit { 7.0 } else { 6.0 })), 1.0, Stroke::new(1.0, egui::Color32::from_black_alpha(160)), egui::StrokeKind::Outside);
                }
            }
            // Its jobs: what is left to track (outlined) and where each is.
            if let Some(st) = world.get::<TrackStatus>(e) {
                for s in [st.forward, st.backward].into_iter().flatten() {
                    let (a, b) = if s.to >= s.at { (s.at + 1, s.to + 1) } else { (s.to, s.at) };
                    let c = if s.waiting { style::MUTED } else if s.phase == tt_track::job::Phase::Loading { style::LIVE } else { tracks::TRACK };
                    if b > a
                        && let Some(r) = bar(y + 6.0, y + 12.0, a, b)
                    {
                        painter.rect_stroke(r, 1.5, Stroke::new(1.0, c.gamma_multiply(0.6)), egui::StrokeKind::Inside);
                    }
                    let x = scale.x(s.at as f64 + 0.5);
                    painter.line_segment([Pos2::new(x, y + 1.0), Pos2::new(x, y + LANE_H - 1.0)], Stroke::new(2.0, c));
                }
            }
        }
        // A SpringFocus's keys (in the colour of what each focuses on) and its moves.
        if kind == LaneKind::Focus
            && let Some(p) = world.get::<tt_core::focus::FocusParams>(e)
        {
            let fps = world.resource::<Transport>().fps.as_f64();
            for (i, k) in p.keys.iter().enumerate() {
                let start = k.frame - (p.lead as f64 * fps).round() as FrameIndex;
                let end = start + (p.move_time as f64 * fps).round() as FrameIndex;
                if i > 0
                    && let Some(r) = bar(y + 3.0, y + 14.0, start, end.max(start + 1))
                {
                    painter.rect_filled(r, 2.0, style::FOCUS.gamma_multiply(0.18));
                }
                if visible.contains(&k.frame) {
                    let c = colors.of(world, k.target, Glyph::of(world, k.target));
                    icons::diamond(&painter, Pos2::new(scale.x(k.frame as f64 + 0.5), y + 8.5), 4.5, Stroke::new(1.0, egui::Color32::from_black_alpha(170)), Some(c));
                }
            }
        }
        // A subject's own offset keys.
        if kind == LaneKind::Subject
            && let Some(sub) = world.get::<tt_core::subject::Subject>(e)
        {
            for k in sub.offsets.iter().filter(|k| visible.contains(&k.frame)) {
                icons::diamond(&painter, Pos2::new(scale.x(k.frame as f64 + 0.5), y + 6.5), 3.5, Stroke::new(1.0, egui::Color32::from_black_alpha(160)), Some(style::SUBJECT));
            }
        }
        // A tick per stroke under a selected sketch (or one whose stroke is selected).
        let strokes = if kind == LaneKind::Sketch { tt_core::commands::strokes_of(world, e) } else { Vec::new() };
        if selected || strokes.iter().any(|c| selection.is_selected(*c)) {
            for c in &strokes {
                if let Some((a, b)) = world.get::<ClockMap>(*c).and_then(|m| m.frame_hull())
                    && let Some(r) = bar(y + 12.5, y + 16.5, a, b + 1)
                {
                    let lit = selection.is_selected(*c) || marquee.is_some_and(|m| r.intersects(m));
                    painter.rect_filled(r, 1.0, if lit { style::TEXT } else { style::TEXT.gamma_multiply(0.4) });
                    hits.ticks.push((*c, r));
                }
            }
        }
        if let Some(l) = live.filter(|l| l.target == Some(e)) {
            live_bars(y, l);
        }
        // How far a stroke at the playhead would pull this sketch's neighbouring frames.
        if let Some((_, at, reach)) = falloff_on.filter(|(s, _, _)| *s == e)
            && let Some(r) = bar(y + 1.0, y + LANE_H - 1.0, at - reach, at + reach + 1)
        {
            let c = style::HAND.gamma_multiply(0.7);
            for (a, b) in [(r.left_top(), r.right_top()), (r.right_top(), r.right_bottom()), (r.right_bottom(), r.left_bottom()), (r.left_bottom(), r.left_top())] {
                painter.add(egui::Shape::dashed_line(&[a, b], Stroke::new(1.0, c), 3.0, 3.0));
            }
        }
        // Its lifetime's ends: brackets where trimmed, grabbable either way.
        if let Some((a, b)) = extent_of(world, e).and_then(|x| span.trim(x)) {
            for (edge, f, trimmed) in [(Edge::First, a, span.first.is_some()), (Edge::Last, b + 1, span.last.is_some())] {
                let x = scale.x(f as f64);
                let lit = hot == Some((e, edge));
                if (trimmed || lit) && x >= area.min.x {
                    let c = if lit { style::TEXT } else { color };
                    let dx = if edge == Edge::First { 3.0 } else { -3.0 };
                    let s = Stroke::new(if lit { 2.0 } else { 1.5 }, c);
                    painter.line_segment([Pos2::new(x, y + 1.0), Pos2::new(x, y + LANE_H - 1.0)], s);
                    painter.line_segment([Pos2::new(x, y + 1.0), Pos2::new(x + dx, y + 1.0)], s);
                    painter.line_segment([Pos2::new(x, y + LANE_H - 1.0), Pos2::new(x + dx, y + LANE_H - 1.0)], s);
                }
                hits.edges.push((e, edge, x, lane.intersect(rows)));
            }
        }
        // The name over the column's shade, clipped to it.
        painter.with_clip_rect(gutter.intersect(lane).shrink2(Vec2::new(2.0, 0.0))).galley(Pos2::new(x, y + (LANE_H - galley.size().y) / 2.0), galley, if selected { style::TEXT } else { style::MUTED });
        hits.lanes.push((e, lane.intersect(rows), label, hit_bars));
    }
    // The column's edge.
    painter.line_segment([Pos2::new(gutter.max.x, rows.min.y), Pos2::new(gutter.max.x, rows.max.y)], Stroke::new(1.0, style::RULER));
    let y = rows.min.y + tree.len() as f32 * LANE_H - *scroll;
    if let Some(l) = live.filter(|l| l.target.is_none()) {
        live_bars(y, l);
        icons::paint(&painter, Rect::from_center_size(Pos2::new(gutter.min.x + 12.0, y + LANE_H / 2.0), Vec2::splat(12.0)), Glyph::Sketch, true);
        painter.text(Pos2::new(gutter.min.x + 24.0, y + LANE_H / 2.0), Align2::LEFT_CENTER, "new sketch (recording)", FontId::proportional(11.0), overlay::LIVE);
    } else if tree.is_empty() {
        painter.text(
            Pos2::new(area.min.x + 8.0, y + 4.0),
            Align2::LEFT_TOP,
            "nothing yet \u{b7} D arms the Sketch tool (press and hold on the video), T the Track tool, M the Draw tool",
            FontId::proportional(11.0),
            style::MUTED,
        );
    }
    // A scrollbar when the lanes don't fit.
    if content_h > rows.height() {
        let h = rows.height() * rows.height() / content_h;
        let top = rows.min.y + (rows.height() - h) * (*scroll / (content_h - rows.height()));
        painter.rect_filled(Rect::from_min_size(Pos2::new(rows.max.x - 5.0, top), Vec2::new(4.0, h)), 2.0, style::TEXT.gamma_multiply(0.35));
    }
    hits
}

/// Timeline pointer state (a box being dragged over the lanes, an end of a
/// lifetime or a mark being dragged) and what the lanes cache.
#[derive(Resource, Debug, Default)]
pub struct TimelineUi {
    /// The box's anchor, in content coordinates (screen y + lane scroll).
    marquee: Option<Pos2>,
    /// The end of a lifetime being dragged, and the one under the pointer.
    edge: Option<(Entity, Edge)>,
    hover_edge: Option<(Entity, Edge)>,
    /// Manual dots being carried to a tracker's lane.
    carry: Option<Vec<Entity>>,
    /// The in or out point being dragged, and the one under the pointer.
    mark: Option<MarkEnd>,
    hover_mark: Option<MarkEnd>,
    /// Tracker lanes' per-column summaries.
    columns: std::collections::HashMap<Entity, (u64, Columns)>,
    /// A stroke was in progress last frame.
    live: bool,
    /// Where the lanes were drawn (the demo aims at it).
    pub lanes_area: Option<Rect>,
}

/// An in or out point (`tt_core::marks`), as a handle on the ruler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MarkEnd {
    In,
    Out,
}

impl MarkEnd {
    fn name(self) -> &'static str {
        if self == MarkEnd::In { "in" } else { "out" }
    }

    fn title(self) -> &'static str {
        if self == MarkEnd::In { "In" } else { "Out" }
    }
}

/// An in or out point is being dragged on the ruler (the viewport shows what an export leaves out meanwhile).
pub fn dragging_mark(world: &World) -> bool {
    world.get_resource::<TimelineUi>().is_some_and(|t| t.mark.is_some())
}

/// The brackets of the marks set: the in point's at the start of its frame, the out point's at the end of its.
fn mark_handles(scale: &Scale, marked: Marks, count: FrameIndex) -> Vec<(MarkEnd, f32)> {
    let r = marked.frames(count);
    [marked.mark_in.map(|_| (MarkEnd::In, scale.x(r.start as f64))), marked.mark_out.map(|_| (MarkEnd::Out, scale.x(r.end as f64)))].into_iter().flatten().collect()
}

/// The in and out points: outside them shaded, between them a bar along the
/// ruler's foot, and a bracket on each one marked (`hot`: under the pointer
/// or being dragged), with a thin line down through the lanes.
fn draw_marks(painter: &egui::Painter, scale: &Scale, rect: Rect, band: Rect, range: Range<FrameIndex>, handles: &[(MarkEnd, f32)], hot: Option<MarkEnd>) {
    let (x0, x1) = (scale.x(range.start as f64).max(rect.min.x), scale.x(range.end as f64).min(rect.max.x));
    let veil = egui::Color32::from_black_alpha(110);
    if x0 > rect.min.x {
        painter.rect_filled(Rect::from_x_y_ranges(rect.min.x..=x0.min(rect.max.x), rect.y_range()), 0.0, veil);
    }
    if x1 < rect.max.x {
        painter.rect_filled(Rect::from_x_y_ranges(x1.max(rect.min.x)..=rect.max.x, rect.y_range()), 0.0, veil);
    }
    if x1 > x0 {
        painter.rect_filled(Rect::from_x_y_ranges(x0..=x1, band.max.y - 4.0..=band.max.y), 0.0, style::RANGE.gamma_multiply(0.8));
    }
    for (end, x) in handles {
        let stroke = Stroke::new(if hot == Some(*end) { 3.0 } else { 2.0 }, style::RANGE);
        let tab = if *end == MarkEnd::In { 6.0 } else { -6.0 };
        let (top, foot) = (band.min.y + 1.5, band.max.y - 1.0);
        painter.line_segment([Pos2::new(*x, top), Pos2::new(*x, foot)], stroke);
        painter.line_segment([Pos2::new(*x, top), Pos2::new(x + tab, top)], stroke);
        painter.line_segment([Pos2::new(*x, foot), Pos2::new(x + tab, foot)], stroke);
        painter.line_segment([Pos2::new(*x, band.max.y), Pos2::new(*x, rect.max.y)], Stroke::new(1.0, style::RANGE.gamma_multiply(0.45)));
    }
}

/// `0.25×`, `1×`, `1.5×`: at most two decimals (auto speed sets rates
/// between the steps); `◀ 2×` for a negative rate (playing backward).
pub(crate) fn rate_label(rate: f64) -> String {
    if rate < 0.0 {
        return format!("◀ {}", rate_label(-rate));
    }
    let s = format!("{rate:.2}");
    format!("{}×", s.trim_end_matches('0').trim_end_matches('.'))
}

/// A "nice" tick spacing in frames with labels at least ~80 px apart.
fn tick_step(px_per_frame: f64, fps: Rational) -> FrameIndex {
    let fps_i = fps.as_f64().round().max(1.0) as FrameIndex;
    let candidates = [1, 2, 5, 10]
        .into_iter()
        .chain([1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 1800, 3600].into_iter().map(|s| s * fps_i));
    for step in candidates {
        if step as f64 * px_per_frame >= 80.0 {
            return step;
        }
    }
    3600 * fps_i
}

fn tick_label(f: FrameIndex, fps: Rational, step: FrameIndex) -> String {
    let fps_i = fps.as_f64().round().max(1.0) as FrameIndex;
    if step < fps_i {
        f.to_string()
    } else {
        let s = fps.frame_to_seconds(f).round() as i64;
        if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Event, Modifiers, PointerButton, RawInput};
    use tt_core::history::{History, undo};
    use tt_core::op::Operator;
    use tt_core::span::{Span, live_span};
    use tt_core::{AppBuilder, CoreModules};

    /// The timeline alone, in a 1000 × 300 pt window: one sketch over frames
    /// 100..=499 of a 600-frame clip (a box signal; no strokes to recompute it).
    fn setup() -> (World, Entity) {
        let mut app = AppBuilder::new();
        app.add_module(CoreModules).add_module(tt_track::TrackModule).add_module(TimelineModule);
        let mut world = app.build().world;
        world.resource_mut::<Transport>().frame_count = 600;
        let sig = world.resource_mut::<SignalStore>().create(6);
        world.resource_mut::<SignalStore>().get_mut(sig).expect("signal").write(100, &[10.0; 6 * 400]);
        let sketch = world.spawn((Name::new("Sketch 1"), Operator { kind: "sketch".into() }, Output(sig))).id();
        (world, sketch)
    }

    fn frame(ctx: &egui::Context, world: &mut World, events: Vec<Event>) {
        let input = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 300.0))), events, ..RawInput::default() };
        ctx.run_ui(input, |ui| super::ui(ui, world)).drop_without_applying_deltas();
    }

    fn button(pos: Pos2, pressed: bool) -> Event {
        Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE }
    }

    /// Dragging a lane's end trims its lifetime, snapping to the playhead,
    /// as one undo step; the data stays.
    #[test]
    fn dragging_a_lanes_end_trims_it_in_one_undo_step() {
        let (mut world, sketch) = setup();
        world.resource_mut::<Transport>().seek(320);
        world.resource_mut::<tt_core::commands::TimeSnap>().enabled = true;
        let ctx = egui::Context::default();
        frame(&ctx, &mut world, Vec::new());
        frame(&ctx, &mut world, Vec::new());
        let lanes = world.resource::<TimelineUi>().lanes_area.expect("lanes drawn");
        // Fitted, the lanes span the clip's 600 frames: the sketch ends at frame 500.
        let x_of = |f: f64| lanes.min.x + (f / 600.0) as f32 * lanes.width();
        let y = lanes.min.y + LANE_H / 2.0;
        let (from, to) = (Pos2::new(x_of(500.0), y), Pos2::new(x_of(322.0), y));
        frame(&ctx, &mut world, vec![Event::PointerMoved(from)]);
        frame(&ctx, &mut world, vec![button(from, true)]);
        for i in 1..=8 {
            let p = from + (to - from) * (i as f32 / 8.0);
            frame(&ctx, &mut world, vec![Event::PointerMoved(p)]);
        }
        frame(&ctx, &mut world, vec![button(to, false)]);
        frame(&ctx, &mut world, Vec::new());
        assert_eq!(world.get::<Span>(sketch).copied(), Some(Span { first: None, last: Some(320) }), "snapped to the playhead");
        assert_eq!(live_span(&world, sketch), Some((100, 320)));
        assert!(world.resource::<Selection>().entities.is_empty(), "a drag on an end is no box selection");
        assert_eq!(world.resource::<History>().undo_label(), Some("Trim Sketch 1"));
        undo(&mut world);
        assert_eq!(world.get::<Span>(sketch), None, "one undo takes the whole drag back");
        assert!(!world.resource::<History>().can_undo());
    }

    /// Dragging a manual dot's frames onto a tracker's lane merges it in:
    /// its drawn frames go to the tracker, the dot goes, one undo step.
    #[test]
    fn dragging_a_manual_dot_onto_a_tracker_merges_it() {
        let (mut world, _) = setup();
        let (out, auto) = {
            let mut store = world.resource_mut::<SignalStore>();
            (store.create(tt_track::TRACK_CHANNELS), store.create(tt_track::TRACK_CHANNELS))
        };
        world.resource_mut::<SignalStore>().get_mut(auto).expect("signal").write(0, &[1.0; tt_track::TRACK_CHANNELS * 600]);
        let tracker = world
            .spawn((
                Name::new("Tracker 1"),
                Operator { kind: "track".into() },
                tt_core::op::Inputs(Vec::new()),
                Output(out),
                tt_track::human::AutoOutput(auto),
                tt_track::Tracker::at(0),
                tt_track::runner::TrackBook::default(),
                tt_track::TrackRun::Paused,
            ))
            .id();
        let mut dot = None;
        tt_core::history::edit(&mut world, "Draw", |tx| {
            let d = tt_track::human::spawn_manual_dot(tx, "Dot 1".into(), 200);
            tt_track::human::write_drawn(tx, d, &(200..260).map(|f| (f, Some([5.0, 5.0]))).collect::<Vec<_>>());
            dot = Some(d);
        });
        let dot = dot.expect("a dot");
        let ctx = egui::Context::default();
        frame(&ctx, &mut world, Vec::new());
        frame(&ctx, &mut world, Vec::new());
        let lanes = world.resource::<TimelineUi>().lanes_area.expect("lanes drawn");
        let order = lane_list(&mut world);
        let row = |e: Entity| order.iter().position(|(l, _, _)| *l == e).expect("a lane") as f32;
        let x = lanes.min.x + (230.0 / 600.0) * lanes.width();
        let (from, to) = (Pos2::new(x, lanes.min.y + row(dot) * LANE_H + LANE_H / 2.0), Pos2::new(x, lanes.min.y + row(tracker) * LANE_H + LANE_H / 2.0));
        frame(&ctx, &mut world, vec![Event::PointerMoved(from)]);
        frame(&ctx, &mut world, vec![button(from, true)]);
        for i in 1..=8 {
            frame(&ctx, &mut world, vec![Event::PointerMoved(from + (to - from) * (i as f32 / 8.0))]);
        }
        frame(&ctx, &mut world, vec![button(to, false)]);
        frame(&ctx, &mut world, Vec::new());
        assert_eq!(tt_track::human::drawn_at(&world, tracker, 230), Some([5.0, 5.0]), "the dot's frames are the tracker's now");
        assert_eq!(tt_track::human::drawn_frames(&world, tracker).0, 60);
        assert!(world.get::<bevy_ecs::entity_disabling::Disabled>(dot).is_some(), "the dot went");
        assert_eq!(world.resource::<Selection>().entities, vec![tracker], "no box selection; the tracker is selected");
        assert_eq!(world.resource::<History>().undo_label(), Some("Merge Dot 1 into Tracker 1"));
        undo(&mut world);
        assert!(world.get::<bevy_ecs::entity_disabling::Disabled>(dot).is_none(), "undo brings the dot back");
        assert_eq!(tt_track::human::drawn_frames(&world, tracker).0, 0);
    }
}
