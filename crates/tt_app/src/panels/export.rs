//! The export window: a new video rendered here from the source, at its size
//! and frame rate (tt_media::render), for any editor, with nothing to keep
//! working there: a stabilized copy, or a tracking target to point an
//! editor's own tracker at. Opened from the right-click menu. It renders
//! the frames from the in point to the out point (`tt_core::marks`, I and O
//! on the timeline; they can be moved while the window is open), or the
//! whole video.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use bevy_ecs::name::Name;
use bevy_ecs::prelude::*;
use tt_core::time::FrameIndex;
use tt_media::render::{Codec, IDENTITY, render_marker, render_warped};
use tt_track::export::{StabilizerDefaults, Steady, follow, follow_path, follower_at, good_points, key_at, stabilize, stabilize_path, stabilizer_map, subject_path, zoom_to_fill};

use crate::media::{Media, StatusLine};
use crate::style;

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
    /// How the motion is used (smoothing, rotation, placement), as the keys were made.
    options: StabilizerDefaults,
    /// The source video (the default name is made from it).
    source: PathBuf,
    codec: Codec,
    /// The whole video, even with in and out points marked.
    whole: bool,
    /// Zoom in to hide the black edges (stabilized), by [`Request::zoom`].
    fill: bool,
    /// That zoom, for the frames it was worked out for.
    zoom: Option<(Range<FrameIndex>, f64)>,
    path: PathBuf,
    /// The path is the default one (a change of format changes its extension).
    default_path: bool,
}

struct Job {
    progress: Arc<AtomicUsize>,
    total: usize,
    cancel: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<Result<PathBuf, String>>>,
    started: Instant,
}

impl Request {
    /// The least zoom that hides the black edges on every one of `frames` (worked out again when they change).
    fn zoom(&mut self, size: [f64; 2], frames: &Range<FrameIndex>) -> f64 {
        match &self.zoom {
            Some((for_frames, z)) if for_frames == frames => *z,
            _ => {
                let z = zoom_to_fill(&self.keys, size, frames.clone(), 3.0);
                self.zoom = Some((frames.clone(), z));
                z
            }
        }
    }
}

/// The export window is open (the viewport dims what it leaves out).
pub fn is_open(world: &World) -> bool {
    world.get_resource::<ExportWindow>().is_some_and(|w| w.request.is_some())
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
    let codec = if kind == Kind::Stabilized { Codec::ProRes422Hq } else { Codec::H264 };
    let path = default_path(&index.path, kind, &name, codec);
    let w = &mut *world.resource_mut::<ExportWindow>();
    w.done = None;
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
        source: video_path,
        codec,
        whole: false,
        fill: false,
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

/// Render `frames` of the source as `r` says, `zoom`ed (stabilized), on a thread of its own.
fn start(world: &World, r: &Request, frames: Range<FrameIndex>, zoom: f64) -> Option<Job> {
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
            Kind::Stabilized => render_warped(&index, &out, codec, frames, &|g| key_at(&keys, g).map_or(IDENTITY, |k| stabilizer_map(&k, size, zoom)), &done, &stop),
            Kind::Target => render_marker(&index, &out, codec, frames, &|g| follower_at(&keys, size, g), half, &done, &stop),
        };
        made.map(|()| out).map_err(|e| format!("{e:#}"))
    });
    Some(Job { progress, total, cancel, handle: Some(handle), started: Instant::now() })
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
    let zoom = state.request.as_mut().map_or(1.0, |r| r.zoom(size, &frames));
    let mut open = true;
    let mut go = false;
    let mut options_changed: Option<StabilizerDefaults> = None;
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
                    let place = if r.options.centre { format!("{} stays in the middle of the picture", r.name) } else { format!("{} holds still as it is on frame {}", r.name, r.reference) };
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
                        ui.label("Edges");
                        ui.vertical(|ui| {
                            ui.radio_value(&mut r.fill, false, "Keep the whole picture (black shows where it moved away)");
                            ui.radio_value(&mut r.fill, true, format!("Zoom in just enough to hide the black edges (\u{d7}{zoom:.2})"))
                                .on_hover_text("The least zoom that covers the frame on every frame exported; the same all through. Centred, it is worked out with the subject in the middle.");
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
    if let Some(o) = options_changed
        && let Some(r) = state.request.as_mut()
    {
        if let Some(made) = make(world, r.kind, &r.follows, r.here, o) {
            (r.keys, r.reference) = (made.keys, made.reference);
        }
        (r.options, r.zoom) = (o, None);
        *world.resource_mut::<StabilizerDefaults>() = o;
    }
    if go && let Some(r) = state.request.as_ref() {
        state.done = None;
        state.job = start(world, r, frames, if r.fill { zoom } else { 1.0 });
    }
    if !open {
        if let Some(job) = &state.job {
            job.cancel.store(true, Ordering::Relaxed);
        }
        state.request = None;
    }
    *world.resource_mut::<ExportWindow>() = state;
}

fn show_in_folder(path: &Path) {
    crate::files::reveal(path);
}
