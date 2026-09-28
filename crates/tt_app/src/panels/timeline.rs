//! Timeline: transport controls and a zoomable, scrubbable ruler. The visible
//! range is session state in the world. A thin strip under the ruler shows
//! which frames the decode service holds in its cache; below it, one lane per
//! sketch shows the frames it covers and, for the selected sketch, its
//! strokes (click a lane to select the sketch).

use bevy_ecs::prelude::*;
use egui::{Align2, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use tt_core::input::{Action, Keymap, PendingActions};
use tt_core::time::{FrameIndex, Rational, timecode};
use tt_core::transport::{RATES, Transport};
use tt_core::{AppBuilder, Class, Module};

use bevy_ecs::name::Name;
use tt_core::capture::LiveCapture;
use tt_core::op::{Inputs, Operator, Output};
use tt_core::selection::Selection;
use tt_core::signal::{FrameState, SignalId, SignalStore};
use tt_core::sketch::{ClockMap, sketch_of};

use super::overlay;
use crate::media::{ActiveSource, Media};
use crate::style;

/// Visible range of the timeline: `span` frames starting at `start`.
/// `None` span = fit the whole clip.
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct TimelineView {
    pub start: f64,
    pub span: Option<f64>,
}

pub struct TimelineModule;

impl Module for TimelineModule {
    fn build(&self, app: &mut AppBuilder) {
        app.declare::<TimelineView>(Class::Session).init_resource::<TimelineView>();
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
        button(ui, if t.playing { "⏸" } else { "▶" }, Action::TogglePlay, "Play / pause");
        button(ui, "▶|", Action::StepForward, "Next frame");
        button(ui, "⏭", Action::GoToEnd, "Go to end");
        ui.separator();

        egui::ComboBox::from_id_salt("rate").width(64.0).selected_text(rate_label(t.rate)).show_ui(ui, |ui| {
            for (i, r) in RATES.iter().enumerate() {
                if ui.selectable_label((r - t.rate).abs() < 1e-9, rate_label(*r)).clicked() {
                    actions.push(Action::SetRate(i as u8));
                }
            }
        });
        if ui.selectable_label(t.looping, "⟲ loop").clicked() {
            actions.push(Action::ToggleLoop);
        }
        ui.separator();
        ui.monospace(format!("{} / {}", t.frame(), t.last_frame()));
        ui.monospace(timecode(t.frame(), t.fps));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            fit = ui.button("Fit").on_hover_text("Show the whole clip").clicked();
            ui.label(egui::RichText::new("wheel: zoom · shift+wheel / middle-drag: pan").weak().small());
        });
    });

    let height = ui.available_height().max(44.0);
    let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, style::PANEL);
    if !t.has_media() {
        world.resource_mut::<PendingActions>().0.extend(actions);
        return;
    }

    // Visible range (session state), with zoom/pan/follow applied.
    let count = t.frame_count as f64;
    let mut view = *world.resource::<TimelineView>();
    if fit {
        view = TimelineView::default();
    }
    let min_span = (rect.width() as f64 / 40.0).max(4.0); // at most 40 px per frame
    let mut span = view.span.unwrap_or(count).clamp(min_span, count * 1.05);
    let mut start = if view.span.is_none() { 0.0 } else { view.start };
    if let Some(pos) = response.hover_pos() {
        let (delta, shift) = ui.input(|i| (i.smooth_scroll_delta, i.modifiers.shift));
        let pan_px = delta.x + if shift { delta.y } else { 0.0 };
        if !shift && delta.y != 0.0 {
            let under = Scale { rect, start, span }.frame_at(pos.x);
            let new_span = (span * (-delta.y as f64 * 0.003).exp()).clamp(min_span, count * 1.05);
            start = under - (under - start) * new_span / span;
            span = new_span;
        }
        start -= pan_px as f64 * span / rect.width() as f64;
    }
    if response.dragged_by(egui::PointerButton::Middle) {
        start -= response.drag_delta().x as f64 * span / rect.width() as f64;
    }
    // Follow the playhead while playing: page forward when it leaves the view.
    let head = t.playhead;
    if t.playing && (head < start || head >= start + span) {
        start = head - span * 0.05;
    }
    start = start.clamp(-span * 0.02, (count - span * 0.98).max(-span * 0.02));
    let fitted = span >= count * 0.999 && start.abs() < 1.0;
    *world.resource_mut::<TimelineView>() = if fitted { TimelineView::default() } else { TimelineView { start, span: Some(span) } };
    let scale = Scale { rect, start, span };

    // Ruler.
    let band = Rect::from_min_size(rect.min, Vec2::new(rect.width(), 22.0));
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
    let strip = Rect::from_min_size(Pos2::new(rect.min.x, band.max.y + 1.0), Vec2::new(rect.width(), 3.0));
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
    let clicked_lane = lanes(&painter, world, &scale, Pos2::new(rect.min.x, strip.max.y + 4.0), response.clicked().then(|| response.interact_pointer_pos()).flatten());
    if let Some(e) = clicked_lane {
        world.resource_mut::<Selection>().select_only(e);
    }

    // Playhead: a frame-wide band when frames are wide enough to see, else a line.
    let shown = t.frame() as f64;
    let (x0, x1) = (scale.x(shown), scale.x(shown + 1.0));
    if x1 - x0 >= 3.0 {
        painter.rect_filled(Rect::from_x_y_ranges(x0..=x1, rect.y_range()), 0.0, style::ACCENT.gamma_multiply(0.25));
    }
    painter.line_segment([Pos2::new(x0, rect.min.y), Pos2::new(x0, rect.max.y)], Stroke::new(1.5, style::ACCENT));

    // Scrub with the primary button.
    let to_frame = |x: f32| (scale.frame_at(x).floor() as FrameIndex).clamp(0, t.last_frame());
    if let Some(pos) = response.hover_pos() {
        let f = to_frame(pos.x);
        response.clone().on_hover_text_at_pointer(format!("{f}  ·  {}", timecode(f, t.fps)));
    }
    if (response.dragged_by(egui::PointerButton::Primary) || response.clicked())
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

/// One lane per sketch: its path's coverage (valid solid, stale dim) and, for
/// the selected sketch, a tick row with each stroke's span. A stroke in
/// progress shows on the lane of the sketch it edits (visited frames solid,
/// frames its falloff moves dim), or on a lane of its own for a new sketch.
/// Returns the sketch whose lane was clicked.
fn lanes(painter: &egui::Painter, world: &mut World, scale: &Scale, top: Pos2, click: Option<Pos2>) -> Option<Entity> {
    let selected = world.resource::<Selection>().primary().and_then(|e| sketch_of(world, e));
    let mut q = world.query::<(Entity, &Operator, &Output, Option<&Name>, Option<&Inputs>)>();
    let mut rows: Vec<(Entity, String, SignalId, Vec<Entity>)> = q
        .iter(world)
        .filter(|(_, o, _, _, _)| o.kind == "sketch")
        .map(|(e, _, out, n, i)| (e, n.map_or("sketch".into(), |n| n.to_string()), out.0, i.map(|i| i.0.iter().map(|x| x.1).collect()).unwrap_or_default()))
        .collect();
    rows.sort_by_key(|r| r.0);
    let visible = (scale.start.floor() as FrameIndex).max(0)..(scale.start + scale.span).ceil() as FrameIndex;
    let store = world.resource::<SignalStore>();
    let live = world.resource::<LiveCapture>().0.as_ref();
    let mut clicked = None;
    let mut y = top.y;
    let bar = |y0: f32, y1: f32, a: FrameIndex, b: FrameIndex, color: egui::Color32| {
        let (x0, x1) = (scale.x(a as f64).max(scale.rect.min.x), scale.x(b as f64).min(scale.rect.max.x));
        if x1 > x0 {
            painter.rect_filled(Rect::from_x_y_ranges(x0..=x1.max(x0 + 1.0), y0..=y1), 1.5, color);
        }
    };
    // Runs of frames where `has(f)` holds, within the visible range.
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
                bar(y + 3.0, y + 12.0, a, b, overlay::LIVE.gamma_multiply(0.35));
            }
        }
        if let Some((first, values)) = &live.boxes {
            for (a, b) in runs(*first, values.len(), &|i| values[i].is_some()) {
                bar(y + 3.0, y + 12.0, a, b, overlay::LIVE);
            }
        }
    };
    for (e, name, signal, strokes) in &rows {
        let is_selected = selected == Some(*e);
        let lane = Rect::from_min_size(Pos2::new(top.x, y), Vec2::new(scale.rect.width(), LANE_H));
        if is_selected {
            painter.rect_filled(lane, 0.0, style::ACCENT.gamma_multiply(0.08));
        }
        let color = if is_selected { style::ACCENT } else { style::MUTED };
        if let Some(sig) = store.get(*signal) {
            for (r, state) in sig.runs(visible.clone()) {
                match state {
                    FrameState::Valid => bar(y + 3.0, y + 12.0, r.start, r.end, color.gamma_multiply(0.7)),
                    FrameState::Stale => bar(y + 3.0, y + 12.0, r.start, r.end, color.gamma_multiply(0.25)),
                    FrameState::Absent => {}
                }
            }
        }
        if is_selected {
            // Each stroke's span, as ticks under the path.
            for s in strokes {
                if let Some((a, b)) = world.get::<ClockMap>(*s).and_then(|c| c.frame_hull()) {
                    bar(y + 13.5, y + 15.5, a, b + 1, style::TEXT.gamma_multiply(0.5));
                }
            }
        }
        if let Some(l) = live.filter(|l| l.target == Some(*e)) {
            live_bars(y, l);
        }
        painter.text(Pos2::new(lane.min.x + 6.0, y + 7.5), Align2::LEFT_CENTER, name, FontId::proportional(11.0), if is_selected { style::TEXT } else { style::MUTED });
        if click.is_some_and(|p| lane.contains(p)) {
            clicked = Some(*e);
        }
        y += LANE_H;
    }
    if let Some(l) = live.filter(|l| l.target.is_none()) {
        live_bars(y, l);
        painter.text(Pos2::new(top.x + 6.0, y + 7.5), Align2::LEFT_CENTER, "⏺ new sketch", FontId::proportional(11.0), style::TEXT);
    } else if rows.is_empty() {
        painter.text(Pos2::new(top.x + 8.0, y + 6.0), Align2::LEFT_TOP, "no sketches yet · D arms the Sketch tool, then press and hold on the video", FontId::proportional(11.0), style::MUTED);
    }
    clicked
}

fn rate_label(rate: f64) -> String {
    if rate >= 1.0 { format!("{rate:.0}×") } else { format!("{rate}×") }
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
