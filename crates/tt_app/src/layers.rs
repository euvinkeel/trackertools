//! Layers in the app (tt_core::layer): their media decoded for the preview,
//! drawn on the video, and attached from the menu or by dropping a picture.
//!
//! - **The preview** decodes each media file once, in the background, at
//!   most [`PREVIEW_SIDE`] on its longer side and [`PREVIEW_BYTES`] for all
//!   its frames (an export reads it again at its own size). A texture per
//!   frame shown (two layers on one file can show different frames at once),
//!   the most recently shown kept, up to [`TEXTURES`] a file.
//! - **On the video**, between the picture and the editor's marks: each
//!   layer where its output puts it on the shown frame, through the shown
//!   view, at its opacity; the selected one outlined, with its anchor. A
//!   file that can't be found shows as a grey box saying so; one still
//!   loading, as a dashed one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bevy_ecs::prelude::*;
use egui::{Color32, Painter, Pos2, Shape, Stroke, TextureHandle};
use tt_core::layer::{LayerParams, layers, placed_at};
use tt_core::time::FrameIndex;
use tt_core::view::SpaceMap;
use tt_media::overlay::{Frames, MediaInfo};

use crate::panels::viewport::ViewportMapping;
use crate::style;

/// The preview's longest side (px).
pub const PREVIEW_SIDE: u32 = 512;
/// The preview's memory for one file's frames (bytes).
pub const PREVIEW_BYTES: usize = 192 << 20;

/// Media a layer can show (the file dialog's filter and drops).
pub const PICTURES: &[&str] = &["png", "jpg", "jpeg", "webp", "bmp", "gif", "tif", "tiff"];
pub const CLIPS: &[&str] = &["mp4", "mov", "webm", "mkv", "m4v", "avi"];

enum Load {
    Loading,
    Ready(Arc<Frames>),
    Failed(String),
}

/// Textures kept per file (its frames shown most recently).
const TEXTURES: usize = 48;

struct Entry {
    load: Arc<Mutex<Load>>,
    /// (frame index, its texture), most recently shown last.
    textures: Vec<(usize, TextureHandle)>,
}

/// The decoded media of the open project's layers (session only).
#[derive(Resource, Default)]
pub struct LayerMedia {
    files: HashMap<String, Entry>,
}

impl LayerMedia {
    /// The preview frames of `path`, starting to load them the first time.
    fn get(&mut self, path: &str) -> Result<Option<Arc<Frames>>, String> {
        let entry = self.files.entry(path.to_string()).or_insert_with(|| {
            let load = Arc::new(Mutex::new(Load::Loading));
            let (l, p) = (load.clone(), PathBuf::from(path));
            std::thread::spawn(move || {
                let result = tt_media::overlay::probe(&p).and_then(|info| tt_media::overlay::decode(&p, &info, tt_media::overlay::fit(&info, PREVIEW_SIDE, PREVIEW_BYTES)));
                *l.lock().expect("media") = match result {
                    Ok(f) => Load::Ready(Arc::new(f)),
                    Err(e) => Load::Failed(format!("{e:#}")),
                };
            });
            Entry { load, textures: Vec::new() }
        });
        match &*entry.load.lock().expect("media") {
            Load::Loading => Ok(None),
            Load::Ready(f) => Ok(Some(f.clone())),
            Load::Failed(e) => Err(e.clone()),
        }
    }

    /// The texture of `path`'s frame showing at `t` seconds.
    pub(crate) fn texture(&mut self, ctx: &egui::Context, path: &str, t: f64) -> Result<Option<TextureHandle>, String> {
        let Some(frames) = self.get(path)? else { return Ok(None) };
        let i = frames.index_at(t);
        let entry = self.files.get_mut(path).expect("loaded");
        if let Some(k) = entry.textures.iter().position(|(j, _)| *j == i) {
            let hit = entry.textures.remove(k);
            let tex = hit.1.clone();
            entry.textures.push(hit);
            return Ok(Some(tex));
        }
        let image = egui::ColorImage::from_rgba_unmultiplied([frames.width as usize, frames.height as usize], &frames.frames[i]);
        let tex = if entry.textures.len() >= TEXTURES {
            // The least recently shown, reused.
            let (_, mut old) = entry.textures.remove(0);
            old.set(image, egui::TextureOptions::LINEAR);
            old
        } else {
            ctx.load_texture(format!("layer:{path}:{i}"), image, egui::TextureOptions::LINEAR)
        };
        entry.textures.push((i, tex.clone()));
        Ok(Some(tex))
    }

    /// Forget `path` (loaded again next time: a file chosen anew with Locate).
    pub fn forget(&mut self, path: &str) {
        self.files.remove(path);
    }
}

/// Every layer on the shown frame `frame` (module docs). `shown`: the view's framing there.
pub fn draw(painter: &Painter, map: &ViewportMapping, world: &mut World, frame: FrameIndex, shown: SpaceMap) {
    let list = layers(world);
    if list.is_empty() {
        return;
    }
    let selected = world.resource::<tt_core::selection::Selection>().entities.clone();
    let ctx = painter.ctx().clone();
    let mut loading = false;
    for e in list {
        let (Some(p), Some(pl)) = (world.get::<LayerParams>(e).cloned(), placed_at(world, e, frame)) else { continue };
        let corners = pl.corners(p.media_size).map(|c| map.to_screen(shown.from_source(c)));
        let tex = world.resource_mut::<LayerMedia>().texture(&ctx, &p.media, pl.clip_time);
        match tex {
            Ok(Some(tex)) => {
                let tint = Color32::from_white_alpha((pl.opacity.clamp(0.0, 1.0) * 255.0).round() as u8);
                let mut mesh = egui::Mesh::with_texture(tex.id());
                for (c, uv) in corners.iter().zip([Pos2::new(0.0, 0.0), Pos2::new(1.0, 0.0), Pos2::new(1.0, 1.0), Pos2::new(0.0, 1.0)]) {
                    mesh.vertices.push(egui::epaint::Vertex { pos: *c, uv, color: tint });
                }
                mesh.indices.extend([0, 1, 2, 0, 2, 3]);
                painter.add(Shape::mesh(mesh));
            }
            Ok(None) => {
                loading = true;
                painter.extend(Shape::dashed_line(&[corners[0], corners[1], corners[2], corners[3], corners[0]], Stroke::new(1.0, style::MUTED), 5.0, 4.0));
            }
            Err(why) => {
                painter.add(Shape::convex_polygon(corners.to_vec(), Color32::from_black_alpha(150), Stroke::new(1.0, style::LOST)));
                let name = Path::new(&p.media).file_name().map_or_else(|| p.media.clone(), |n| n.to_string_lossy().into_owned());
                let missing = if Path::new(&p.media).exists() { "can't read" } else { "not found" };
                let c = Pos2::new(corners.iter().map(|c| c.x).sum::<f32>() / 4.0, corners.iter().map(|c| c.y).sum::<f32>() / 4.0);
                painter.text(c, egui::Align2::CENTER_CENTER, format!("{name}: {missing}"), egui::FontId::proportional(11.0), style::LOST);
                tracing::debug!("layer media {}: {why}", p.media);
            }
        }
        if selected.contains(&e) {
            let ring = [corners[0], corners[1], corners[2], corners[3], corners[0]];
            painter.add(Shape::line(ring.to_vec(), Stroke::new(3.0, Color32::from_black_alpha(140))));
            painter.add(Shape::line(ring.to_vec(), Stroke::new(1.5, style::LAYER)));
            let a = map.to_screen(shown.from_source(pl.at));
            painter.circle_stroke(a, 5.0, Stroke::new(1.5, style::LAYER));
            painter.line_segment([a - egui::vec2(8.0, 0.0), a + egui::vec2(8.0, 0.0)], Stroke::new(1.0, style::LAYER));
            painter.line_segment([a - egui::vec2(0.0, 8.0), a + egui::vec2(0.0, 8.0)], Stroke::new(1.0, style::LAYER));
            // Its handles (the Select tool): a square at each corner scales it; just outside one turns it.
            if world.resource::<tt_core::tool::ActiveTool>().0 == tt_core::tool::Tool::Select {
                for c in corners {
                    let r = egui::Rect::from_center_size(c, egui::vec2(8.0, 8.0));
                    painter.rect_filled(r, 1.0, Color32::from_black_alpha(160));
                    painter.rect_stroke(r, 1.0, Stroke::new(1.5, style::LAYER), egui::StrokeKind::Inside);
                }
                hover_cursor(painter, map, world, e, frame, shown);
            }
        }
    }
    if loading {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
}

/// Over the selected layer's handles, the pointer says what a drag does.
fn hover_cursor(painter: &Painter, map: &ViewportMapping, world: &World, e: Entity, frame: FrameIndex, shown: SpaceMap) {
    use tt_core::layer::Handle;
    let ctx = painter.ctx();
    let Some(pos) = ctx.input(|i| i.pointer.hover_pos()).filter(|p| map.panel.contains(*p)) else { return };
    let src = shown.to_source(map.to_canvas(pos));
    let grab = tt_core::layer::HANDLE_REACH / map.points_per_canvas() * shown.a;
    let alt = ctx.input(|i| i.modifiers.alt);
    let icon = match tt_core::layer::handle_at(world, e, frame, src, grab) {
        Some(Handle::Corner(i)) => {
            if i % 2 == 0 { egui::CursorIcon::ResizeNwSe } else { egui::CursorIcon::ResizeNeSw }
        }
        Some(Handle::Rotate(_)) => egui::CursorIcon::Alias,
        Some(Handle::Body) if alt => egui::CursorIcon::Crosshair,
        Some(Handle::Body) => egui::CursorIcon::Move,
        None => return,
    };
    ctx.set_cursor_icon(icon);
}

/// The parameters of a new layer showing `path` (probed: its size and length).
pub fn params_for(path: &Path) -> Result<LayerParams, String> {
    let info: MediaInfo = tt_media::overlay::probe(path).map_err(|e| format!("{e:#}"))?;
    Ok(LayerParams {
        media: path.display().to_string(),
        media_size: [info.width as f32, info.height as f32],
        clip_duration: info.duration as f32,
        clip_fps: info.fps as f32,
        ..LayerParams::default()
    })
}

/// Attach the file at `path` to `target` on the shown frame (one undo step;
/// the new layer is selected); a file that can't be read says why.
pub fn attach_file(world: &mut World, target: Entity, path: &Path) -> Option<Entity> {
    match params_for(path) {
        Ok(p) => {
            let frame = world.resource::<tt_core::transport::Transport>().frame();
            tt_core::layer::attach(world, target, p, frame)
        }
        Err(e) => {
            world.resource_mut::<crate::media::StatusLine>().0 = Some((format!("Couldn't read {}: {e}", path.display()), true));
            None
        }
    }
}

/// Choose a picture, GIF or clip and attach it to `target`.
pub fn attach_dialog(world: &mut World, target: Entity) {
    let all: Vec<&str> = PICTURES.iter().chain(CLIPS).copied().collect();
    if let Some(path) = rfd::FileDialog::new().set_title("Attach a picture, GIF or clip").add_filter("Pictures, GIFs and clips", &all).add_filter("All files", &["*"]).pick_file() {
        attach_file(world, target, &path);
    }
}

/// Choose the file again for layer `e` (its media moved or was renamed); one undo step.
pub fn locate(world: &mut World, e: Entity) {
    let all: Vec<&str> = PICTURES.iter().chain(CLIPS).copied().collect();
    let Some(path) = rfd::FileDialog::new().set_title("Locate the layer's file").add_filter("Pictures, GIFs and clips", &all).add_filter("All files", &["*"]).pick_file() else { return };
    match params_for(&path) {
        Ok(fresh) => {
            if let Some(old) = world.get::<LayerParams>(e).map(|p| p.media.clone()) {
                world.resource_mut::<LayerMedia>().forget(&old);
            }
            tt_core::layer::set_params(world, e, "Locate the layer's file", |p| {
                p.media = fresh.media;
                p.media_size = fresh.media_size;
                p.clip_duration = fresh.clip_duration;
                p.clip_fps = fresh.clip_fps;
            });
        }
        Err(err) => world.resource_mut::<crate::media::StatusLine>().0 = Some((format!("Couldn't read {}: {err}", path.display()), true)),
    }
}

/// A dropped file that is a picture or a GIF: attached to the selected
/// sketch, tracker or subject. True: it was taken (else it opens as a video).
pub fn drop_file(world: &mut World, path: &Path) -> bool {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if !PICTURES.contains(&ext.as_str()) {
        return false;
    }
    let target = world.resource::<tt_core::selection::Selection>().primary().and_then(|e| tt_core::view::followable(world, e));
    match target {
        Some(t) => {
            attach_file(world, t, path);
        }
        None => {
            world.resource_mut::<crate::media::StatusLine>().0 =
                Some(("To attach a picture, select a sketch, tracker or subject first. Then drop the picture on the window.".into(), true));
        }
    }
    true
}

pub struct LayersModule;

impl tt_core::Module for LayersModule {
    fn build(&self, app: &mut tt_core::AppBuilder) {
        app.declare::<LayerMedia>(tt_core::Class::Session).init_resource::<LayerMedia>();
    }
}
