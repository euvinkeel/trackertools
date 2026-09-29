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
//! - A stroke that starts scrolls its lane into view.
//! - Wheel on the ruler or Ctrl+wheel zooms time; Shift+wheel pans time; the
//!   wheel on the lanes scrolls them; a middle-drag pans both.

use bevy_ecs::prelude::*;
use egui::{Align2, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use tt_core::input::{Action, Keymap, PendingActions};
use tt_core::time::{FrameIndex, Rational, timecode};
use tt_core::transport::{RATES, Transport};
use tt_core::{AppBuilder, Class, Module};

use bevy_ecs::name::Name;
use tt_core::capture::LiveCapture;
use tt_core::op::Output;
use tt_core::selection::Selection;
use tt_core::signal::{FrameState, SignalStore};
use tt_core::sketch::{ClockMap, sketch_of};

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
    let mut actions = Vec::new();
    let mut fit = false;

    ui.horizontal(|ui| {
        let mut button = |ui: &mut egui::Ui, text: &str, action: Action, what: &str| {
            let tip = match keymap.chord_for(action) {
                Some(chord) => format!("{what} ({chord})"),
                None => what.to_string(),
            };
            if ui.button(text).on_hover_text(tip).clicked() {
                actions.push(action);
            }
        };
        button(ui, "⏮", Action::GoToStart, "Go to start");
        button(ui, "◀", Action::StepBackward, "Previous frame");
        button(ui, "◀", Action::ShuttleBackward, "Play backward; again: faster");
        button(ui, if t.playing { "⏸" } else { "▶" }, Action::TogglePlay, "Play / pause (also K)");
        button(ui, "▶▶", Action::ShuttleForward, "Play forward; again: faster");
        button(ui, "▶|", Action::StepForward, "Next frame");
        button(ui, "⏭", Action::GoToEnd, "Go to end");
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
        ui.monospace(format!("{} / {}", t.frame(), t.last_frame()));
        ui.monospace(timecode(t.frame(), t.fps));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            fit = ui.button("Fit").on_hover_text("Show the whole clip").clicked();
            ui.label(
                egui::RichText::new("wheel on the ruler or Ctrl+wheel: zoom · Shift+wheel: pan · wheel on lanes: scroll · middle-drag: pan both · drag lanes: box select · right-click: menu")
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
    let band = Rect::from_min_size(rect.min, Vec2::new(rect.width(), 22.0));
    let strip = Rect::from_min_size(Pos2::new(rect.min.x, band.max.y + 1.0), Vec2::new(rect.width(), 3.0));
    let lanes_area = Rect::from_min_max(Pos2::new(rect.min.x, strip.max.y + 4.0), rect.max);

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
    let min_span = (rect.width() as f64 / 40.0).max(4.0).min(max_span);
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
            let under = Scale { rect, start, span }.frame_at(pos.x);
            let new_span = (span / factor).clamp(min_span, max_span);
            start = under - (under - start) * new_span / span;
            span = new_span;
        }
        start -= delta.x as f64 * span / rect.width() as f64;
    }
    if response.dragged_by(egui::PointerButton::Middle) {
        let d = response.drag_delta();
        start -= d.x as f64 * span / rect.width() as f64;
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
    let scale = Scale { rect, start, span };

    // Ruler.
    painter.rect_filled(band, 0.0, style::RULER);
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
    if end_x < rect.max.x {
        painter.rect_filled(Rect::from_x_y_ranges(end_x..=rect.max.x, rect.y_range()), 0.0, style::BG);
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
    let hits = lanes(&painter, world, &scale, lanes_area, &mut lane_scroll, ui_state.marquee.zip(pointer), stroke_started, hot, &mut ui_state.columns);
    let fitted = span >= count * 0.999 && start.abs() < 1.0;
    *world.resource_mut::<TimelineView>() =
        if fitted { TimelineView { lane_scroll, ..TimelineView::default() } } else { TimelineView { start, span: Some(span), lane_scroll } };

    ui_state.lanes_area = Some(lanes_area);
    let in_lanes = origin.is_some_and(|o| lanes_area.contains(o));
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
    } else if let Some(m) = hits.marquee {
        painter.with_clip_rect(lanes_area).rect(m, 2.0, style::ACCENT.gamma_multiply(0.12), Stroke::new(1.0, style::ACCENT), egui::StrokeKind::Inside);
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
            // Double-click a lane: into that sketch's view.
            if let Some(e) = under.and_then(|e| sketch_of(world, e)) {
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
        && let Some(e) = response.interact_pointer_pos().filter(|p| lanes_area.contains(*p)).and_then(|p| hits.at(p))
    {
        menu::right_clicked(world, e);
    }
    response.context_menu(|ui| menu::entity_menu(ui, world));

    // Playhead: a frame-wide band when frames are wide enough to see, else a line.
    let shown = t.frame() as f64;
    let (x0, x1) = (scale.x(shown), scale.x(shown + 1.0));
    if x1 - x0 >= 3.0 {
        painter.rect_filled(Rect::from_x_y_ranges(x0..=x1, rect.y_range()), 0.0, style::ACCENT.gamma_multiply(0.25));
    }
    painter.line_segment([Pos2::new(x0, rect.min.y), Pos2::new(x0, rect.max.y)], Stroke::new(1.5, style::ACCENT));

    // Scrub with the primary button, from the ruler; snapping (N, Ctrl inverts)
    // pulls the playhead to the edges of timeline objects within 8 points.
    let snapping = world.resource::<tt_core::commands::TimeSnap>().enabled != ui.input(|i| i.modifiers.ctrl);
    let points = if snapping { tt_core::commands::snap_points(world) } else { Vec::new() };
    if snapping {
        for p in &points {
            let x = scale.x(*p as f64 + 0.5);
            if rect.x_range().contains(x) {
                let y = lanes_area.min.y;
                painter.add(egui::Shape::convex_polygon(vec![Pos2::new(x - 3.0, y - 5.0), Pos2::new(x + 3.0, y - 5.0), Pos2::new(x, y)], style::ACCENT.gamma_multiply(0.6), Stroke::NONE));
            }
        }
    }
    let reach = ((8.0 / scale.px_per_frame().max(1e-6)).round() as FrameIndex).max(1);
    let to_frame = |x: f32| {
        let f = (scale.frame_at(x).floor() as FrameIndex).clamp(0, t.last_frame());
        tt_core::commands::snap(&points, f, reach).unwrap_or(f)
    };
    if let Some(pos) = response.hover_pos().filter(|p| p.y < lanes_area.min.y) {
        let f = to_frame(pos.x);
        response.clone().on_hover_text_at_pointer(format!("{f}  ·  {}", timecode(f, t.fps)));
    }
    let from_ruler = origin.is_some_and(|o| o.y < lanes_area.min.y);
    if from_ruler
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
/// A lane's end can be grabbed this close (points).
const EDGE_GRAB: f32 = 5.0;

/// What a lane shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaneKind {
    Sketch,
    View,
    Tracker,
}

/// The lanes, top to bottom: each sketch in the outliner's tree order, then
/// its view and its trackers (indented), then the sketches nested in its
/// view; trackers following something else come last.
pub fn lane_list(world: &mut World) -> Vec<(Entity, usize, LaneKind)> {
    let tree = super::outliner::sketch_tree(world);
    let mut out = Vec::new();
    for (s, depth) in tree {
        out.push((s, depth, LaneKind::Sketch));
        if let Some(v) = tt_core::view::view_of(world, s).filter(|v| world.get::<bevy_ecs::entity_disabling::Disabled>(*v).is_none()) {
            out.push((v, depth + 1, LaneKind::View));
        }
        out.extend(tt_track::trackers_of(world, s).into_iter().map(|t| (t, depth + 1, LaneKind::Tracker)));
    }
    let mut rest: Vec<Entity> = tracks::list(world).into_iter().map(|(e, _)| e).filter(|e| !out.iter().any(|(l, _, _)| l == e)).collect();
    tt_core::meta::creation_order(world, &mut rest);
    out.extend(rest.into_iter().map(|t| (t, 0, LaneKind::Tracker)));
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
}

impl Hits {
    /// The stroke tick or lane at `p`.
    fn at(&self, p: Pos2) -> Option<Entity> {
        if let Some((e, _)) = self.ticks.iter().find(|(_, r)| r.expand(1.0).contains(p)) {
            return Some(*e);
        }
        self.lanes.iter().find(|(_, lane, _, _)| lane.contains(p)).map(|(e, _, _, _)| *e)
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
            let i = ((scale.x(f as f64 + 0.5) - scale.rect.min.x) as usize).min(width - 1);
            let (score, flagged) = (v.get(6).copied().unwrap_or(1.0), tt_track::flags(v) != 0);
            cols[i] = Some(cols[i].map_or((score, flagged), |(s, fl)| (s.min(score), fl || flagged)));
        }
    }
    cache.insert(e, (key, cols.clone()));
    cols
}

/// The lanes ([`lane_list`]): each object's frames (valid solid, stale
/// dim, outside its lifetime faint) with its lifetime's ends to drag; under a
/// selected sketch, a tick per stroke; on a tracker's, its score (a line
/// along the bottom, low is down), its flagged frames (red) and its jobs
/// (what is left to track, and where they are). A stroke in progress shows
/// on the lane of the sketch it edits (visited frames solid, frames its
/// falloff moves dim), or on a lane of its own for a new sketch. The lanes
/// scroll vertically under the fixed ruler; `scroll` is clamped here, and
/// brought to the live stroke's lane when `stroke_started`. `marquee`: a
/// box's anchor (in content coordinates) and the pointer. `hot`: the end
/// being dragged or under the pointer.
#[allow(clippy::too_many_arguments)]
fn lanes(
    painter: &egui::Painter,
    world: &mut World,
    scale: &Scale,
    area: Rect,
    scroll: &mut f32,
    marquee: Option<(Pos2, Pos2)>,
    stroke_started: bool,
    hot: Option<(Entity, Edge)>,
    cache: &mut std::collections::HashMap<Entity, (u64, Columns)>,
) -> Hits {
    let tree = lane_list(world);
    let selection = world.resource::<Selection>().clone();
    let store = world.resource::<SignalStore>();
    let live = world.resource::<LiveCapture>().0.as_ref();
    let new_lane = live.is_some_and(|l| l.target.is_none());
    let content_h = (tree.len() + usize::from(new_lane)) as f32 * LANE_H + 6.0;
    if stroke_started && let Some(l) = live {
        let i = l.target.and_then(|t| tree.iter().position(|(e, _, _)| *e == t)).unwrap_or(tree.len());
        let y = i as f32 * LANE_H;
        *scroll = scroll.max(y + LANE_H - area.height()).min(y);
    }
    *scroll = scroll.clamp(0.0, (content_h - area.height()).max(0.0));
    let marquee = marquee.map(|(a, b)| Rect::from_two_pos(Pos2::new(a.x, a.y - *scroll), b.clamp(area.min, area.max)));
    let painter = painter.with_clip_rect(area);
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
    for (i, (e, depth, kind)) in tree.iter().copied().enumerate() {
        let y = area.min.y + i as f32 * LANE_H - *scroll;
        let lane = Rect::from_min_size(Pos2::new(area.min.x, y), Vec2::new(area.width(), LANE_H));
        // Off screen, a lane only matters to a box that swept it before the lanes scrolled.
        if (lane.max.y < area.min.y || lane.min.y > area.max.y) && marquee.is_none() {
            continue;
        }
        let name = world.get::<Name>(e).map_or("item".into(), |n| n.to_string());
        let name = match kind {
            LaneKind::Sketch => name,
            LaneKind::View => format!("▭ {name}"),
            LaneKind::Tracker => format!("⌖ {name}"),
        };
        let label_pos = Pos2::new(lane.min.x + 6.0 + depth as f32 * 10.0, y + 7.0);
        let galley = painter.layout_no_wrap(name, FontId::proportional(11.0), egui::Color32::PLACEHOLDER);
        let label = Rect::from_min_size(label_pos - Vec2::new(0.0, galley.size().y / 2.0), galley.size()).expand(2.0);
        let span = span_of(world, e);
        let alive = span.range();
        let (y0, y1) = if kind == LaneKind::View { (y + 4.0, y + 9.0) } else { (y + 2.0, y + 11.0) };
        // (bar, state, inside its lifetime)
        let mut bars: Vec<(Rect, FrameState, bool)> = Vec::new();
        if let Some(sig) = world.get::<Output>(e).and_then(|o| store.get(o.0)) {
            for (r, state) in sig.runs(visible.clone()) {
                let inside = r.start.max(alive.start)..r.end.min(alive.end);
                for (part, alive) in [(r.start..inside.start.min(r.end), false), (inside.clone(), true), (inside.end.max(r.start)..r.end, false)] {
                    if !part.is_empty()
                        && let Some(b) = bar(y0, y1, part.start, part.end)
                    {
                        bars.push((b, state, alive));
                    }
                }
            }
        }
        let strokes = if kind == LaneKind::Sketch { tt_core::commands::strokes_of(world, e) } else { Vec::new() };
        let selected = selection.is_selected(e);
        let previewed = marquee.is_some_and(|m| label.intersects(m) || bars.iter().any(|(b, _, _)| b.intersects(m)));
        if selected || previewed {
            painter.rect_filled(lane, 0.0, style::ACCENT.gamma_multiply(if selected { 0.10 } else { 0.05 }));
        }
        let base = if kind == LaneKind::Tracker { tracks::TRACK } else { style::ACCENT };
        let color = if selected || previewed { base } else if kind == LaneKind::Tracker { tracks::TRACK.gamma_multiply(0.6) } else { style::MUTED };
        for (b, state, alive) in &bars {
            let alpha = match (alive, *state == FrameState::Valid) {
                (false, _) => 0.10,
                (true, true) => 0.7,
                (true, false) => 0.25,
            };
            painter.rect_filled(*b, 1.5, color.gamma_multiply(alpha));
        }
        if kind == LaneKind::Tracker
            && let Some(sig) = world.get::<Output>(e).and_then(|o| store.get(o.0))
        {
            // Flagged frames red, and the score along the bottom (1 at the top of the band).
            let cols = tracker_columns(cache, e, sig, alive.clone(), scale);
            let x0 = scale.rect.min.x;
            let mut line: Vec<Pos2> = Vec::new();
            let flush = |line: &mut Vec<Pos2>| {
                if line.len() > 1 {
                    painter.add(egui::Shape::line(std::mem::take(line), Stroke::new(1.0, style::TEXT.gamma_multiply(0.55))));
                }
                line.clear();
            };
            for (i, c) in cols.iter().enumerate() {
                let x = x0 + i as f32 + 0.5;
                match c {
                    Some((score, flagged)) => {
                        if *flagged {
                            painter.rect_filled(Rect::from_x_y_ranges(x - 0.5..=x + 0.5, y0..=y1), 0.0, tracks::LOST.gamma_multiply(0.85));
                        }
                        line.push(Pos2::new(x, y + 16.5 - 4.5 * score.clamp(0.0, 1.0)));
                    }
                    None => flush(&mut line),
                }
            }
            flush(&mut line);
            // Its jobs: what is left to track (outlined) and where each is.
            if let Some(st) = world.get::<TrackStatus>(e) {
                for s in [st.forward, st.backward].into_iter().flatten() {
                    let (a, b) = if s.to >= s.at { (s.at + 1, s.to + 1) } else { (s.to, s.at) };
                    let c = if s.waiting { style::MUTED } else { tracks::TRACK };
                    if b > a
                        && let Some(r) = bar(y0, y1, a, b)
                    {
                        painter.rect_stroke(r, 1.5, Stroke::new(1.0, c.gamma_multiply(0.6)), egui::StrokeKind::Inside);
                    }
                    let x = scale.x(s.at as f64 + 0.5);
                    painter.line_segment([Pos2::new(x, y + 1.0), Pos2::new(x, y + LANE_H - 1.0)], Stroke::new(2.0, c));
                }
            }
        }
        // A tick per stroke under a selected sketch (or one whose stroke is selected).
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
        // Its lifetime's ends: brackets where trimmed, grabbable either way.
        if let Some((a, b)) = extent_of(world, e).and_then(|x| span.trim(x)) {
            for (edge, f, trimmed) in [(Edge::First, a, span.first.is_some()), (Edge::Last, b + 1, span.last.is_some())] {
                let x = scale.x(f as f64);
                let lit = hot == Some((e, edge));
                if trimmed || lit {
                    let c = if lit { style::TEXT } else { color };
                    let dx = if edge == Edge::First { 3.0 } else { -3.0 };
                    let s = Stroke::new(if lit { 2.0 } else { 1.5 }, c);
                    painter.line_segment([Pos2::new(x, y + 1.0), Pos2::new(x, y + LANE_H - 1.0)], s);
                    painter.line_segment([Pos2::new(x, y + 1.0), Pos2::new(x + dx, y + 1.0)], s);
                    painter.line_segment([Pos2::new(x, y + LANE_H - 1.0), Pos2::new(x + dx, y + LANE_H - 1.0)], s);
                }
                hits.edges.push((e, edge, x, lane.intersect(area)));
            }
        }
        painter.galley(label.min + Vec2::splat(2.0), galley, if selected { style::TEXT } else { style::MUTED });
        hits.lanes.push((e, lane.intersect(area), label, bars.into_iter().map(|(b, _, _)| b).collect()));
    }
    let y = area.min.y + tree.len() as f32 * LANE_H - *scroll;
    if let Some(l) = live.filter(|l| l.target.is_none()) {
        live_bars(y, l);
        painter.text(Pos2::new(area.min.x + 6.0, y + 7.0), Align2::LEFT_CENTER, "⏺ new sketch", FontId::proportional(11.0), style::TEXT);
    } else if tree.is_empty() {
        painter.text(Pos2::new(area.min.x + 8.0, y + 4.0), Align2::LEFT_TOP, "no sketches yet · D arms the Sketch tool, then press and hold on the video", FontId::proportional(11.0), style::MUTED);
    }
    // A scrollbar when the lanes don't fit.
    if content_h > area.height() {
        let h = area.height() * area.height() / content_h;
        let top = area.min.y + (area.height() - h) * (*scroll / (content_h - area.height()));
        painter.rect_filled(Rect::from_min_size(Pos2::new(area.max.x - 5.0, top), Vec2::new(4.0, h)), 2.0, style::TEXT.gamma_multiply(0.35));
    }
    hits
}

/// Timeline pointer state (a box being dragged over the lanes, an end of a
/// lifetime being dragged) and what the lanes cache.
#[derive(Resource, Debug, Default)]
pub struct TimelineUi {
    /// The box's anchor, in content coordinates (screen y + lane scroll).
    marquee: Option<Pos2>,
    /// The end of a lifetime being dragged, and the one under the pointer.
    edge: Option<(Entity, Edge)>,
    hover_edge: Option<(Entity, Edge)>,
    /// Tracker lanes' per-column summaries.
    columns: std::collections::HashMap<Entity, (u64, Columns)>,
    /// A stroke was in progress last frame.
    live: bool,
    /// Where the lanes were drawn (the demo aims at it).
    pub lanes_area: Option<Rect>,
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
}
