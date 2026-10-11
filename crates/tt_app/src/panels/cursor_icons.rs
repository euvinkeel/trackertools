//! A template tracker's cursor icons in the Inspector (`tt_track::icons`):
//! add a pack found on this computer, see its icons, set its size in the
//! video (or let the tracker find it from its looks), remove it.

use bevy_ecs::prelude::*;
use egui::{Color32, Rect, Stroke, StrokeKind, TextureHandle, Vec2};
use tt_core::history::{History, edit};
use tt_track::icons::{CursorIcons, Icon, IconFit, PackSize, packs};

use crate::style;

/// An icon's texture (its own pixels; sharp: it is a few px across).
fn texture(ctx: &egui::Context, icon: &Icon) -> TextureHandle {
    let id = egui::Id::new(("cursor-icon", &icon.pack, &icon.name, icon.w, icon.h));
    if let Some(t) = ctx.data(|d| d.get_temp::<TextureHandle>(id)) {
        return t;
    }
    let image = egui::ColorImage::from_rgba_unmultiplied([icon.w as usize, icon.h as usize], &icon.rgba);
    let t = ctx.load_texture(format!("cursor-icon-{}-{}", icon.pack, icon.name), image, egui::TextureOptions::NEAREST);
    ctx.data_mut(|d| d.insert_temp(id, t.clone()));
    t
}

/// A tile: the icon over a checkerboard, as big as fits.
fn tile(ui: &mut egui::Ui, icon: &Icon, side: f32) -> egui::Response {
    let (rect, r) = ui.allocate_exact_size(Vec2::splat(side), egui::Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 2.0, Color32::from_gray(58));
    let half = side / 2.0;
    for (dx, dy) in [(0.0, 0.0), (half, half)] {
        p.rect_filled(Rect::from_min_size(rect.min + Vec2::new(dx, dy), Vec2::splat(half)), 0.0, Color32::from_gray(84));
    }
    let k = ((side - 4.0) / icon.w.max(icon.h) as f32).min(2.0);
    let r2 = Rect::from_center_size(rect.center(), Vec2::new(icon.w as f32 * k, icon.h as f32 * k));
    p.image(texture(ui.ctx(), icon).id(), r2, Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
    p.rect_stroke(rect, 2.0, Stroke::new(1.0, Color32::from_gray(100)), StrokeKind::Inside);
    r.on_hover_text(format!("{} {}: {}\u{d7}{} px, its point at {:.0}, {:.0}", icon.pack, icon.name, icon.w, icon.h, icon.hotspot[0], icon.hotspot[1]))
}

/// Tracker `e`'s cursor icons: add a pack, and each pack's icons and size.
pub fn section(ui: &mut egui::Ui, world: &mut World, e: Entity) {
    let current = world.get::<CursorIcons>(e).cloned().unwrap_or_default();
    let fit = world.get::<IconFit>(e).cloned();
    let added = current.packs();
    ui.horizontal_wrapped(|ui| {
        ui.label("Cursor icons:");
        let found = packs();
        if found.is_empty() {
            ui.label(egui::RichText::new("none found on this computer (Windows' cursors, or a Roblox install)").small().color(style::MUTED));
        }
        for pack in found {
            if added.iter().any(|p| p == pack.name) {
                continue;
            }
            let names: Vec<&str> = pack.icons.iter().map(|i| i.name.as_str()).collect();
            let tip = format!(
                "Add {}'s cursors from this computer ({}): {}. The tracker matches each as one more look, so it keeps following the cursor when it turns into one of them. Add only the pack your video shows: one arrow looks like another.",
                pack.name,
                pack.source.display(),
                names.join(", ")
            );
            if ui.button(format!("+ {} cursors", pack.name)).on_hover_text(tip).clicked() {
                let mut next = current.clone();
                next.icons.extend(pack.icons.iter().cloned());
                edit(world, &format!("Add {} cursor icons", pack.name), |tx| tx.insert(e, next));
            }
        }
    });
    for pack in &added {
        let icons: Vec<&Icon> = current.icons.iter().filter(|i| &i.pack == pack).collect();
        let pfit = fit.as_ref().and_then(|f| f.packs.iter().find(|p| &p.pack == pack));
        let set = current.size_of(pack);
        // Its name, how its size is found, and remove.
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new(pack.as_str()).strong());
            let mut auto = set.is_none();
            if ui.checkbox(&mut auto, "auto size").on_hover_text("Find the size from the tracker's looks (where one of them shows this pack's cursor)").changed() {
                let mut next = current.clone();
                next.sizes.retain(|s| &s.pack != pack);
                if !auto {
                    next.sizes.push(PackSize { pack: pack.clone(), size: pfit.map_or(1.0, |f| f.size) });
                }
                edit(world, "Edit cursor icon size", |tx| tx.insert(e, next));
            }
            if ui.small_button("Remove").on_hover_text(format!("Remove {pack}'s icons from this tracker")).clicked() {
                let mut next = current.clone();
                next.icons.retain(|i| &i.pack != pack);
                next.sizes.retain(|s| &s.pack != pack);
                edit(world, &format!("Remove {pack} cursor icons"), |tx| tx.insert(e, next));
            }
        });
        // Its icons.
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            for i in &icons {
                tile(ui, i, 22.0);
            }
        });
        // Its size, set by hand: a row of its own, as wide as the panel.
        if let Some(size) = set {
            ui.horizontal(|ui| {
                let mut v = size;
                ui.spacing_mut().slider_width = (ui.available_width() - 70.0).clamp(40.0, 220.0);
                let r = ui
                    .add(egui::Slider::new(&mut v, 0.2..=3.0).logarithmic(true).fixed_decimals(2).suffix("\u{d7}"))
                    .on_hover_text("Video px per icon px: the size the cursor has in the video (screen scaling, the recording's size)");
                if r.drag_started() {
                    world.resource_mut::<History>().begin("Edit cursor icon size");
                }
                if r.changed() {
                    let mut next = current.clone();
                    if let Some(s) = next.sizes.iter_mut().find(|s| &s.pack == pack) {
                        s.size = v;
                    }
                    edit(world, "Edit cursor icon size", |tx| tx.insert(e, next));
                }
                if r.drag_stopped() {
                    world.resource_mut::<History>().end();
                }
            });
        }
        // What the tracker made of it.
        let words = match pfit {
            Some(f) if f.auto && f.matched => (format!("{:.2}\u{d7}, found from the looks", f.size), style::MUTED),
            Some(f) if f.auto => ("not found from the looks: none of them shows this pack's cursor. Untick auto and set the size".to_string(), style::LIVE),
            Some(f) if !f.matched => (format!("{:.2}\u{d7}: matches none of the looks there", f.size), style::LIVE),
            Some(f) => (format!("{:.2}\u{d7}, matches the looks", f.size), style::MUTED),
            None => ("used when it tracks next".to_string(), style::MUTED),
        };
        ui.label(egui::RichText::new(words.0).small().color(words.1));
    }
    if let Some(n) = fit.as_ref().and_then(|f| f.lined_up.as_ref()).filter(|_| !added.is_empty()) {
        ui.label(egui::RichText::new(format!("Their point lines up with the look's: {n}")).small().color(style::MUTED));
    }
}
