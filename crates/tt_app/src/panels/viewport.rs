//! Viewport: the video frame at the playhead, with zoom/pan (session state in
//! the world). The frame is drawn by the GPU through a paint callback
//! (video.rs); while the exact frame is still decoding, the nearest cached
//! frame is shown and labelled, so the picture never blanks.

use bevy_ecs::prelude::*;
use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Vec2};
use tt_core::transport::Transport;
use tt_core::{AppBuilder, Class, Module, Set};
use tt_core::input::{Action, PendingActions};

use crate::media::{ActiveSource, Media, Which};
use crate::style;
use crate::video::{LAST_UPLOAD_US, VideoPaint};

/// Zoom/pan of the viewport. Session state (not undoable).
#[derive(Resource, Debug, Clone, Copy)]
pub struct ViewportView {
    /// 1.0 = the whole frame fits the panel.
    pub zoom: f32,
    /// Video point (0..1 each axis) at the panel centre.
    pub center: [f32; 2],
}

impl Default for ViewportView {
    fn default() -> Self {
        Self { zoom: 1.0, center: [0.5, 0.5] }
    }
}

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
    let mut view = *world.resource::<ViewportView>();
    let (vw, vh) = {
        let m = world.resource::<Media>();
        (m.index().width as f32, m.index().height as f32)
    };

    // Zoom about the cursor, pan with the secondary/middle button.
    let fit = (rect.width() / vw).min(rect.height() / vh);
    let video_rect = |view: &ViewportView| {
        let size = Vec2::new(vw, vh) * fit * view.zoom;
        let min = rect.center() - Vec2::new(view.center[0] * size.x, view.center[1] * size.y);
        Rect::from_min_size(min, size)
    };
    if let Some(pos) = response.hover_pos() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll != 0.0 {
            let before = video_rect(&view);
            let uv = (pos - before.min) / before.size();
            view.zoom = (view.zoom * (scroll * 0.0025).exp()).clamp(0.1, 64.0);
            let size = Vec2::new(vw, vh) * fit * view.zoom;
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
    let px_per_source = vr.width() / vw;
    let shown_grid = t.frame();

    // Show the proxy unless it would be magnified: past its resolution, the original.
    let media = world.resource::<Media>();
    let prefer = match media.proxy() {
        Some(p) if vr.width() <= p.index.width as f32 => Which::Proxy,
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
        let callback = VideoPaint {
            key: (media.generation * 2 + u64::from(which == Which::Proxy), p),
            data,
            width: src.index.width,
            height: src.index.height,
            scale: [rect.width() / vr.width(), rect.height() / vr.height()],
            offset: [(rect.min.x - vr.min.x) / vr.width(), (rect.min.y - vr.min.y) / vr.height()],
            background,
            nearest: vr.width() / src.index.width as f32 >= 3.0,
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
    if let Some(pos) = response.hover_pos() {
        let src = (pos - vr.min) / px_per_source;
        hud(&painter, rect.right_top() + Vec2::new(-10.0, 8.0), Align2::RIGHT_TOP, &format!("{:.1}, {:.1} px · {:.0}%", src.x, src.y, px_per_source * 100.0), style::MUTED);
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
    world.resource_mut::<WaitingForFrame>().0 = !exact;
}

fn hud(painter: &egui::Painter, pos: Pos2, align: Align2, text: &str, color: Color32) {
    let galley = painter.layout_no_wrap(text.to_string(), FontId::monospace(12.0), color);
    let r = align.anchor_size(pos, galley.size()).expand(4.0);
    painter.rect_filled(r, 3.0, Color32::from_black_alpha(170));
    painter.galley(r.min + Vec2::splat(4.0), galley, color);
}
