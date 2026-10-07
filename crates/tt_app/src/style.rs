//! Visual constants. Canvas-drawn panels use these instead of literals so the
//! palette stays in one place (v1 duplicated CSS variables in JS).
//!
//! **The visual language** (DESIGN §14): a hue says where data came from,
//! the same in the viewport, the timeline, the outliner and the inspector.
//! - [`AUTO`] (cyan): computed by an algorithm: a tracker's automatic results.
//! - [`HAND`] (orange): drawn by a person: sketches, a tracker's human layer
//!   (drawn frames over its automatic results), manual dots.
//! - [`PIN`] (white): what a person told an algorithm: a template tracker's
//!   looks (rectangles), a CoTracker's reset points (diamonds).
//! - [`SUBJECT`] (purple): subjects, carried by their members.
//! - [`VIEW`] (blue): views (a sketch's framing).
//! - [`LOST`] (red): frames not to trust (lost, outside the sketch), errors.
//! - [`LIVE`] (amber): being recorded now; warnings.
//! - [`FOCUS`] (periwinkle): SpringFocus, moving from one tracked thing to the next.
//!
//! Solid and bright: valid and selected. Dim: stale (being recomputed) or
//! not selected. Dashed and light: a preview of what a press would make.
//! [`ACCENT`] (the same cyan) is the interface's own: buttons, the playhead.

use egui::Color32;

pub const BG: Color32 = Color32::from_rgb(0x0d, 0x0f, 0x13);
pub const PANEL: Color32 = Color32::from_rgb(0x15, 0x18, 0x1e);
pub const RULER: Color32 = Color32::from_rgb(0x2a, 0x30, 0x3b);
pub const TICK: Color32 = Color32::from_rgb(0x5b, 0x65, 0x75);
pub const TEXT: Color32 = Color32::from_rgb(0xd8, 0xde, 0xe9);
pub const MUTED: Color32 = Color32::from_rgb(0x7d, 0x87, 0x96);
pub const ACCENT: Color32 = Color32::from_rgb(0x22, 0xd3, 0xee);
/// The in and out points (what an export covers): its handles, bar and badges.
pub const RANGE: Color32 = Color32::from_rgb(0xe2, 0xe8, 0xf0);

/// Computed by an algorithm.
pub const AUTO: Color32 = Color32::from_rgb(0x22, 0xd3, 0xee);
/// Drawn by a person.
pub const HAND: Color32 = Color32::from_rgb(0xfb, 0x92, 0x3c);
/// What a person told an algorithm (looks, reset points).
pub const PIN: Color32 = Color32::from_rgb(0xf8, 0xfa, 0xfc);
pub const SUBJECT: Color32 = Color32::from_rgb(0xc0, 0x84, 0xfc);
pub const VIEW: Color32 = Color32::from_rgb(0x60, 0xa5, 0xfa);
pub const LOST: Color32 = Color32::from_rgb(0xf4, 0x3f, 0x5e);
pub const LIVE: Color32 = Color32::from_rgb(0xfb, 0xbf, 0x24);
/// SpringFocus: one point moving from one tracked thing to the next.
pub const FOCUS: Color32 = Color32::from_rgb(0xa5, 0xb4, 0xfc);

pub fn apply(ctx: &egui::Context) {
    ctx.set_visuals(egui::Visuals::dark());
}
