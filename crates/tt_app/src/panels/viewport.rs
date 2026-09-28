//! Viewport: the video frame at the playhead, with zoom/pan (session state in
//! the world). The frame is drawn by the GPU through a paint callback
//! (video.rs); while the exact frame is still decoding, the nearest cached
//! frame is shown and labelled, so the picture never blanks.
//!
//! It shows the source or a derived view ([`ActiveView`]): the panel lays out
//! the view's *canvas* (its pixel grid) and the shader samples the source
//! through the view's per-frame mapping, from the original whenever the proxy
//! would be magnified. A breadcrumb (`Source ▸ Sketch 1 ▸ …`) walks the chain.

use bevy_ecs::name::Name;
use bevy_ecs::prelude::*;
use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Vec2};
use tt_core::transport::Transport;
use tt_core::view::{ActiveView, SpaceMap, chain, map_at, sketch_framed};
use tt_core::{AppBuilder, Class, Module, Set};
use tt_core::input::{Action, PendingActions};

use crate::media::{ActiveSource, Media, Which};
use crate::style;
use crate::video::{LAST_UPLOAD_US, VideoPaint};

/// Zoom/pan of the viewport. Session state (not undoable).
#[derive(Resource, Debug, Clone, Copy)]
pub struct ViewportView {
    /// 1.0 = the whole canvas fits the panel.
    pub zoom: f32,
    /// Canvas point (0..1 each axis) at the panel centre.
    pub center: [f32; 2],
    /// The view this zoom/pan belongs to (a different view starts fitted).
    pub for_view: Option<Entity>,
}

impl Default for ViewportView {
    fn default() -> Self {
        Self { zoom: 1.0, center: [0.5, 0.5], for_view: None }
    }
}

/// Where the viewport and the shown canvas (the source's pixels, or a view's)
/// were drawn in the last UI pass (egui points), so the next frame's pointer
/// samples map into canvas pixels. The core maps canvas pixels to the source.
#[derive(Resource, Debug, Clone, Copy)]
pub struct ViewportMapping {
    pub panel: Rect,
    pub video: Rect,
    /// Canvas size (pixels).
    pub canvas: Vec2,
}

impl Default for ViewportMapping {
    fn default() -> Self {
        Self { panel: Rect::NOTHING, video: Rect::from_min_size(Pos2::ZERO, Vec2::splat(1.0)), canvas: Vec2::splat(1.0) }
    }
}

impl ViewportMapping {
    fn scale(self) -> f64 {
        self.video.width() as f64 / self.canvas.x as f64
    }

    /// Screen points per canvas pixel.
    pub fn points_per_canvas(self) -> f64 {
        self.scale()
    }

    pub fn to_canvas(self, p: Pos2) -> [f64; 2] {
        let s = self.scale();
        [(p.x - self.video.min.x) as f64 / s, (p.y - self.video.min.y) as f64 / s]
    }

    /// `[t, x, y]` for a timestamped sample.
    pub fn to_canvas_at(self, t: f64, p: Pos2) -> [f64; 3] {
        let [x, y] = self.to_canvas(p);
        [t, x, y]
    }

    pub fn to_screen(self, c: [f64; 2]) -> Pos2 {
        let s = self.scale();
        Pos2::new(self.video.min.x + (c[0] * s) as f32, self.video.min.y + (c[1] * s) as f32)
    }
}

/// Zoom/pan per view (None = the source), restored when a view is shown again.
#[derive(Resource, Debug, Default)]
pub struct ViewMemory(pub std::collections::HashMap<Option<Entity>, (f32, [f32; 2])>);

/// A change of framing at the frame being looked at (an edit to the view's
/// own sketch, a re-tune) eases in instead of snapping.
#[derive(Resource, Debug, Default)]
pub struct FramingEase {
    at: Option<(Option<Entity>, tt_core::time::FrameIndex)>,
    target: Option<SpaceMap>,
    shown: Option<SpaceMap>,
    from: Option<(SpaceMap, f64)>,
}

const EASE_SECONDS: f64 = 0.25;

fn ease_framing(world: &mut World, view: Option<Entity>, frame: tt_core::time::FrameIndex, target: SpaceMap) -> SpaceMap {
    let now = world.resource::<tt_core::time::WallClock>().now;
    let mut e = world.resource_mut::<FramingEase>();
    if e.at != Some((view, frame)) {
        // Another view or frame: show it as it is.
        *e = FramingEase { at: Some((view, frame)), target: Some(target), shown: Some(target), from: None };
        return target;
    }
    if e.target != Some(target) {
        let from = e.shown.unwrap_or(target);
        e.from = Some((from, now));
        e.target = Some(target);
    }
    let shown = match e.from {
        Some((from, t0)) if now - t0 < EASE_SECONDS => {
            let u = ((now - t0) / EASE_SECONDS).clamp(0.0, 1.0);
            let u = u * u * (3.0 - 2.0 * u);
            let lerp = |a: f64, b: f64| a + (b - a) * u;
            SpaceMap { a: (lerp(from.a.ln(), target.a.ln())).exp(), b: [lerp(from.b[0], target.b[0]), lerp(from.b[1], target.b[1])], canvas: target.canvas }
        }
        _ => target,
    };
    e.shown = Some(shown);
    shown
}

/// Until when (wall seconds) the wheel is not the viewport's to zoom with.
#[derive(Resource, Debug, Default)]
pub struct WheelLock(pub f64);

/// Set while the viewport shows a stand-in for a frame still decoding, so the
/// shell keeps repainting until the exact frame arrives.
#[derive(Resource, Default, Debug)]
pub struct WaitingForFrame(pub bool);

/// Playback diagnostics for spike S2 (derived; never saved).
#[derive(Resource, Default, Debug)]
pub struct PlaybackProbe {
    /// UI frames drawn while playing, and how many of them lacked the exact frame.
    pub frames: u64,
    pub late: u64,
    /// Distinct video frames shown while playing, and the wall time spent playing.
    pub distinct: u64,
    pub seconds: f64,
    last_shown: Option<i64>,
    started: Option<f64>,
    was_playing: bool,
}

fn frame_all(mut actions: ResMut<PendingActions>, mut view: ResMut<ViewportView>) {
    if !actions.take(|a| a == Action::FrameAll).is_empty() {
        *view = ViewportView::default();
    }
}

pub struct ViewportModule;

impl Module for ViewportModule {
    fn build(&self, app: &mut AppBuilder) {
        app.declare::<ViewportView>(Class::Session)
            .declare::<PlaybackProbe>(Class::Derived)
            .declare::<WaitingForFrame>(Class::Derived)
            .declare::<ViewportMapping>(Class::Derived)
            .declare::<WheelLock>(Class::Derived)
            .declare::<ViewMemory>(Class::Session)
            .declare::<SpeedFlash>(Class::Derived)
            .init_resource::<SpeedFlash>()
            .declare::<FramingEase>(Class::Derived)
            .init_resource::<ViewMemory>()
            .init_resource::<FramingEase>()
            .init_resource::<WheelLock>()
            .init_resource::<ViewportMapping>()
            .init_resource::<ViewportView>()
            .init_resource::<PlaybackProbe>()
            .init_resource::<WaitingForFrame>()
            .add_systems(frame_all.in_set(Set::Intents));
    }
}

pub fn ui(ui: &mut egui::Ui, world: &mut World) {
    let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    if world.get_resource::<Media>().is_none() {
        world.resource_mut::<WaitingForFrame>().0 = false;
        painter.rect_filled(rect, 0.0, style::BG);
        painter.text(rect.center(), Align2::CENTER_CENTER, "Open a video — Ctrl+O, or drop a file here", FontId::proportional(16.0), style::MUTED);
        return;
    }

    let t = world.resource::<Transport>().clone();
    let active_view = world.resource::<ActiveView>().0;
    let mut view = *world.resource::<ViewportView>();
    if view.for_view != active_view {
        // Each view keeps its own zoom/pan: stepping back restores it.
        let mut memory = world.resource_mut::<ViewMemory>();
        memory.0.insert(view.for_view, (view.zoom, view.center));
        let (zoom, center) = memory.0.get(&active_view).copied().unwrap_or((1.0, [0.5, 0.5]));
        view = ViewportView { zoom, center, for_view: active_view };
    }
    let (vw, vh) = {
        let m = world.resource::<Media>();
        (m.index().width as f32, m.index().height as f32)
    };
    // The canvas: the source's pixels, or the shown view's.
    let space = map_at(world, active_view, t.frame());
    let (cw, ch) = (space.canvas[0] as f32, space.canvas[1] as f32);

    // Zoom about the cursor, pan with the secondary/middle button.
    let fit = (rect.width() / cw).min(rect.height() / ch);
    let video_rect = |view: &ViewportView| {
        let size = Vec2::new(cw, ch) * fit * view.zoom;
        let min = rect.center() - Vec2::new(view.center[0] * size.x, view.center[1] * size.y);
        Rect::from_min_size(min, size)
    };
    // While a stroke uses the wheel (falloff), and briefly after, the wheel doesn't
    // zoom: egui spreads one notch over several frames of smoothed scrolling.
    let now = world.resource::<tt_core::time::WallClock>().now;
    if world.resource::<tt_core::tool::PointerFrame>().wheel_taken {
        world.resource_mut::<WheelLock>().0 = now + 0.4;
    }
    let wheel_free = now >= world.resource::<WheelLock>().0;
    if let Some(pos) = response.hover_pos().filter(|_| wheel_free) {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll != 0.0 {
            let before = video_rect(&view);
            let uv = (pos - before.min) / before.size();
            view.zoom = (view.zoom * (scroll * 0.0025).exp()).clamp(0.1, 64.0);
            let size = Vec2::new(cw, ch) * fit * view.zoom;
            let min = pos - Vec2::new(uv.x * size.x, uv.y * size.y);
            view.center = [(rect.center().x - min.x) / size.x, (rect.center().y - min.y) / size.y];
        }
    }
    if response.dragged_by(egui::PointerButton::Secondary) || response.dragged_by(egui::PointerButton::Middle) {
        let size = video_rect(&view).size();
        let d = response.drag_delta();
        view.center = [view.center[0] - d.x / size.x, view.center[1] - d.y / size.y];
    }
    *world.resource_mut::<ViewportView>() = view;

    let vr = video_rect(&view);
    let ppc = vr.width() / cw; // screen points per canvas pixel
    let eased = ease_framing(world, active_view, t.frame(), space);
    let screen_per_source = ppc as f64 / space.a;
    let mapping = ViewportMapping { panel: rect, video: vr, canvas: Vec2::new(cw, ch) };
    *world.resource_mut::<ViewportMapping>() = mapping;
    let shown_grid = t.frame();

    // Show the proxy unless it would be magnified: past its resolution, the original.
    let media = world.resource::<Media>();
    let prefer = match media.proxy() {
        Some(p) if vw as f64 * screen_per_source <= p.index.width as f64 => Which::Proxy,
        _ => Which::Original,
    };
    let other = if prefer == Which::Proxy { Which::Original } else { Which::Proxy };
    let wanted = media.presented(shown_grid);
    // Exact frame from the preferred source, then from the other one, then the
    // nearest cached frame — the picture never blanks while decoding.
    let exact_from = |w: Which| media.source(w).and_then(|s| s.player.frame(wanted)).map(|d| (w, wanted, d));
    let nearest_from = |w: Which| media.source(w).and_then(|s| s.player.frame_or_nearest(wanted)).map(|(p, d)| (w, p, d));
    let shown = exact_from(prefer).or_else(|| exact_from(other)).or_else(|| nearest_from(prefer)).or_else(|| nearest_from(other));
    let exact = shown.as_ref().is_some_and(|(_, p, _)| *p == wanted);
    let bg = style::BG;
    let background = [bg.r() as f32 / 255.0, bg.g() as f32 / 255.0, bg.b() as f32 / 255.0, 1.0];

    let mut shown_source = None;
    if let Some((which, p, data)) = shown.clone() {
        let src = media.source(which).expect("shown source exists");
        shown_source = Some(which);
        // Panel → canvas → source → texture coordinates, through the view as it
        // framed the frame actually shown (a stand-in keeps its own framing).
        let g = media.index().grid_of[p] as tt_core::time::FrameIndex;
        let m = if g == t.frame() { eased } else { map_at(world, active_view, g) };
        let (a, b) = (m.a as f32, [m.b[0] as f32, m.b[1] as f32]);
        let callback = VideoPaint {
            key: (media.generation * 2 + u64::from(which == Which::Proxy), p),
            data,
            width: src.index.width,
            height: src.index.height,
            scale: [a * rect.width() / (ppc * vw), a * rect.height() / (ppc * vh)],
            offset: [(b[0] + a * (rect.min.x - vr.min.x) / ppc) / vw, (b[1] + a * (rect.min.y - vr.min.y) / ppc) / vh],
            background,
            nearest: (ppc / a) * (vw / src.index.width as f32) >= 3.0,
            color: media.color,
        };
        painter.add(eframe::egui_wgpu::Callback::new_paint_callback(rect, callback));
    } else {
        painter.rect_filled(rect, 0.0, style::BG);
    }

    // HUD
    let state = match &shown {
        _ if exact => format!("frame {shown_grid}"),
        Some((_, p, _)) => format!("frame {shown_grid} · decoding (showing {})", media.index().grid_of[*p]),
        None => format!("frame {shown_grid} · decoding…"),
    };
    hud(&painter, rect.left_top() + Vec2::new(10.0, 8.0), Align2::LEFT_TOP, &state, if exact { style::TEXT } else { Color32::from_rgb(0xfb, 0xbf, 0x24) });
    super::overlay::draw(ui, &painter, &response, world, &mapping, shown_grid, active_view, eased);
    breadcrumb(ui, world, rect.left_top() + Vec2::new(10.0, 34.0), active_view, shown_grid);
    speed(&painter, rect, t.rate, world);
    let media = world.resource::<Media>();
    if let Some(pos) = response.hover_pos() {
        let src = space.to_source(mapping.to_canvas(pos));
        hud(&painter, rect.right_top() + Vec2::new(-10.0, 44.0), Align2::RIGHT_TOP, &format!("{:.1}, {:.1} px · {:.0}%", src[0], src[1], screen_per_source * 100.0), style::MUTED);
    }
    let active = media.source(prefer).expect("preferred source exists");
    let stats = active.player.stats();
    let source_label = match shown_source {
        Some(Which::Proxy) => format!("proxy {}p", media.proxy().map_or(0, |p| p.index.height)),
        Some(Which::Original) => "original".to_string(),
        None => "—".to_string(),
    };

    let now = world.resource::<tt_core::time::WallClock>().now;
    world.resource_mut::<ActiveSource>().0 = prefer;
    let mut probe = world.resource_mut::<PlaybackProbe>();
    if t.playing {
        let started = *probe.started.get_or_insert(now);
        probe.seconds = now - started;
        probe.frames += 1;
        if !exact {
            probe.late += 1;
        }
        if exact && probe.last_shown != Some(shown_grid) {
            probe.distinct += 1;
            probe.last_shown = Some(shown_grid);
        }
    } else if probe.was_playing {
        tracing::info!(
            "playback: {:.2} s, {} UI frames ({:.1}/s), {} distinct video frames ({:.1} fps shown), {} UI frames without the exact frame",
            probe.seconds,
            probe.frames,
            probe.frames as f64 / probe.seconds.max(1e-9),
            probe.distinct,
            probe.distinct as f64 / probe.seconds.max(1e-9),
            probe.late
        );
        probe.started = None;
    }
    probe.was_playing = t.playing;
    let line = format!(
        "{source_label} · cache {} fr / {:.2} GB · decoded {} · spawns {} · upload {:.2} ms · late {}/{}{}",
        stats.cached_frames,
        stats.cached_bytes as f64 / 1e9,
        stats.decoded,
        stats.spawns,
        LAST_UPLOAD_US.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1000.0,
        probe.late,
        probe.frames,
        stats.error.map(|e| format!(" · error: {e}")).unwrap_or_default(),
    );
    hud(&painter, rect.left_bottom() + Vec2::new(10.0, -8.0), Align2::LEFT_BOTTOM, &line, style::MUTED);
    if let Some(probe) = world.get_resource::<crate::input_probe::InputProbe>() {
        hud(&painter, rect.center_top() + Vec2::new(0.0, 34.0), Align2::CENTER_TOP, &format!("input probe · {}", probe.summary), style::ACCENT);
    }
    let easing = world.resource::<FramingEase>().from.is_some_and(|(_, t0)| now - t0 < EASE_SECONDS);
    world.resource_mut::<WaitingForFrame>().0 = !exact || easing;
}

/// `Source ▸ Sketch 1 ▸ Sketch 3`: where the viewport is; click a level to go there.
fn breadcrumb(ui: &mut egui::Ui, world: &mut World, at: Pos2, active: Option<Entity>, frame: tt_core::time::FrameIndex) {
    let chain = chain(world, active);
    let selected = world.resource::<tt_core::selection::Selection>().primary().and_then(|e| tt_core::sketch::sketch_of(world, e));
    let name = |w: &World, e: Entity| sketch_framed(w, e).and_then(|s| w.get::<Name>(s)).map_or("view".to_string(), |n| n.to_string());
    let mut go: Option<Option<Entity>> = None;
    // Its own foreground layer: a click here is the breadcrumb's, never a press on the video.
    egui::Area::new(egui::Id::new("viewport-breadcrumb")).order(egui::Order::Foreground).fixed_pos(at).interactable(true).show(ui.ctx(), |ui| {
        egui::Frame::new().fill(Color32::from_black_alpha(170)).corner_radius(3.0).inner_margin(egui::Margin::symmetric(6, 2)).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                let crumb = |ui: &mut egui::Ui, text: String, current: bool| {
                    let rich = egui::RichText::new(text).monospace().size(12.0);
                    if current {
                        ui.label(rich.color(style::TEXT).strong());
                        false
                    } else {
                        ui.add(egui::Label::new(rich.color(style::ACCENT)).sense(Sense::click())).on_hover_text("Go to this level").clicked()
                    }
                };
                if crumb(ui, "Source".into(), chain.is_empty()) {
                    go = Some(None);
                }
                for (i, v) in chain.iter().enumerate() {
                    ui.label(egui::RichText::new("▸").monospace().size(12.0).color(style::MUTED));
                    if crumb(ui, name(world, *v), i + 1 == chain.len()) {
                        go = Some(Some(*v));
                    }
                }
                if let Some(v) = active
                    && let Some(s) = sketch_framed(world, v)
                    && world.get::<tt_core::op::Output>(s).and_then(|o| world.resource::<tt_core::signal::SignalStore>().get(o.0)).is_some_and(|sig| sig.get(frame).is_none())
                {
                    ui.label(egui::RichText::new("· outside this sketch: nearest framing").monospace().size(12.0).color(Color32::from_rgb(0xfb, 0xbf, 0x24)));
                }
                let hint = match (selected, active.and_then(|v| sketch_framed(world, v))) {
                    (Some(s), current) if Some(s) != current => Some(format!("· Tab: view {}", world.get::<Name>(s).map_or("sketch".into(), |n| n.to_string()))),
                    _ if active.is_some() => Some("· Shift+Tab: up".to_string()),
                    _ => None,
                };
                if let Some(h) = hint {
                    ui.label(egui::RichText::new(h).monospace().size(12.0).color(style::MUTED));
                }
            });
        });
    });
    if let Some(v) = go {
        world.resource_mut::<ActiveView>().0 = v;
    }
}

/// The playback speed, always in view (it is the capture speed of the next
/// stroke): a badge top-right, and a big flash in the middle when it changes.
fn speed(painter: &egui::Painter, rect: Rect, rate: f64, world: &mut World) {
    const FLASH: f64 = 1.2;
    let now = world.resource::<tt_core::time::WallClock>().now;
    let mut flash = world.resource_mut::<SpeedFlash>();
    if flash.rate != Some(rate) {
        // Not on the first frame (opening a video isn't a change).
        flash.changed_at = if flash.rate.is_some() { now } else { f64::NEG_INFINITY };
        flash.rate = Some(rate);
    }
    let age = now - flash.changed_at;
    let label = super::timeline::rate_label(rate);
    let color = if (rate - 1.0).abs() < 1e-9 { style::TEXT } else { Color32::from_rgb(0xfb, 0xbf, 0x24) };

    let galley = painter.layout_no_wrap(format!("{label} speed"), FontId::proportional(20.0), color);
    let r = Align2::RIGHT_TOP.anchor_size(rect.right_top() + Vec2::new(-10.0, 8.0), galley.size()).expand(5.0);
    painter.rect_filled(r, 4.0, Color32::from_black_alpha(190));
    painter.galley(r.min + Vec2::splat(5.0), galley, color);

    if age < FLASH {
        // Full for 0.6 s, then fading out.
        let alpha = (1.0 - ((age - 0.6) / (FLASH - 0.6)).clamp(0.0, 1.0)) as f32;
        let galley = painter.layout_no_wrap(label, FontId::proportional(96.0), color.gamma_multiply(alpha));
        let r = Align2::CENTER_CENTER.anchor_size(rect.center(), galley.size()).expand(18.0);
        painter.rect_filled(r, 12.0, Color32::from_black_alpha((170.0 * alpha) as u8));
        painter.galley(r.min + Vec2::splat(18.0), galley, color.gamma_multiply(alpha));
        painter.ctx().request_repaint();
    }
}

/// When the playback speed last changed (for the flash).
#[derive(Resource, Debug)]
pub struct SpeedFlash {
    rate: Option<f64>,
    changed_at: f64,
}

impl Default for SpeedFlash {
    fn default() -> Self {
        Self { rate: None, changed_at: f64::NEG_INFINITY }
    }
}

fn hud(painter: &egui::Painter, pos: Pos2, align: Align2, text: &str, color: Color32) {
    let galley = painter.layout_no_wrap(text.to_string(), FontId::monospace(12.0), color);
    let r = align.anchor_size(pos, galley.size()).expand(4.0);
    painter.rect_filled(r, 3.0, Color32::from_black_alpha(170));
    painter.galley(r.min + Vec2::splat(4.0), galley, color);
}
