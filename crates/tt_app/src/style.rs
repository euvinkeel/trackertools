//! Visual constants. Canvas-drawn panels use these instead of literals so the
//! palette stays in one place (v1 duplicated CSS variables in JS).

use egui::Color32;

pub const BG: Color32 = Color32::from_rgb(0x0d, 0x0f, 0x13);
pub const PANEL: Color32 = Color32::from_rgb(0x15, 0x18, 0x1e);
pub const RULER: Color32 = Color32::from_rgb(0x2a, 0x30, 0x3b);
pub const TICK: Color32 = Color32::from_rgb(0x5b, 0x65, 0x75);
pub const TEXT: Color32 = Color32::from_rgb(0xd8, 0xde, 0xe9);
pub const MUTED: Color32 = Color32::from_rgb(0x7d, 0x87, 0x96);
pub const ACCENT: Color32 = Color32::from_rgb(0x22, 0xd3, 0xee);

pub fn apply(ctx: &egui::Context) {
    ctx.set_visuals(egui::Visuals::dark());
}
