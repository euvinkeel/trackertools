//! A cursor tracker's patterns (`tt_track::job::cursor`): each its own
//! colour, and what each learned drawn as a tile (its pixels over a
//! checkerboard where it isn't the cursor): in the Inspector, and under the
//! brush while painting, so you see which pattern a paint teaches and what
//! it has learned so far.

use bevy_ecs::prelude::*;
use egui::{Align2, Color32, FontId, Painter, Pos2, Rect, Stroke, StrokeKind, TextureHandle, Vec2};
use tt_track::job::cursor::{PaintSeen, PaintUse, Shape};
use tt_track::runner::CursorShapes;

/// The first ten patterns' colours (keys 1–0): easy to tell apart, none the red of a lost frame.
const COLOURS: [Color32; 10] = [
    Color32::from_rgb(0xa3, 0xe6, 0x35),
    Color32::from_rgb(0xf4, 0x72, 0xb6),
    Color32::from_rgb(0x38, 0xbd, 0xf8),
    Color32::from_rgb(0xfa, 0xcc, 0x15),
    Color32::from_rgb(0xa7, 0x8b, 0xfa),
    Color32::from_rgb(0x34, 0xd3, 0x99),
    Color32::from_rgb(0xe8, 0x79, 0xf9),
    Color32::from_rgb(0xfd, 0xba, 0x74),
    Color32::from_rgb(0x81, 0x8c, 0xf8),
    Color32::from_rgb(0x99, 0xf6, 0xe4),
];

/// Pattern `p`'s colour (from 0).
pub fn colour(p: u32) -> Color32 {
    match COLOURS.get(p as usize) {
        Some(c) => *c,
        // (Past ten: hues a golden angle apart.)
        None => egui::ecolor::Hsva::new((0.13 + p as f32 * 0.381_966) % 1.0, 0.6, 0.95, 1.0).into(),
    }
}

/// What `tracker` learned of pattern `p`, if anything yet.
pub fn shape(world: &World, tracker: Entity, p: u32) -> Option<&Shape> {
    world.get::<CursorShapes>(tracker)?.learned.shapes.iter().find(|s| s.pattern == p)
}

/// A texture of `shape`: its grey, see-through where it isn't the cursor
/// (sharp pixels: it is a few px across). Made again when it changes.
pub fn texture(ctx: &egui::Context, tracker: Entity, shape: &Shape) -> TextureHandle {
    let id = egui::Id::new(("cursor-pattern", tracker, shape.pattern));
    let stamp = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (shape.w, shape.h).hash(&mut h);
        for (v, a) in shape.value.iter().zip(&shape.alpha) {
            (v.to_bits(), a.to_bits()).hash(&mut h);
        }
        h.finish()
    };
    if let Some((s, t)) = ctx.data(|d| d.get_temp::<(u64, TextureHandle)>(id))
        && s == stamp
    {
        return t;
    }
    let rgba: Vec<u8> = shape.value.iter().zip(&shape.alpha).flat_map(|(v, a)| {
        let g = v.clamp(0.0, 255.0) as u8;
        [g, g, g, (a.clamp(0.0, 1.0) * 255.0) as u8]
    }).collect();
    let image = egui::ColorImage::from_rgba_unmultiplied([shape.w, shape.h], &rgba);
    let t = ctx.load_texture(format!("cursor-pattern-{tracker}-{}", shape.pattern), image, egui::TextureOptions::NEAREST);
    ctx.data_mut(|d| d.insert_temp(id, (stamp, t.clone())));
    t
}

/// A pattern's tile in `rect`: what it learned (`learned`: its texture and
/// size) over a checkerboard, framed in its colour (thicker if `active`),
/// its number in the corner. Nothing learned yet: "new", dashed.
pub fn tile(painter: &Painter, rect: Rect, learned: Option<(&TextureHandle, [usize; 2])>, p: u32, active: bool) {
    let c = colour(p);
    // A checkerboard: see-through shows.
    let check = 6.0;
    painter.rect_filled(rect, 3.0, Color32::from_gray(52));
    let (nx, ny) = ((rect.width() / check).ceil() as usize, (rect.height() / check).ceil() as usize);
    for j in 0..ny {
        for i in 0..nx {
            if (i + j) % 2 == 0 {
                let r = Rect::from_min_size(rect.min + Vec2::new(i as f32 * check, j as f32 * check), Vec2::splat(check)).intersect(rect);
                painter.rect_filled(r, 0.0, Color32::from_gray(78));
            }
        }
    }
    match learned {
        Some((t, [w, h])) => {
            // As big as fits, in whole pixels where it can.
            let inner = rect.shrink(4.0);
            let k = (inner.width() / w as f32).min(inner.height() / h as f32);
            let k = if k >= 1.0 { k.floor() } else { k };
            let r = Rect::from_center_size(inner.center(), Vec2::new(w as f32 * k, h as f32 * k));
            painter.image(t.id(), r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
        }
        None => {
            painter.text(rect.center(), Align2::CENTER_CENTER, "new", FontId::proportional(10.0), c);
        }
    }
    painter.rect_stroke(rect, 3.0, Stroke::new(if active { 2.5 } else { 1.0 }, if active { c } else { c.gamma_multiply(0.6) }), StrokeKind::Inside);
    let n = if p < 9 { format!("{}", p + 1) } else if p == 9 { "0".to_string() } else { format!("{}", p + 1) };
    let label = painter.layout_no_wrap(n, FontId::proportional(10.0), Color32::BLACK);
    let badge = Rect::from_min_size(rect.min, label.size() + Vec2::new(6.0, 2.0));
    painter.rect_filled(badge, 3.0, c);
    painter.galley(badge.min + Vec2::new(3.0, 1.0), label, Color32::BLACK);
}

/// Pattern `p`'s key, as shown ("1" … "9", "0", then none).
pub fn key(p: u32) -> Option<String> {
    match p {
        0..=8 => Some(format!("{}", p + 1)),
        9 => Some("0".to_string()),
        _ => None,
    }
}

/// A texture of a paint's picture (`PaintSeen::picture`): its pixels,
/// dimmed where it isn't painted. Made again when it is learned again.
pub fn paint_texture(ctx: &egui::Context, tracker: Entity, seen: &PaintSeen) -> TextureHandle {
    let pic = &seen.picture;
    let id = egui::Id::new(("cursor-paint", tracker, seen.frame, seen.pattern));
    // (A new picture is a new allocation: its address tells it apart.)
    let stamp = (std::sync::Arc::as_ptr(&pic.rgb) as usize, pic.w, pic.h);
    if let Some((s, t)) = ctx.data(|d| d.get_temp::<((usize, usize, usize), TextureHandle)>(id))
        && s == stamp
    {
        return t;
    }
    let rgba: Vec<u8> = (0..pic.w * pic.h)
        .flat_map(|i| {
            let k = if pic.painted.get(i).copied().unwrap_or(false) { 1.0 } else { 0.35 };
            let c = |j: usize| (pic.rgb.get(3 * i + j).copied().unwrap_or(0) as f32 * k) as u8;
            [c(0), c(1), c(2), 255]
        })
        .collect();
    let image = egui::ColorImage::from_rgba_unmultiplied([pic.w.max(1), pic.h.max(1)], &rgba);
    let t = ctx.load_texture(format!("cursor-paint-{tracker}-{}-{}", seen.pattern, seen.frame), image, egui::TextureOptions::NEAREST);
    ctx.data_mut(|d| d.insert_temp(id, (stamp, t.clone())));
    t
}

/// A paint's tile in `rect`: its picture (as big as fits), where its
/// pattern's shape lies in it (`shape`: its size), framed by what became of
/// it: its pattern's colour if learned from (fainter: one say with another
/// paint of the same screen), red if left out.
pub fn paint_tile(painter: &Painter, rect: Rect, tex: &TextureHandle, seen: &PaintSeen, shape: Option<[usize; 2]>, lost: Color32) {
    let pic = &seen.picture;
    let c = colour(seen.pattern);
    painter.rect_filled(rect, 3.0, Color32::from_gray(24));
    let inner = rect.shrink(2.0);
    let k = (inner.width() / pic.w.max(1) as f32).min(inner.height() / pic.h.max(1) as f32);
    let r = Rect::from_center_size(inner.center(), Vec2::new(pic.w as f32 * k, pic.h as f32 * k));
    painter.image(tex.id(), r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
    if let (Some(at), Some([w, h])) = (seen.picture.shape_at, shape) {
        let b = Rect::from_min_size(r.min + Vec2::new(at[0] as f32 * k, at[1] as f32 * k), Vec2::new(w as f32 * k, h as f32 * k));
        painter.rect_stroke(b, 0.0, Stroke::new(1.0, c), StrokeKind::Outside);
    }
    let edge = match seen.used {
        PaintUse::Used => Stroke::new(1.5, c),
        PaintUse::SameAs(_) => Stroke::new(1.0, c.gamma_multiply(0.5)),
        PaintUse::NotLinedUp | PaintUse::LeftOut => Stroke::new(1.5, lost),
    };
    painter.rect_stroke(rect, 3.0, edge, StrokeKind::Inside);
}

/// What became of a paint, in words (its tile's tooltip).
pub fn paint_words(seen: &PaintSeen) -> String {
    match seen.used {
        PaintUse::Used => format!("Frame {}: learned from", seen.frame),
        PaintUse::SameAs(f) => format!("Frame {}: the same still screen as the paint on frame {f}, so the two count as one", seen.frame),
        PaintUse::NotLinedUp => format!(
            "Frame {}: left out. Nothing near its middle lines up with the other paints. Is the cursor in it, and about centred?",
            seen.frame
        ),
        PaintUse::LeftOut => format!(
            "Frame {}: left out. It lines up, but doesn't look like what the other paints agree on. Another shape of the cursor? Paint it as its own pattern (Shift+brush)",
            seen.frame
        ),
    }
}
