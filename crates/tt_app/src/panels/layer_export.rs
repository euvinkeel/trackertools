//! Exporting layers (tt_core::layer, tt_media::layers), and the motion of
//! anything tracked as data:
//!
//! - **The Export layers window** (right-click a layer): the video with the
//!   layers on top (ProRes, DNxHR, H.264, with its sound), or the layers
//!   alone on transparency (ProRes 4444, a PNG sequence, WebM), the size,
//!   frame rate and frames of the video, from the in point to the out point
//!   or the whole video; or the motion data as JSON. The layers are read at
//!   their own size for it, on a thread of its own.
//! - **Motion data (JSON)** ([`motion_json`]): per frame, each layer's
//!   placement (its anchor's point, angle, scale, opacity, clip time and its
//!   picture's corners) and each tracked thing's point, box, angle (a
//!   subject's) and whether that frame can be trusted; source pixels, y
//!   down, and also as fractions of the frame.
//! - **After Effects keyframes** ([`ae_keyframes`]): text to paste onto a
//!   layer in After Effects (its Transform: position, rotation, scale and
//!   opacity for a layer; position, and rotation for a subject, for anything
//!   tracked), one key a frame from the first frame exported, which is the
//!   comp's frame 0.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use bevy_ecs::prelude::*;
use serde_json::json;
use tt_core::layer::{LayerParams, Placed, placed_at, target_of};
use tt_core::op::Output;
use tt_core::signal::SignalStore;
use tt_core::time::FrameIndex;
use tt_media::layers::{AlphaFormat, Overlay};
use tt_media::render::Codec;

use crate::media::{Media, StatusLine};
use crate::style;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum What {
    Over,
    Alpha,
    Json,
}

struct Setup {
    what: What,
    /// Every layer, and whether it goes in.
    layers: Vec<(Entity, String, bool)>,
    codec: Codec,
    alpha: AlphaFormat,
    whole: bool,
    path: PathBuf,
    /// The path is the default (a change of what or format changes it).
    default_path: bool,
}

struct Job {
    progress: Arc<AtomicUsize>,
    total: usize,
    cancel: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<Result<PathBuf, String>>>,
    /// Reading the layers' media (before the first frame).
    reading: Arc<AtomicBool>,
}

#[derive(Resource, Default)]
pub struct LayerExport {
    setup: Option<Setup>,
    job: Option<Job>,
    done: Option<Result<PathBuf, String>>,
}

/// What it exports: the in point to the out point, or the whole video.
fn frames(world: &World, whole: bool) -> Option<Range<FrameIndex>> {
    let count = world.get_resource::<Media>()?.index().frame_count();
    Some(if whole { 0..count } else { tt_core::marks::marks(world).frames(count) })
}

/// Next to the video: "<video> - layers" with the format's extension; never an existing file.
fn default_path(source: &Path, what: What, codec: Codec, alpha: AlphaFormat) -> PathBuf {
    let stem = source.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "video".into());
    let (base, ext) = match what {
        What::Over => (format!("{stem} - with layers"), codec.extension()),
        What::Alpha => (format!("{stem} - layers"), alpha.extension()),
        What::Json => (format!("{stem} - motion"), "json"),
    };
    let dir = source.parent().map(Path::to_path_buf).unwrap_or_default();
    let name = |n: usize| {
        let b = if n == 1 { base.clone() } else { format!("{base} {n}") };
        if ext.is_empty() { b } else { format!("{b}.{ext}") }
    };
    (1..).map(|n| dir.join(name(n))).find(|p| !p.exists()).expect("a free name")
}

/// Open the window with every layer in it.
pub fn open(world: &mut World) {
    let Some(source) = world.get_resource::<Media>().map(|m| m.index().path.clone()) else {
        world.resource_mut::<StatusLine>().0 = Some(("Open a video first".into(), true));
        return;
    };
    let layers: Vec<(Entity, String, bool)> = tt_core::layer::layers(world).into_iter().map(|e| (e, crate::panels::outliner::label(world, e), true)).collect();
    let (codec, alpha) = (Codec::ProRes422Hq, AlphaFormat::ProRes4444);
    let mut x = world.resource_mut::<LayerExport>();
    x.done = None;
    x.setup = Some(Setup { what: What::Over, layers, codec, alpha, whole: false, path: default_path(&source, What::Over, codec, alpha), default_path: true });
}

/// Whether any of `layers` (or what it follows) still has frames to work
/// out: an export now would miss them, so it waits (a frame or two).
fn pending(world: &World, layers: &[Entity]) -> bool {
    let dirty = |e: Entity| world.get::<tt_core::op::Dirty>(e).is_some_and(|d| !d.0.is_empty());
    layers.iter().any(|l| dirty(*l) || target_of(world, *l).is_some_and(dirty))
}

/// Start the open window's export as it is set (the scene demo's `=layer-render`):
/// the whole video's frames 560–620, `alpha` or over the video, to `path`.
/// False: not yet (the layers are still being worked out).
pub fn start_now(world: &mut World, alpha: bool, path: PathBuf) -> bool {
    let chosen: Vec<Entity> = world.resource::<LayerExport>().setup.as_ref().map(|s| s.layers.iter().filter(|(_, _, on)| *on).map(|(e, _, _)| *e).collect()).unwrap_or_default();
    if chosen.is_empty() || pending(world, &chosen) {
        return false;
    }
    let Some(mut s) = world.resource_mut::<LayerExport>().setup.take() else { return false };
    s.what = if alpha { What::Alpha } else { What::Over };
    s.path = path.clone();
    let layers: Vec<Entity> = s.layers.iter().filter(|(_, _, on)| *on).map(|(e, _, _)| *e).collect();
    let job = start(world, &layers, s.what, s.codec, s.alpha, 560..620, path);
    let x = &mut *world.resource_mut::<LayerExport>();
    x.setup = Some(s);
    x.job = job;
    true
}

/// The window, while open.
pub fn ui(ctx: &egui::Context, world: &mut World) {
    // A finished job: its result.
    if let Some(job) = world.resource_mut::<LayerExport>().job.as_mut()
        && job.handle.as_ref().is_some_and(|h| h.is_finished())
    {
        let result = job.handle.take().expect("a job").join().unwrap_or_else(|_| Err("the export stopped unexpectedly".into()));
        let x = &mut *world.resource_mut::<LayerExport>();
        x.job = None;
        let cancelled = result.as_ref().err().is_some_and(|e| e.contains("cancelled"));
        x.done = Some(result.map_err(|e| if cancelled { "Cancelled: nothing was saved.".to_string() } else { e }));
    }
    if world.resource::<LayerExport>().setup.is_none() {
        return;
    }
    let Some(source) = world.get_resource::<Media>().map(|m| m.index().path.clone()) else { return };
    let mut x = std::mem::take(&mut *world.resource_mut::<LayerExport>());
    let (mut open, mut go, mut cancel, mut reveal) = (true, false, false, None);
    let busy = x.job.is_some();
    // An export waits while the chosen layers are being worked out (else it would miss frames).
    let waiting = x.setup.as_ref().is_some_and(|s| pending(world, &s.layers.iter().filter(|(_, _, on)| *on).map(|(e, _, _)| *e).collect::<Vec<_>>()));
    let range = frames(world, x.setup.as_ref().is_some_and(|s| s.whole)).unwrap_or(0..0);
    let fps = world.resource::<tt_core::transport::Transport>().fps.as_f64();
    egui::Window::new("Export layers").collapsible(false).resizable(false).default_width(440.0).open(&mut open).show(ctx, |ui| {
        let s = x.setup.as_mut().expect("open");
        ui.add_enabled_ui(!busy, |ui| {
            let before = (s.what, s.codec, s.alpha);
            ui.radio_value(&mut s.what, What::Over, "The video with the layers on top");
            ui.radio_value(&mut s.what, What::Alpha, "The layers alone, on transparency (to put over the video in your editor)");
            ui.radio_value(&mut s.what, What::Json, "The motion as data (JSON): every layer and what it follows, frame by frame");
            ui.add_space(4.0);
            match s.what {
                What::Over => {
                    egui::ComboBox::from_label("Format").selected_text(s.codec.label()).show_ui(ui, |ui| {
                        for c in Codec::ALL {
                            ui.selectable_value(&mut s.codec, c, c.label());
                        }
                    });
                }
                What::Alpha => {
                    egui::ComboBox::from_label("Format").selected_text(s.alpha.label()).show_ui(ui, |ui| {
                        for a in AlphaFormat::ALL {
                            ui.selectable_value(&mut s.alpha, a, a.label());
                        }
                    });
                }
                What::Json => {}
            }
            if s.default_path && before != (s.what, s.codec, s.alpha) {
                s.path = default_path(&source, s.what, s.codec, s.alpha);
            }
            ui.add_space(4.0);
            ui.label("Layers (bottom first):");
            if s.layers.is_empty() {
                ui.label(egui::RichText::new("No layers yet: right-click a sketch, tracker or subject, then Attach a picture, GIF or clip.").color(style::MUTED));
            }
            for (_, name, on) in &mut s.layers {
                ui.checkbox(on, name.as_str());
            }
            ui.add_space(4.0);
            ui.checkbox(&mut s.whole, "The whole video (not only from the in point to the out point)");
            ui.label(egui::RichText::new(format!("Frames {} to {} ({:.2} s)", range.start, range.end - 1, (range.end - range.start) as f64 / fps.max(1e-9))).color(style::MUTED).small());
            ui.horizontal(|ui| {
                let shown = s.path.display().to_string();
                ui.label(egui::RichText::new(&shown).monospace().small()).on_hover_text(&shown);
                if ui.small_button("Choose\u{2026}").clicked() {
                    let name = s.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let mut d = rfd::FileDialog::new().set_file_name(name);
                    if let Some(dir) = s.path.parent() {
                        d = d.set_directory(dir);
                    }
                    let chosen = match (s.what, s.alpha) {
                        (What::Alpha, AlphaFormat::PngSequence) => d.set_title("A new folder for the pictures").save_file(),
                        (What::Json, _) => d.add_filter("JSON", &["json"]).save_file(),
                        (What::Over, _) => d.add_filter(s.codec.extension(), &[s.codec.extension()]).save_file(),
                        (What::Alpha, a) => d.add_filter(a.extension(), &[a.extension()]).save_file(),
                    };
                    if let Some(p) = chosen {
                        s.path = p;
                        s.default_path = false;
                    }
                }
            });
        });
        let chosen = s.layers.iter().filter(|(_, _, on)| *on).count();
        ui.add_space(6.0);
        match &x.job {
            Some(job) => {
                let done = job.progress.load(Ordering::Relaxed);
                if job.reading.load(Ordering::Relaxed) {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Reading the layers' pictures at their own size\u{2026}");
                    });
                } else {
                    ui.add(egui::ProgressBar::new(done as f32 / job.total.max(1) as f32).text(format!("frame {done} of {}", job.total)));
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            }
            None if waiting => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Working out where the layers are\u{2026}");
                });
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            }
            None => {
                if ui.add_enabled(chosen > 0 && !range.is_empty(), egui::Button::new(if s.what == What::Json { "Save" } else { "Export" })).clicked() {
                    go = true;
                }
            }
        }
        match &x.done {
            Some(Ok(p)) => {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Saved.").color(style::ACCENT));
                    if ui.small_button("Show it").clicked() {
                        reveal = Some(p.clone());
                    }
                });
            }
            Some(Err(e)) => {
                ui.label(egui::RichText::new(e).color(style::LOST));
            }
            None => {}
        }
    });
    if cancel && let Some(job) = &x.job {
        job.cancel.store(true, Ordering::Relaxed);
    }
    if go && let Some(s) = &x.setup {
        x.done = None;
        let chosen: Vec<Entity> = s.layers.iter().filter(|(_, _, on)| *on).map(|(e, _, _)| *e).collect();
        match s.what {
            What::Json => {
                let targets: Vec<Entity> = chosen.iter().filter_map(|l| target_of(world, *l)).collect();
                x.done = Some(write_json(world, &s.path, &chosen, &targets, range.clone()).map(|()| s.path.clone()));
            }
            what => x.job = start(world, &chosen, what, s.codec, s.alpha, range.clone(), s.path.clone()),
        }
    }
    if let Some(p) = reveal {
        crate::files::reveal(&p);
    }
    if !open && x.job.is_none() {
        x.setup = None;
    }
    *world.resource_mut::<LayerExport>() = x;
}

/// A layer's placement on each frame exported.
type Placements = Vec<Option<Placed>>;

/// Each chosen layer's media path, its own size, and its placement on each of `frames`.
type Gathered = Vec<(String, [f32; 2], Placements)>;

fn gather(world: &World, layers: &[Entity], frames: &Range<FrameIndex>) -> Gathered {
    layers
        .iter()
        .filter_map(|e| {
            let p = world.get::<LayerParams>(*e)?;
            Some((p.media.clone(), p.media_size, frames.clone().map(|g| placed_at(world, *e, g)).collect()))
        })
        .collect()
}

/// Render on a thread of its own: read the layers' media at their own size (up to a memory limit), then the frames.
fn start(world: &World, layers: &[Entity], what: What, codec: Codec, alpha: AlphaFormat, frames: Range<FrameIndex>, out: PathBuf) -> Option<Job> {
    let index = world.get_resource::<Media>()?.original.index.clone();
    let gathered = gather(world, layers, &frames);
    let total = (frames.end - frames.start).max(0) as usize;
    let (progress, cancel, reading) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(true)));
    let (done, stop, read) = (progress.clone(), cancel.clone(), reading.clone());
    let handle = std::thread::spawn(move || {
        // 1.5 GB for all the layers' frames, shared out.
        let budget = (1536usize << 20) / gathered.len().max(1);
        let mut media: Vec<(Arc<tt_media::overlay::Frames>, [f32; 2], Placements)> = Vec::new();
        for (path, size, placed) in gathered {
            let p = Path::new(&path);
            let info = tt_media::overlay::probe(p).map_err(|e| format!("{e:#}"))?;
            let frames = tt_media::overlay::decode(p, &info, tt_media::overlay::fit(&info, 8192, budget)).map_err(|e| format!("{e:#}"))?;
            media.push((Arc::new(frames), size, placed));
        }
        read.store(false, Ordering::Relaxed);
        let first = frames.start;
        let places: Vec<Box<dyn Fn(FrameIndex) -> Option<Placed> + Sync>> = media
            .iter()
            .map(|(_, _, placed)| {
                let placed = placed.clone();
                Box::new(move |g: FrameIndex| placed.get((g - first) as usize).copied().flatten()) as Box<dyn Fn(FrameIndex) -> Option<Placed> + Sync>
            })
            .collect();
        let overlays: Vec<Overlay> = media.iter().zip(&places).map(|((f, size, _), at)| Overlay { frames: f.clone(), size: *size, placed: at.as_ref() }).collect();
        let made = match what {
            What::Over => tt_media::layers::render_over(&index, &out, codec, frames, &overlays, &done, &stop),
            _ => tt_media::layers::render_alpha(&index, &out, alpha, frames, &overlays, &done, &stop),
        };
        made.map(|()| out).map_err(|e| format!("{e:#}"))
    });
    Some(Job { progress, total, cancel, handle: Some(handle), reading })
}

/// A tracked thing's output on frame `g`: `[x, y, left, top, right, bottom, angle (rad, a subject's), trusted]`.
fn tracked_at(world: &World, e: Entity, g: FrameIndex) -> Option<([f64; 6], Option<f64>, bool)> {
    let v = world.resource::<SignalStore>().get(world.get::<Output>(e)?.0)?.get(g)?;
    if v.len() < 6 || !tt_core::span::span_of(world, e).contains(g) {
        return None;
    }
    let b = std::array::from_fn(|i| v[i] as f64);
    let subject = tt_core::layer::has_angle(world, e);
    let angle = subject.then(|| v.get(6).copied().unwrap_or(0.0) as f64);
    let trusted = subject || v.get(7).is_none_or(|f| *f == 0.0);
    Some((b, angle, trusted))
}

/// The motion of `layers` and `targets` over `frames` as JSON (module docs).
pub fn motion_json(world: &World, layers: &[Entity], targets: &[Entity], frames: Range<FrameIndex>) -> serde_json::Value {
    let index = world.get_resource::<Media>().map(|m| m.index());
    let (w, h) = index.as_ref().map_or((1920.0, 1080.0), |i| (i.width as f64, i.height as f64));
    let fps = world.resource::<tt_core::transport::Transport>().fps.as_f64();
    let r = |v: f64| (v * 1000.0).round() / 1000.0;
    let name = |e: Entity| crate::panels::outliner::label(world, e);
    let layer_json: Vec<_> = layers
        .iter()
        .filter_map(|e| {
            let p = world.get::<LayerParams>(*e)?;
            let rows: Vec<_> = frames
                .clone()
                .filter_map(|g| {
                    let pl = placed_at(world, *e, g)?;
                    let corners: Vec<_> = pl.corners(p.media_size).iter().map(|c| json!([r(c[0]), r(c[1])])).collect();
                    Some(json!({
                        "frame": g, "seconds": r(g as f64 / fps),
                        "x": r(pl.at[0]), "y": r(pl.at[1]), "x_fraction": r(pl.at[0] / w), "y_fraction": r(pl.at[1] / h),
                        "angle_degrees": r(pl.angle.to_degrees()), "scale": r(pl.scale[0]), "opacity": r(pl.opacity),
                        "clip_seconds": r(pl.clip_time), "anchor": [r(pl.anchor[0]), r(pl.anchor[1])], "corners": corners,
                    }))
                })
                .collect();
            Some(json!({
                "name": name(*e), "media": p.media, "media_size": [p.media_size[0], p.media_size[1]],
                "attached_to": target_of(world, *e).map(name), "frames": rows,
            }))
        })
        .collect();
    let mut seen = Vec::new();
    let target_json: Vec<_> = targets
        .iter()
        .filter(|t| {
            let new = !seen.contains(*t);
            seen.push(**t);
            new
        })
        .map(|t| {
            let rows: Vec<_> = frames
                .clone()
                .filter_map(|g| {
                    let (b, angle, trusted) = tracked_at(world, *t, g)?;
                    let mut row = json!({
                        "frame": g, "seconds": r(g as f64 / fps),
                        "x": r(b[0]), "y": r(b[1]), "x_fraction": r(b[0] / w), "y_fraction": r(b[1] / h),
                        "box": [r(b[2]), r(b[3]), r(b[4]), r(b[5])], "trusted": trusted,
                    });
                    if let Some(a) = angle {
                        row["angle_degrees"] = json!(r(a.to_degrees()));
                    }
                    Some(row)
                })
                .collect();
            json!({ "name": name(*t), "kind": crate::icons::Glyph::of(world, *t).name(), "frames": rows })
        })
        .collect();
    json!({
        "made_by": format!("trackertools {}", crate::update::version()),
        "video": index.as_ref().map(|i| i.path.display().to_string()),
        "width": w, "height": h, "fps": r(fps),
        "first_frame": frames.start, "last_frame": frames.end - 1,
        "units": "source pixels, y down; angles clockwise on screen; frames of the video",
        "layers": layer_json,
        "tracked": target_json,
    })
}

fn write_json(world: &World, path: &Path, layers: &[Entity], targets: &[Entity], frames: Range<FrameIndex>) -> Result<(), String> {
    let text = serde_json::to_string_pretty(&motion_json(world, layers, targets, frames)).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| format!("Couldn't write {}: {e}", path.display()))
}

/// Save the motion of `e` (a layer, or anything tracked) as JSON: a file dialog, then the file.
pub fn save_motion(world: &mut World, e: Entity) {
    let Some(source) = world.get_resource::<Media>().map(|m| m.index().path.clone()) else { return };
    let whole = frames(world, false).unwrap_or(0..0);
    let path = default_path(&source, What::Json, Codec::H264, AlphaFormat::ProRes4444);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut d = rfd::FileDialog::new().add_filter("JSON", &["json"]).set_file_name(name);
    if let Some(dir) = path.parent() {
        d = d.set_directory(dir);
    }
    let Some(path) = d.save_file() else { return };
    let (layers, targets) = if tt_core::layer::is_layer(world, e) { (vec![e], target_of(world, e).into_iter().collect()) } else { (Vec::new(), vec![e]) };
    let line = match write_json(world, &path, &layers, &targets, whole) {
        Ok(()) => (format!("Saved the motion to {}", path.display()), false),
        Err(err) => (err, true),
    };
    world.resource_mut::<StatusLine>().0 = Some(line);
}

/// After Effects' keyframe text for `e` (a layer, or anything tracked)
/// over `frames` (module docs); None: it has no frame there.
pub fn ae_keyframes(world: &World, e: Entity, frames: Range<FrameIndex>) -> Option<String> {
    let index = world.get_resource::<Media>().map(|m| m.index());
    let (w, h) = index.as_ref().map_or((1920, 1080), |i| (i.width, i.height));
    let fps = world.resource::<tt_core::transport::Transport>().fps.as_f64();
    let first = frames.start;
    // (frame in the comp, position, angle in degrees, scale %, opacity %)
    type AeKey = (FrameIndex, [f64; 2], Option<f64>, Option<f64>, Option<f64>);
    let mut keys: Vec<AeKey> = Vec::new();
    for g in frames {
        if tt_core::layer::is_layer(world, e) {
            if let Some(pl) = placed_at(world, e, g) {
                keys.push((g - first, pl.at, Some(pl.angle.to_degrees()), Some(pl.scale[0] * 100.0), Some(pl.opacity * 100.0)));
            }
        } else if let Some((b, angle, _)) = tracked_at(world, e, g) {
            keys.push((g - first, [b[0], b[1]], angle.map(f64::to_degrees), None, None));
        }
    }
    if keys.is_empty() {
        return None;
    }
    let mut t = String::from("Adobe After Effects 8.0 Keyframe Data\r\n\r\n");
    t += &format!("\tUnits Per Second\t{}\r\n\tSource Width\t{w}\r\n\tSource Height\t{h}\r\n\tSource Pixel Aspect Ratio\t1\r\n\tComp Pixel Aspect Ratio\t1\r\n\r\n", (fps * 1000.0).round() / 1000.0);
    t += "Transform\tPosition\r\n\tFrame\tX pixels\tY pixels\tZ pixels\t\r\n";
    for (f, p, ..) in &keys {
        t += &format!("\t{f}\t{:.3}\t{:.3}\t0\t\r\n", p[0], p[1]);
    }
    if keys.iter().any(|k| k.2.is_some()) {
        t += "\r\nTransform\tRotation\r\n\tFrame\tdegrees\t\r\n";
        for (f, _, a, ..) in &keys {
            t += &format!("\t{f}\t{:.4}\t\r\n", a.unwrap_or(0.0));
        }
    }
    if keys.iter().any(|k| k.3.is_some()) {
        t += "\r\nTransform\tScale\r\n\tFrame\tX percent\tY percent\tZ percent\t\r\n";
        for (f, _, _, s, _) in &keys {
            let s = s.unwrap_or(100.0);
            t += &format!("\t{f}\t{s:.3}\t{s:.3}\t100\t\r\n");
        }
    }
    if keys.iter().any(|k| k.4.is_some()) {
        t += "\r\nTransform\tOpacity\r\n\tFrame\tpercent\t\r\n";
        for (f, .., o) in &keys {
            t += &format!("\t{f}\t{:.2}\t\r\n", o.unwrap_or(100.0));
        }
    }
    t += "\r\n\r\nEnd of Keyframe Data\r\n";
    Some(t)
}

/// Copy `e`'s After Effects keyframes (the in point to the out point) to the clipboard, and say how to paste them.
pub fn copy_ae_keyframes(ctx: &egui::Context, world: &mut World, e: Entity) {
    let Some(range) = frames(world, false) else { return };
    let first = range.start;
    let line = match ae_keyframes(world, e, range) {
        Some(text) => {
            ctx.copy_text(text);
            (format!("Copied. In After Effects, select a layer and paste (Ctrl+V): frame 0 of the comp is frame {first} of the video."), false)
        }
        None => ("Nothing to copy: it has no frames between the in and out points.".to_string(), true),
    };
    world.resource_mut::<StatusLine>().0 = Some(line);
}

pub struct LayerExportModule;

impl tt_core::Module for LayerExportModule {
    fn build(&self, app: &mut tt_core::AppBuilder) {
        app.declare::<LayerExport>(tt_core::Class::Session).init_resource::<LayerExport>();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_ecs::name::Name;
    use tt_core::history::edit;
    use tt_core::op::{Inputs, Operator};

    /// A tracker-like point moving 2 px right a frame (frame 5 flagged lost), with a layer on it.
    fn world() -> (tt_core::Core, Entity, Entity) {
        let mut app = tt_core::AppBuilder::new();
        app.add_module(tt_core::CoreModules).add_module(tt_track::TrackModule);
        let mut core = app.build();
        core.world.resource_mut::<tt_core::transport::Transport>().frame_count = 20;
        let mut t = None;
        edit(&mut core.world, "tracker", |tx| {
            let out = tx.create_signal(8);
            for f in 0..10 {
                let x = 100.0 + 2.0 * f as f32;
                tx.signal(out).set(f, &[x, 50.0, x - 5.0, 45.0, x + 5.0, 55.0, 0.9, if f == 5 { 1.0 } else { 0.0 }]);
            }
            t = Some(tx.spawn((Name::new("Tracker 1"), Operator { kind: "track".into() }, Inputs(Vec::new()), Output(out))));
        });
        let t = t.unwrap();
        let p = LayerParams { media: "face.png".into(), media_size: [10.0, 10.0], offset_x: tt_core::layer::Animated::fixed(3.0), ..LayerParams::default() };
        let l = tt_core::layer::attach(&mut core.world, t, p, 0).unwrap();
        for _ in 0..3 {
            core.run_pre_ui();
        }
        (core, t, l)
    }

    #[test]
    fn motion_reads_as_json_and_as_after_effects_keyframes() {
        let (core, t, l) = world();
        let w = &core.world;
        let j = motion_json(w, &[l], &[t, t], 0..10);
        assert_eq!(j["layers"][0]["name"], "face");
        assert_eq!(j["layers"][0]["attached_to"], "Tracker 1");
        let f3 = &j["layers"][0]["frames"][3];
        assert_eq!((f3["frame"].as_i64(), f3["x"].as_f64(), f3["y"].as_f64()), (Some(3), Some(109.0), Some(50.0)), "the point plus its offset");
        assert_eq!(j["tracked"].as_array().unwrap().len(), 1, "each tracked thing once");
        let lost = &j["tracked"][0]["frames"][5];
        assert_eq!((lost["trusted"].as_bool(), lost["box"][0].as_f64()), (Some(false), Some(105.0)), "a lost frame says so");
        assert!(j["tracked"][0]["frames"][4]["angle_degrees"].is_null(), "a tracker has no angle");

        // From frame 2: the comp's frame 0.
        let ae = ae_keyframes(w, l, 2..10).expect("keys");
        assert!(ae.starts_with("Adobe After Effects 8.0 Keyframe Data"));
        assert!(ae.contains("Transform\tPosition") && ae.contains("Transform\tScale") && ae.contains("Transform\tOpacity"));
        assert!(ae.contains("\t0\t107.000\t50.000\t0\t"), "frame 2 of the video is the comp's 0:\n{ae}");
        assert!(ae.trim_end().ends_with("End of Keyframe Data"));
        let tracked = ae_keyframes(w, t, 0..10).expect("keys");
        assert!(!tracked.contains("Rotation") && !tracked.contains("Scale"), "a tracker: position only");
        assert!(ae_keyframes(w, l, 15..18).is_none(), "nothing there");
    }
}
