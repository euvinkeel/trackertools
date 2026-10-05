//! The export window: a new video rendered here from the source, at its size
//! and frame rate (tt_media::render), for any editor, with nothing to keep
//! working there: a stabilized copy, or a tracking target to point an
//! editor's own tracker at. Opened from the right-click menu. It renders
//! the frames from the in point to the out point (`tt_core::marks`, I and O
//! on the timeline; they can be moved while the window is open), or the
//! whole video.
//!
//! A stabilized copy can be zoomed and moved in its frame
//! (`tt_track::export::Framing`), and a small preview shows the playhead's
//! frame as the export renders it: the decoded frame drawn on the CPU
//! through the export's own map (`rendered_map`), with guidelines over it.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use bevy_ecs::name::Name;
use bevy_ecs::prelude::*;
use egui::{Color32, Stroke, Vec2};
use tt_core::input::{Action, PendingActions};
use tt_core::time::FrameIndex;
use tt_media::render::{Codec, invert, nv12_preview, render_marker, render_warped};
use tt_track::export::{Framing, Smoothing, StabilizerDefaults, Steady, follow, follow_path, follower_at, good_points, hides_the_edges, rendered_map, stabilize, stabilize_path, subject_path, zoom_to_fill};

use crate::media::{Media, StatusLine, Which};
use crate::style;

/// The preview fits in this (points), at the output's aspect ratio.
const PREVIEW: Vec2 = Vec2::new(320.0, 240.0);
/// The most zoom the slider offers, and how far the picture moves (a fraction of the frame).
const MAX_ZOOM: f32 = 4.0;
const MAX_OFFSET: f32 = 0.5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Stabilized,
    Target,
}

/// What it goes by: a subject's final data, or trackers' and sketches' points.
#[derive(Clone, Debug)]
pub enum Source {
    Subject(Entity),
    Points(Vec<Entity>),
}

#[derive(Resource, Default)]
pub struct ExportWindow {
    request: Option<Request>,
    job: Option<Job>,
    done: Option<Result<PathBuf, String>>,
    preview: Preview,
}

/// The stabilized export's preview: the playhead's frame as the export renders it, small.
struct Preview {
    texture: Option<egui::TextureHandle>,
    /// What the texture shows (made again when any of it changes).
    shows: Option<Shown>,
    /// Over it: the centre cross and the thirds; the point it follows.
    guides: bool,
    point: bool,
    /// Where it was drawn last (points).
    rect: Option<egui::Rect>,
}

impl Default for Preview {
    fn default() -> Self {
        Self { texture: None, shows: None, guides: true, point: true, rect: None }
    }
}

/// A preview picture: which decoded frame (the media's generation, the
/// proxy's or the original's, the presented frame and its grid frame), the
/// framing and the size in pixels.
#[derive(Clone, Copy, PartialEq)]
struct Shown {
    generation: u64,
    proxy: bool,
    presented: usize,
    grid: FrameIndex,
    framing: Framing,
    pixels: [usize; 2],
}

struct Request {
    kind: Kind,
    /// What it follows, as the window says it, and the thing itself.
    name: String,
    follows: Source,
    video: String,
    /// The frame held as it is (asked for, and as made: the first with the points if not that one).
    here: FrameIndex,
    reference: FrameIndex,
    keys: Vec<Steady>,
    /// How the motion is used (smoothing, rotation, placement) as the keys
    /// were made, and how the picture is framed (zoom, position).
    options: StabilizerDefaults,
    /// Where what it follows is on each frame with a fit, as the keys were
    /// made (source px, y down; `Stabilization::followed`): what the
    /// stabilizer holds still.
    followed: Vec<(FrameIndex, [f64; 2])>,
    /// Several points: each one's frames (source px, y down). Else empty.
    points: Vec<Vec<(FrameIndex, [f64; 2])>>,
    /// The source video (the default name is made from it).
    source: PathBuf,
    codec: Codec,
    /// The whole video, even with in and out points marked.
    whole: bool,
    /// The zoom that hides the black edges ([`Request::fill_zoom`]).
    zoom: Option<FillZoom>,
    path: PathBuf,
    /// The path is the default one (a change of format changes its extension).
    default_path: bool,
}

/// The least zoom that hides the black edges (up to [`MAX_ZOOM`]), for the
/// frames and offset it was worked out for, and whether it does hide them
/// (else it is the most).
#[derive(Clone)]
struct FillZoom {
    frames: Range<FrameIndex>,
    offset: [f64; 2],
    zoom: f64,
    hides: bool,
}

struct Job {
    progress: Arc<AtomicUsize>,
    total: usize,
    cancel: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<Result<PathBuf, String>>>,
    started: Instant,
}

impl Request {
    /// The least zoom that hides the black edges on every one of `frames`,
    /// the picture moved as the options say. It takes a pass over the frames
    /// for each step of a search, so it is worked out again only when the
    /// frames or the offset change (or the keys: `zoom` is cleared), and not
    /// while the pointer is held (`settled` false: a drag goes on). Then
    /// the last one serves until the drag ends.
    fn fill_zoom(&mut self, size: [f64; 2], frames: &Range<FrameIndex>, settled: bool) -> FillZoom {
        let offset = self.options.offset();
        match &self.zoom {
            Some(z) if (z.frames == *frames && z.offset == offset) || !settled => z.clone(),
            _ => {
                let zoom = zoom_to_fill(&self.keys, size, frames.clone(), offset, MAX_ZOOM as f64);
                // Below the most, it hides them; at the most, perhaps not.
                let hides = zoom < MAX_ZOOM as f64 || hides_the_edges(&self.keys, size, frames.clone(), Framing { zoom, offset });
                let made = FillZoom { frames: frames.clone(), offset, zoom, hides };
                self.zoom = Some(made.clone());
                made
            }
        }
    }

    /// The zoom that hides the black edges if it is known for `frames` and the offset as they are now.
    fn known_fill_zoom(&self, frames: &Range<FrameIndex>) -> Option<FillZoom> {
        self.zoom.clone().filter(|z| z.frames == *frames && z.offset == self.options.offset())
    }

    /// How the export frames the picture: the zoom that hides the edges or the one chosen, and the offset.
    fn framing(&mut self, size: [f64; 2], frames: &Range<FrameIndex>, settled: bool) -> Framing {
        let zoom = if self.options.fill { self.fill_zoom(size, frames, settled).zoom } else { self.options.zoom.clamp(1.0, MAX_ZOOM) as f64 };
        Framing { zoom, offset: self.options.offset() }
    }
}

/// The export window is open (the viewport dims what it leaves out).
pub fn is_open(world: &World) -> bool {
    world.get_resource::<ExportWindow>().is_some_and(|w| w.request.is_some())
}

/// Where the open window's preview is on screen (points; the scene demo drags in it).
pub fn preview_rect(world: &World) -> Option<egui::Rect> {
    world.get_resource::<ExportWindow>().filter(|w| w.request.is_some()).and_then(|w| w.preview.rect)
}

/// What an export renders: from the in point to the out point, or the whole video.
fn frames_to_export(world: &World, whole: bool, count: FrameIndex) -> Range<FrameIndex> {
    if whole { 0..count } else { tt_core::marks::marks(world).frames(count) }
}

/// `361 frames, 7.22 s` (minutes past a minute).
fn length(frames: FrameIndex, fps: f64) -> String {
    let secs = frames as f64 / fps.max(1e-9);
    if secs < 60.0 { format!("{frames} frames, {secs:.2} s") } else { format!("{frames} frames, {}:{:05.2}", (secs / 60.0).floor(), secs % 60.0) }
}

/// The keys for `kind` following `source`, held as on frame `here`, with `options`.
fn make(world: &World, kind: Kind, source: &Source, here: FrameIndex, options: StabilizerDefaults) -> Option<tt_track::export::Stabilization> {
    let index = &world.get_resource::<Media>()?.original.index;
    let size = [index.width as f64, index.height as f64];
    let fps = world.resource::<tt_core::transport::Transport>().fps.as_f64();
    let smoothing = options.into();
    match (source, kind) {
        (Source::Subject(s), Kind::Stabilized) => stabilize_path(&subject_path(world, *s), size, here, smoothing, fps),
        (Source::Subject(s), Kind::Target) => follow_path(&subject_path(world, *s), size, here, smoothing, fps),
        (Source::Points(p), k) => {
            let tracks: Vec<_> = p.iter().map(|e| good_points(world, *e)).collect();
            if k == Kind::Stabilized { stabilize(&tracks, size, here, smoothing, fps) } else { follow(&tracks, size, here, smoothing, fps) }
        }
    }
}

/// Open the window for `kind`, following `source` as it is now (the
/// stabilizer holds it as on the playhead's frame, or the in point's when
/// the playhead is outside the in and out points).
pub fn open(world: &mut World, kind: Kind, source: Source) {
    let Some(media) = world.get_resource::<Media>() else {
        world.resource_mut::<StatusLine>().0 = Some(("Open a video first".into(), true));
        return;
    };
    let index = media.original.index.clone();
    let video = media.name.clone();
    let transport = world.resource::<tt_core::transport::Transport>();
    let marked = tt_core::marks::marks(world).frames(index.frame_count());
    let here = transport.frame().clamp(marked.start, (marked.end - 1).max(marked.start));
    let options = *world.resource::<StabilizerDefaults>();
    let made = make(world, kind, &source, here, options);
    let Some(made) = made else {
        world.resource_mut::<StatusLine>().0 = Some(("Nothing to export: no frame has the points it needs".into(), true));
        return;
    };
    let name = match &source {
        Source::Subject(s) => world.get::<Name>(*s).map_or("the subject".to_string(), |n| n.to_string()),
        Source::Points(p) if p.len() == 1 => world.get::<Name>(p[0]).map_or("the point".to_string(), |n| n.to_string()),
        Source::Points(p) => format!("{} points", p.len()),
    };
    let points = match &source {
        Source::Points(p) if p.len() > 1 => p.iter().map(|e| good_points(world, *e)).collect(),
        _ => Vec::new(),
    };
    let codec = if kind == Kind::Stabilized { Codec::ProRes422Hq } else { Codec::H264 };
    let path = default_path(&index.path, kind, &name, codec);
    let w = &mut *world.resource_mut::<ExportWindow>();
    w.done = None;
    w.preview.shows = None;
    let video_path = index.path.clone();
    w.request = Some(Request {
        kind,
        name,
        follows: source,
        video,
        here,
        reference: made.reference,
        keys: made.keys,
        options,
        followed: made.followed,
        points,
        source: video_path,
        codec,
        whole: false,
        zoom: None,
        path,
        default_path: true,
    });
}

/// Next to the source: "<video> - stabilized (<what>).mov"; never an existing file.
fn default_path(source: &Path, kind: Kind, name: &str, codec: Codec) -> PathBuf {
    let stem = source.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "video".into());
    let what = if kind == Kind::Stabilized { "stabilized" } else { "tracking target" };
    let name: String = name.chars().map(|c| if r#"\/:*?"<>|"#.contains(c) { '_' } else { c }).collect();
    let base = format!("{stem} - {what} ({name})");
    let dir = source.parent().map(Path::to_path_buf).unwrap_or_default();
    (1..)
        .map(|n| dir.join(if n == 1 { format!("{base}.{}", codec.extension()) } else { format!("{base} {n}.{}", codec.extension()) }))
        .find(|p| !p.exists())
        .expect("a free name")
}

/// Render `frames` of the source as `r` says, framed by `framing` (stabilized), on a thread of its own.
fn start(world: &World, r: &Request, frames: Range<FrameIndex>, framing: Framing) -> Option<Job> {
    let index = world.get_resource::<Media>()?.original.index.clone();
    let total = (frames.end - frames.start).max(0) as usize;
    let size = [index.width as f64, index.height as f64];
    let (progress, cancel) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicBool::new(false)));
    let (done, stop) = (progress.clone(), cancel.clone());
    let (keys, out, codec, kind) = (r.keys.clone(), r.path.clone(), r.codec, r.kind);
    // The marker: about a 24th of the picture's height across its half.
    let half = (size[0].min(size[1]) / 24.0).clamp(16.0, 120.0);
    let handle = std::thread::spawn(move || {
        let made = match kind {
            Kind::Stabilized => render_warped(&index, &out, codec, frames, &|g| rendered_map(&keys, size, framing, g), &done, &stop),
            Kind::Target => render_marker(&index, &out, codec, frames, &|g| follower_at(&keys, size, g), half, &done, &stop),
        };
        made.map(|()| out).map_err(|e| format!("{e:#}"))
    });
    Some(Job { progress, total, cancel, handle: Some(handle), started: Instant::now() })
}

/// The preview's size (points): the output's aspect ratio, inside [`PREVIEW`].
fn preview_size(size: [f64; 2]) -> Vec2 {
    let aspect = (size[0] / size[1].max(1.0)) as f32;
    if aspect >= PREVIEW.x / PREVIEW.y { Vec2::new(PREVIEW.x, (PREVIEW.x / aspect).round()) } else { Vec2::new((PREVIEW.y * aspect).round(), PREVIEW.y) }
}

/// Makes the preview's picture again when what it shows changes: the
/// decoded frame nearest the playhead's (the proxy's if it has it: plenty
/// for a small picture) through the export's own map, as `framing` frames
/// it. True: it shows the playhead's frame (else a neighbour, until that
/// one is decoded).
fn refresh_preview(ctx: &egui::Context, world: &World, p: &mut Preview, r: &Request, size: [f64; 2], framing: Framing) -> bool {
    let Some(media) = world.get_resource::<Media>() else { return true };
    let wanted = media.presented(world.resource::<tt_core::transport::Transport>().frame());
    let exact = |w: Which| media.source(w).and_then(|s| s.player.frame(wanted)).map(|d| (w, wanted, d));
    let nearest = |w: Which| media.source(w).and_then(|s| s.player.frame_or_nearest(wanted)).map(|(q, d)| (w, q, d));
    let Some((which, presented, data)) = exact(Which::Proxy).or_else(|| exact(Which::Original)).or_else(|| nearest(Which::Proxy)).or_else(|| nearest(Which::Original)) else {
        return false;
    };
    // As many pixels as it takes on screen, up to 480 across.
    let display = preview_size(size);
    let scale = ctx.pixels_per_point().min(480.0 / display.x);
    let pixels = [((display.x * scale).round() as usize).max(1), ((display.y * scale).round() as usize).max(1)];
    let grid = media.index().grid_of[presented];
    let shown = Shown { generation: media.generation, proxy: which == Which::Proxy, presented, grid, framing, pixels };
    if p.shows != Some(shown) {
        let src = media.source(which).expect("it gave the frame");
        let (fw, fh) = (src.index.width as usize, src.index.height as usize);
        // Preview pixels, to output pixels, to source pixels (the export's map), to this rendition's pixels.
        let m = rendered_map(&r.keys, size, framing, grid);
        let (sx, sy) = (size[0] / pixels[0] as f64, size[1] / pixels[1] as f64);
        let (fx, fy) = (fw as f64 / size[0], fh as f64 / size[1]);
        let map = [fx * m[0] * sx, fx * m[1] * sy, fx * m[2], fy * m[3] * sx, fy * m[4] * sy, fy * m[5]];
        let image = egui::ColorImage::from_rgb(pixels, &nv12_preview(&data, fw, fh, media.color, &map, pixels));
        match &mut p.texture {
            Some(t) => t.set(image, egui::TextureOptions::LINEAR),
            None => p.texture = Some(ctx.load_texture("export-preview", image, egui::TextureOptions::LINEAR)),
        }
        p.shows = Some(shown);
    }
    presented == wanted
}

/// The preview (`display` points) and what is over it. Returns where it is,
/// and how far it was dragged this frame (a part of its width and height).
fn preview_ui(ui: &mut egui::Ui, p: &Preview, r: &Request, size: [f64; 2], display: Vec2) -> (egui::Rect, Option<Vec2>) {
    let (rect, response) = ui.allocate_exact_size(display, egui::Sense::drag());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, Color32::BLACK);
    match &p.texture {
        Some(t) => {
            painter.image(t.id(), rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
        }
        None => {
            painter.text(rect.center(), egui::Align2::CENTER_CENTER, "Decoding\u{2026}", egui::FontId::proportional(13.0), style::MUTED);
        }
    }
    if p.guides {
        let faint = Stroke::new(1.0, Color32::from_white_alpha(70));
        for t in [1.0 / 3.0, 2.0 / 3.0] {
            painter.vline(rect.left() + rect.width() * t, rect.y_range(), faint);
            painter.hline(rect.x_range(), rect.top() + rect.height() * t, faint);
        }
        // The centre: a cross, edged in dark so it shows on a bright picture too.
        let (c, arm) = (rect.center(), 10.0);
        for stroke in [Stroke::new(3.0, Color32::from_black_alpha(120)), Stroke::new(1.0, Color32::from_white_alpha(210))] {
            painter.hline(c.x - arm..=c.x + arm, c.y, stroke);
            painter.vline(c.x, c.y - arm..=c.y + arm, stroke);
        }
    }
    // Where what it follows is in the output: the export's map, the other way.
    // The ring: what the stabilizer holds (the points' anchor, the subject's
    // point), on the frames it was measured on. Several points: a dot on each one there.
    if p.point
        && let Some(shown) = p.shows
        && let Some(back) = invert(&rendered_map(&r.keys, size, shown.framing, shown.grid))
    {
        let on_screen = |s: [f64; 2]| {
            let o = [back[0] * s[0] + back[1] * s[1] + back[2], back[3] * s[0] + back[4] * s[1] + back[5]];
            rect.min + Vec2::new((o[0] / size[0]) as f32 * rect.width(), (o[1] / size[1]) as f32 * rect.height())
        };
        let at = |t: &[(FrameIndex, [f64; 2])]| t.binary_search_by_key(&shown.grid, |(f, _)| *f).ok().map(|i| t[i].1);
        let colour = if matches!(r.follows, Source::Subject(_)) { style::SUBJECT } else { style::AUTO };
        for s in r.points.iter().filter_map(|t| at(t)) {
            painter.circle_filled(on_screen(s), 2.0, colour);
        }
        if let Some(s) = at(&r.followed) {
            painter.circle_stroke(on_screen(s), 5.0, Stroke::new(1.5, colour));
        }
    }
    painter.rect_stroke(rect, 0.0, Stroke::new(1.0, style::RULER), egui::StrokeKind::Inside);
    if response.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        let d = response.drag_delta();
        return (rect, (d != Vec2::ZERO).then(|| Vec2::new(d.x / rect.width(), d.y / rect.height())));
    }
    if response.hovered() && ui.is_enabled() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
    }
    (rect, None)
}

/// The window (drawn every frame; empty unless an export was asked for).
pub fn ui(ctx: &egui::Context, world: &mut World) {
    // A finished job: its result.
    if let Some(job) = world.resource_mut::<ExportWindow>().job.as_mut()
        && job.handle.as_ref().is_some_and(|h| h.is_finished())
    {
        let result = job.handle.take().expect("a job").join().unwrap_or_else(|_| Err("the export stopped unexpectedly".into()));
        let w = &mut *world.resource_mut::<ExportWindow>();
        w.job = None;
        let cancelled = result.as_ref().err().is_some_and(|e| e.contains("cancelled"));
        w.done = Some(result.map_err(|e| if cancelled { "Cancelled: nothing was saved.".to_string() } else { e }));
    }
    let mut state = std::mem::take(&mut *world.resource_mut::<ExportWindow>());
    let Some((count, fps, size)) = world.get_resource::<Media>().map(|m| {
        let i = &m.original.index;
        (i.frame_count(), i.fps.as_f64(), [i.width as f64, i.height as f64])
    }) else {
        *world.resource_mut::<ExportWindow>() = state;
        return;
    };
    if state.request.is_none() {
        *world.resource_mut::<ExportWindow>() = state;
        return;
    }
    // In and out, as they are now (they can be moved while the window is open).
    let marked = tt_core::marks::marks(world);
    let frames = frames_to_export(world, state.request.as_ref().is_some_and(|r| r.whole), count);
    // Stabilized: the framing in effect, the zoom that hides the edges (worked
    // out only when it is on, and not during a drag), and the preview's picture.
    let settled = !ctx.input(|i| i.pointer.any_down());
    let (fill, framing) = match state.request.as_mut() {
        Some(r) if r.kind == Kind::Stabilized => {
            let framing = r.framing(size, &frames, settled);
            (if r.options.fill { r.zoom.clone() } else { r.known_fill_zoom(&frames) }, framing)
        }
        _ => (None, Framing::default()),
    };
    if let Some(r) = state.request.as_ref().filter(|r| r.kind == Kind::Stabilized)
        && !refresh_preview(ctx, world, &mut state.preview, r, size, framing)
    {
        // Until the playhead's frame is decoded (the preview shows a neighbour meanwhile).
        ctx.request_repaint_after(std::time::Duration::from_millis(40));
    }
    let playhead = world.resource::<tt_core::transport::Transport>().frame();
    let display = preview_size(size);
    let mut open = true;
    let mut go = false;
    let mut options_changed: Option<StabilizerDefaults> = None;
    let mut seek: Option<FrameIndex> = None;
    let title = match state.request.as_ref().map(|r| r.kind) {
        Some(Kind::Stabilized) => "Export stabilized video",
        _ => "Export tracking target video",
    };
    egui::Window::new(title).collapsible(false).resizable(false).default_width(460.0).open(&mut open).show(ctx, |ui| {
        let busy = state.job.is_some();
        if let Some(r) = state.request.as_mut() {
            let about = match r.kind {
                Kind::Stabilized => {
                    let how = if r.options.rotation { "moved and turned" } else { "moved (never turned)" };
                    let place = match (r.options.centre, r.options.offset != [0.0; 2]) {
                        (true, false) => format!("{} stays in the middle of the picture", r.name),
                        (true, true) => format!("{} stays in one place in the picture, where Position puts it", r.name),
                        (false, _) => format!("{} holds still as it is on frame {}", r.name, r.reference),
                    };
                    format!(
                        "A new video file, the same size and frame rate as {}, every frame {how} so that {place}. \
                         The sound comes along. Use it in any editor like any other clip: there is nothing to set up or keep working there.",
                        r.video
                    )
                }
                Kind::Target => format!(
                    "A video the size of {}, as long as the frames you export: on black, a marker that moves (and turns) with {}. \
                     In your editor, put it on a track above the footage, lined up with those frames, and point the editor's own tracker at the marker's middle, where its squares cross \
                     (two-point or planar trackers can use the small square beside it for the turn). Attach your text to that track, then hide this layer.",
                    r.video, r.name
                ),
            };
            ui.label(about);
            ui.add_space(6.0);
            ui.add_enabled_ui(!busy, |ui| {
                egui::Grid::new("export-options").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    ui.label("Frames");
                    ui.vertical(|ui| {
                        let whole = format!("Whole video: frames 0\u{2013}{} ({})", count - 1, length(count, fps));
                        if marked.is_set() {
                            let m = marked.frames(count);
                            ui.radio_value(&mut r.whole, false, format!("In to out: frames {}\u{2013}{} ({})", m.start, m.end - 1, length(m.end - m.start, fps)))
                                .on_hover_text("Move the in and out points on the timeline (drag their brackets, or I and O at the playhead); this follows.");
                            ui.radio_value(&mut r.whole, true, whole);
                        } else {
                            ui.label(whole);
                            ui.label(egui::RichText::new("To export part of it, mark in and out on the timeline: I and O at the playhead.").weak().small());
                        }
                    });
                    ui.end_row();
                    ui.label("Format");
                    let before = r.codec;
                    egui::ComboBox::from_id_salt("export-codec").selected_text(r.codec.label()).width(300.0).show_ui(ui, |ui| {
                        for c in Codec::ALL {
                            ui.selectable_value(&mut r.codec, c, c.label());
                        }
                    });
                    if r.codec != before {
                        r.path = if r.default_path { default_path(&r.source, r.kind, &r.name, r.codec) } else { r.path.with_extension(r.codec.extension()) };
                    }
                    ui.end_row();
                    // How the motion is used (remembered; the right-click menu's Fusion copies use it too).
                    let mut o = r.options;
                    ui.label("Rotation");
                    let turns = if r.kind == Kind::Stabilized { "Undo its turning too" } else { "The marker turns with it" };
                    ui.checkbox(&mut o.rotation, turns)
                        .on_hover_text("With two or more points, or a subject's angle. Off: position only: the picture moves, but never turns.");
                    ui.end_row();
                    if r.kind == Kind::Stabilized {
                        ui.label("Placement");
                        ui.vertical(|ui| {
                            ui.radio_value(&mut o.centre, true, format!("Keep {} in the middle of the picture", r.name))
                                .on_hover_text("On every frame, what it follows is in the middle (the centre of the points, or the subject's point).");
                            ui.radio_value(&mut o.centre, false, format!("Hold it where it is on frame {}", r.here))
                                .on_hover_text("The classic stabilizer: the picture keeps its framing on that frame.");
                        });
                        ui.end_row();
                        // How the picture sits in the frame (remembered; rendered exports only).
                        ui.label("Zoom");
                        ui.vertical(|ui| {
                            let least = "The least zoom that covers the frame on every frame you export, with the picture where Position puts it. \
                                         It is the same on all frames. With the subject in the middle, it keeps the subject in the middle.";
                            let (text, tip) = match &fill {
                                Some(z) if !z.hides => (
                                    egui::RichText::new(format!("Cannot hide every black edge, even at \u{d7}{:.2}", z.zoom)).color(style::LIVE),
                                    "Even the most zoom does not cover the frame on every frame you export. When this is on, the export zooms in that much. \
                                     To hide the edges, move the picture back to the middle (Position), or export fewer frames.",
                                ),
                                Some(z) => (egui::RichText::new(format!("Just enough to hide the black edges (\u{d7}{:.2})", z.zoom)), least),
                                None => (egui::RichText::new("Just enough to hide the black edges"), least),
                            };
                            ui.checkbox(&mut o.fill, text).on_hover_text(tip);
                            // (Clamped on edits only: a value shown is never rounded and written back, which would turn the option above off.)
                            let mut z = framing.zoom as f32;
                            let tip = if o.fill {
                                "The zoom in use. Move the slider to set a zoom of your own."
                            } else {
                                "Your zoom, the same on all frames. At \u{d7}1.00 you see the whole picture: black shows where the picture moved away."
                            };
                            let slider = egui::Slider::new(&mut z, 1.0..=MAX_ZOOM).clamping(egui::SliderClamping::Edits).max_decimals(2).prefix("\u{d7}");
                            if ui.add(slider).on_hover_text(tip).changed() {
                                (o.fill, o.zoom) = (false, z);
                            }
                        });
                        ui.end_row();
                        ui.label("Position");
                        ui.vertical(|ui| {
                            // (Clamped on edits only, and one axis at a time: a dragged offset is kept as it is, not rounded.)
                            let mut percent = o.offset.map(|v| v * 100.0);
                            let range = -MAX_OFFSET * 100.0..=MAX_OFFSET * 100.0;
                            let x = ui
                                .add(egui::Slider::new(&mut percent[0], range.clone()).clamping(egui::SliderClamping::Edits).max_decimals(1).suffix("%").text("X"))
                                .on_hover_text("Moves the picture to the right (more than 0) or to the left (less than 0). The value is a percentage of the width.");
                            let y = ui
                                .add(egui::Slider::new(&mut percent[1], range).clamping(egui::SliderClamping::Edits).max_decimals(1).suffix("%").text("Y"))
                                .on_hover_text("Moves the picture down (more than 0) or up (less than 0). The value is a percentage of the height.");
                            if x.changed() {
                                o.offset[0] = percent[0] / 100.0;
                            }
                            if y.changed() {
                                o.offset[1] = percent[1] / 100.0;
                            }
                            ui.horizontal(|ui| {
                                if ui.add_enabled(o.offset != [0.0; 2], egui::Button::new("Reset")).on_hover_text("Puts the picture back where the stabilizer puts it.").clicked() {
                                    o.offset = [0.0; 2];
                                }
                                ui.label(egui::RichText::new("You can also drag the picture in the preview.").weak().small());
                            });
                        });
                        ui.end_row();
                        ui.label("Preview");
                        ui.vertical(|ui| {
                            let (at, dragged) = preview_ui(ui, &state.preview, r, size, display);
                            state.preview.rect = Some(at);
                            if let Some(d) = dragged {
                                o.offset = [(o.offset[0] + d.x).clamp(-MAX_OFFSET, MAX_OFFSET), (o.offset[1] + d.y).clamp(-MAX_OFFSET, MAX_OFFSET)];
                            }
                            let point = match &r.follows {
                                Source::Points(p) if p.len() > 1 => "A dot on each point, and a ring on the point that the stabilizer holds (their centre).".to_string(),
                                _ => format!("A ring where {} is.", r.name),
                            };
                            ui.horizontal(|ui| {
                                ui.checkbox(&mut state.preview.guides, "Guidelines").on_hover_text("A cross at the centre, and lines at one third and two thirds.");
                                ui.checkbox(&mut state.preview.point, "Point").on_hover_text(point);
                            });
                            let last = (frames.end - 1).max(frames.start);
                            let mut f = playhead.clamp(frames.start, last);
                            ui.horizontal(|ui| {
                                ui.spacing_mut().slider_width = (display.x - 90.0).max(80.0);
                                let frame = ui.add(egui::Slider::new(&mut f, frames.start..=last).prefix("frame "));
                                if frame.on_hover_text("The frame in the preview: the playhead. Move it here or on the timeline.").changed() {
                                    seek = Some(f);
                                }
                            });
                            if !frames.contains(&playhead) {
                                ui.label(egui::RichText::new(format!("Frame {playhead} is outside the frames to export.")).weak().small());
                            }
                        });
                        ui.end_row();
                    }
                    if o != r.options {
                        options_changed = Some(o);
                    }
                    ui.label("Save as");
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(r.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()).monospace())
                            .on_hover_text(r.path.display().to_string());
                        if ui.button("Change\u{2026}").clicked() {
                            let mut dialog = rfd::FileDialog::new().add_filter(r.codec.extension(), &[r.codec.extension()]).set_file_name(r.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
                            if let Some(dir) = r.path.parent() {
                                dialog = dialog.set_directory(dir);
                            }
                            if let Some(p) = dialog.save_file() {
                                r.path = p;
                                r.default_path = false;
                            }
                        }
                    });
                    ui.end_row();
                });
            });
        }
        ui.add_space(8.0);
        match (&state.job, &state.done) {
            (Some(job), _) => {
                let done = job.progress.load(Ordering::Relaxed);
                let part = done as f32 / job.total.max(1) as f32;
                let left = if done > 10 {
                    let secs = job.started.elapsed().as_secs_f64() / done as f64 * (job.total - done) as f64;
                    if secs > 90.0 { format!(", about {:.0} min left", secs / 60.0) } else { format!(", about {secs:.0} s left") }
                } else {
                    String::new()
                };
                ui.add(egui::ProgressBar::new(part).text(format!("frame {done} of {}{left}", job.total)));
                if ui.button("Cancel").clicked() {
                    job.cancel.store(true, Ordering::Relaxed);
                }
                ctx.request_repaint_after(std::time::Duration::from_millis(200));
            }
            (None, Some(Ok(path))) => {
                ui.label(egui::RichText::new(format!("\u{2714} Saved {}", path.display())).color(style::ACCENT));
                ui.horizontal(|ui| {
                    if ui.button("Show in folder").clicked() {
                        show_in_folder(path);
                    }
                    if ui.button("Export again").clicked() {
                        go = true;
                    }
                });
            }
            (None, done) => {
                if let Some(Err(e)) = done {
                    ui.label(egui::RichText::new(e).color(egui::Color32::from_rgb(0xf4, 0x3f, 0x5e)));
                }
                if ui.button("Export").clicked() {
                    go = true;
                }
            }
        }
    });
    // Another way to use the motion: the keys again (and the zoom that hides the edges), remembered.
    // Another zoom or position: the same keys, framed another way.
    if let Some(o) = options_changed
        && let Some(r) = state.request.as_mut()
    {
        if Smoothing::from(o) != Smoothing::from(r.options) {
            if let Some(made) = make(world, r.kind, &r.follows, r.here, o) {
                (r.keys, r.reference, r.followed) = (made.keys, made.reference, made.followed);
            }
            r.zoom = None;
            state.preview.shows = None;
        }
        r.options = o;
        *world.resource_mut::<StabilizerDefaults>() = o;
    }
    if let Some(f) = seek {
        world.resource_mut::<PendingActions>().push(Action::Seek(f));
    }
    if go && let Some(r) = state.request.as_mut() {
        state.done = None;
        let framing = r.framing(size, &frames, true);
        state.job = start(world, r, frames, framing);
    }
    if !open {
        if let Some(job) = &state.job {
            job.cancel.store(true, Ordering::Relaxed);
        }
        state.request = None;
        (state.preview.texture, state.preview.shows) = (None, None);
    }
    *world.resource_mut::<ExportWindow>() = state;
}

fn show_in_folder(path: &Path) {
    crate::files::reveal(path);
}
