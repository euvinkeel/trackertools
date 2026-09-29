//! The Look editor (DESIGN §6.3): a tracker's look, magnified, to paint which
//! pixels are the subject (its mask). Left paints, right erases; a drag is
//! one undo step. *Auto* marks the cells that differ from the rectangle's
//! border (a cursor on a plain background); *Clear* goes back to
//! centre-weighting.

use bevy_ecs::prelude::*;
use egui::{Color32, Rect, Sense, Stroke, StrokeKind, Vec2};
use tt_core::history::{History, edit};
use tt_core::input::{Action, PendingActions};
use tt_core::transport::Transport;
use tt_track::look::{Look, MASK_N};

use crate::media::Media;
use crate::style;

/// Painting state (session): the brush radius in mask cells.
#[derive(Resource, Debug, Clone, Copy)]
pub struct LookBrush {
    pub radius: f32,
}

impl Default for LookBrush {
    fn default() -> Self {
        Self { radius: 1.2 }
    }
}

/// RGB of source pixel `(x, y)` from an NV12 frame of `w × h` (BT.709, limited range).
fn rgb(frame: &[u8], w: usize, h: usize, x: usize, y: usize) -> Color32 {
    let (x, y) = (x.min(w - 1), y.min(h - 1));
    let luma = frame[y * w + x] as f32;
    let row = 2 * w.div_ceil(2);
    let uv = w * h + (y / 2) * row + (x / 2) * 2;
    let (u, v) = (frame.get(uv).copied().unwrap_or(128) as f32 - 128.0, frame.get(uv + 1).copied().unwrap_or(128) as f32 - 128.0);
    let c = 1.164 * (luma - 16.0);
    let px = |v: f32| v.round().clamp(0.0, 255.0) as u8;
    Color32::from_rgb(px(c + 1.793 * v), px(c - 0.213 * u - 0.533 * v), px(c + 2.112 * u))
}

/// The look's rectangle sampled on the mask grid (nearest source pixel per cell), if its frame is decoded.
fn cells(world: &World, look: &Look) -> Option<Vec<Color32>> {
    let media = world.get_resource::<Media>()?;
    let index = media.index();
    let frame = media.original.player.frame(media.presented(look.frame))?;
    let (w, h) = (index.width as usize, index.height as usize);
    let [l, t, r, b] = [look.rect()[2], look.rect()[3], look.rect()[4], look.rect()[5]];
    Some(
        (0..MASK_N * MASK_N)
            .map(|k| {
                let (i, j) = ((k % MASK_N) as f64, (k / MASK_N) as f64);
                let x = l + (i + 0.5) / MASK_N as f64 * (r - l);
                let y = t + (j + 0.5) / MASK_N as f64 * (b - t);
                rgb(&frame, w, h, x.max(0.0) as usize, y.max(0.0) as usize)
            })
            .collect(),
    )
}

/// A new look's mask, painted automatically (`tt_track::look::LookMasker`):
/// the cells that differ from its rectangle's border. None when its frame
/// isn't decoded or too little stands out (it stays centre-weighted).
pub fn auto_mask_look(world: &World, look: &Look) -> Option<Vec<u8>> {
    let mask = auto_mask(&cells(world, look)?);
    let on = mask.iter().filter(|c| **c > 0).count();
    (on >= 6 && on < mask.len() * 9 / 10).then_some(mask)
}

/// Cells that differ from the rectangle's border colour: the subject on a plain background.
fn auto_mask(colors: &[Color32]) -> Vec<u8> {
    let border: Vec<Color32> = (0..MASK_N * MASK_N).filter(|k| k % MASK_N == 0 || k % MASK_N == MASK_N - 1 || k / MASK_N == 0 || k / MASK_N == MASK_N - 1).map(|k| colors[k]).collect();
    let median = |f: fn(&Color32) -> u8| {
        let mut v: Vec<u8> = border.iter().map(f).collect();
        v.sort_unstable();
        v[v.len() / 2] as f32
    };
    let bg = [median(|c| c.r()), median(|c| c.g()), median(|c| c.b())];
    colors
        .iter()
        .map(|c| {
            let d = ((c.r() as f32 - bg[0]).powi(2) + (c.g() as f32 - bg[1]).powi(2) + (c.b() as f32 - bg[2]).powi(2)).sqrt();
            if d > 40.0 { 255 } else { 0 }
        })
        .collect()
}

/// The editor for look `e`.
pub fn ui(ui: &mut egui::Ui, world: &mut World, e: Entity) {
    let Some(look) = world.get::<Look>(e).cloned() else { return };
    ui.label(egui::RichText::new(format!("Frame {} · {:.0}×{:.0} px around ({:.0}, {:.0})", look.frame, 2.0 * look.half_w, 2.0 * look.half_h, look.x, look.y)).color(style::MUTED));
    let Some(colors) = cells(world, &look) else {
        ui.horizontal(|ui| {
            ui.label("Its frame isn't decoded.");
            if ui.button(format!("Go to frame {}", look.frame)).clicked() {
                world.resource_mut::<PendingActions>().push(Action::Seek(look.frame));
            }
        });
        return;
    };
    let mut mask = if look.mask.len() == MASK_N * MASK_N { look.mask.clone() } else { vec![0; MASK_N * MASK_N] };
    let painted = look.painted().is_some();
    ui.label(
        egui::RichText::new(if painted { "Only the painted cells count as the subject." } else { "Nothing painted: the centre counts most (paint to say exactly which pixels are the subject)." })
            .color(style::MUTED)
            .small(),
    );

    // The grid, as big as the panel allows, keeping the rectangle's aspect.
    let aspect = look.half_w / look.half_h.max(0.01);
    let side = ui.available_width().min(360.0);
    let size = if aspect >= 1.0 { Vec2::new(side, side / aspect) } else { Vec2::new(side * aspect, side) };
    let (response, painter) = ui.allocate_painter(size, Sense::click_and_drag());
    let rect = response.rect;
    let cell = Vec2::new(rect.width() / MASK_N as f32, rect.height() / MASK_N as f32);
    for k in 0..MASK_N * MASK_N {
        let (i, j) = ((k % MASK_N) as f32, (k / MASK_N) as f32);
        let r = Rect::from_min_size(rect.min + Vec2::new(i * cell.x, j * cell.y), cell);
        let c = colors[k];
        let shown = if painted && mask[k] == 0 { c.gamma_multiply(0.3) } else { c };
        painter.rect_filled(r, 0.0, shown);
        if mask[k] > 0 {
            painter.rect_stroke(r.shrink(0.5), 0.0, Stroke::new(1.0, super::tracks::TRACK.gamma_multiply(0.7)), StrokeKind::Inside);
        }
    }
    painter.rect_stroke(rect, 0.0, Stroke::new(1.0, style::MUTED), StrokeKind::Outside);

    // Paint: left on, right off, a round brush in cells.
    let radius = world.resource::<LookBrush>().radius;
    let mut changed = false;
    let pointer = ui.input(|i| (i.pointer.primary_down(), i.pointer.secondary_down()));
    if let Some(pos) = response.hover_pos() {
        painter.circle_stroke(pos, radius * cell.x.max(cell.y), Stroke::new(1.0, Color32::WHITE));
        if response.is_pointer_button_down_on() && (pointer.0 || pointer.1) {
            let value = if pointer.0 { 255 } else { 0 };
            let (ci, cj) = ((pos.x - rect.min.x) / cell.x, (pos.y - rect.min.y) / cell.y);
            for (k, cell) in mask.iter_mut().enumerate() {
                let (i, j) = ((k % MASK_N) as f32 + 0.5, (k / MASK_N) as f32 + 0.5);
                if (i - ci).hypot(j - cj) <= radius && *cell != value {
                    *cell = value;
                    changed = true;
                }
            }
        }
    }
    // A paint drag is one undo step.
    if response.drag_started() {
        world.resource_mut::<History>().begin("Paint look mask");
    }

    let mut set: Option<(Vec<u8>, &str)> = changed.then(|| (mask.clone(), "Paint look mask"));
    ui.horizontal(|ui| {
        let mut r = radius;
        if ui.add(egui::Slider::new(&mut r, 0.5..=6.0).text("brush")).changed() {
            world.resource_mut::<LookBrush>().radius = r;
        }
    });
    ui.horizontal(|ui| {
        if ui.button("Auto").on_hover_text("Mark the cells whose colour differs from the rectangle's border: the subject on a plain background").clicked() {
            set = Some((auto_mask(&colors), "Auto mask"));
        }
        if ui.button("Fill").clicked() {
            set = Some((vec![255; MASK_N * MASK_N], "Fill mask"));
        }
        if ui.button("Invert").clicked() {
            set = Some((mask.iter().map(|c| if *c > 0 { 0 } else { 255 }).collect(), "Invert mask"));
        }
        if ui.add_enabled(painted, egui::Button::new("Clear")).on_hover_text("Back to centre-weighting").clicked() {
            set = Some((Vec::new(), "Clear mask"));
        }
    });
    if let Some((m, label)) = set {
        edit(world, label, |tx| tx.modify::<Look>(e, |l| l.mask = m));
    }
    if response.drag_stopped() && world.resource::<History>().in_gesture() {
        world.resource_mut::<History>().end();
    }
    if world.resource::<Transport>().frame() != look.frame {
        ui.label(egui::RichText::new(format!("(the look is on frame {}; the viewport shows another)", look.frame)).weak().small());
    }
}
