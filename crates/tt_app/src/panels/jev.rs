//! A Jev tracker's part of the Inspector, and the Jev viewer (dev only;
//! `tt_track::job::jev`).
//!
//! - **Inspector:** the API key when there is none (looked for again every
//!   few seconds: one added in Terminal shows), its settings (applied with
//!   a button: each change is new requests, paid), Ask Jev, a summary, and
//!   Open the Jev viewer.
//! - **The Jev viewer** (a window, as big as it is dragged): every frame Jev
//!   was asked about as the crop it was sent, Jev's box drawn on it in its
//!   confidence's colour, what it said under it, and Go to / Retry; the
//!   examples it was given; Ask again, Retry failed, Retry the unsure.
//!
//! A Jev tracker starts paused and pauses again when a job ends (the
//! runner), so nothing asks (and pays) by itself.

use bevy_ecs::prelude::*;
use egui::{Color32, Rect, Stroke, StrokeKind, TextureHandle, Vec2};
use tt_core::input::{Action, PendingActions};
use tt_core::transport::Transport;
use tt_track::job::jev::{self, Answer};
use tt_track::look::{Look, looks_of};
use tt_track::runner::{JevAnswers, settled};
use tt_track::{JevExamples, JevSettings, TrackRun, Tracker};

use crate::style;

/// $ per input token (output is free): $0.042 per million.
const PRICE_PER_TOKEN: f64 = 0.042e-6;
/// The key is looked for again this often (s): `security` runs each time on a Mac.
const KEY_RECHECK: f64 = 3.0;
/// Box confidence at or above which a kept answer shows as sure (else in between).
const SURE: f32 = 0.5;

/// What the section keeps between frames (egui's memory, per tracker).
#[derive(Clone, Default)]
struct Draft {
    /// The settings as edited, not applied yet.
    settings: Option<JevSettings>,
    /// The key as typed (never shown), and why saving it failed.
    key: String,
    key_error: Option<String>,
    /// The range to ask about, as set here.
    range: Option<(i64, i64)>,
}

/// Whether there is an API key, looked for at most every [`KEY_RECHECK`] s.
fn has_key(ctx: &egui::Context, force: bool) -> bool {
    let id = egui::Id::new("jev-has-key");
    let now = ctx.input(|i| i.time);
    let cached: Option<(f64, bool)> = ctx.data(|d| d.get_temp(id));
    match cached {
        Some((at, has)) if !force && now - at < KEY_RECHECK => has,
        _ => {
            let has = jev::api_key().is_some();
            ctx.data_mut(|d| d.insert_temp(id, (now, has)));
            has
        }
    }
}

/// The tracker the viewer shows, if it is open.
fn viewer_id() -> egui::Id {
    egui::Id::new("jev-viewer")
}

fn open_viewer(ctx: &egui::Context, e: Entity) {
    ctx.data_mut(|d| d.insert_temp(viewer_id(), Some(e)));
}

/// Its paints' frames (painted looks).
fn paints(world: &World, e: Entity) -> Vec<i64> {
    looks_of(world, e).iter().filter_map(|l| world.get::<Look>(*l)).filter(|l| l.mask.iter().any(|c| *c > 0)).map(|l| l.frame).collect()
}

fn answers(world: &World, e: Entity) -> Vec<Answer> {
    world.get::<JevAnswers>(e).map(|a| a.answers.values().cloned().collect()).unwrap_or_default()
}

/// Asking now (a job running, or about to).
fn busy(world: &World, e: Entity) -> bool {
    tt_track::run_of(world, e) != TrackRun::Paused && !settled(world, e)
}

pub fn section(ui: &mut egui::Ui, world: &mut World, e: Entity) {
    let Some(tracker) = world.get::<Tracker>(e).cloned() else { return };
    let min = tracker.jev.min_confidence;
    let id = egui::Id::new(("jev", e));
    let mut d: Draft = ui.ctx().data_mut(|m| m.get_temp(id)).unwrap_or_default();
    let key = has_key(ui.ctx(), false);
    // (The scene demo that asks shows the viewer, once.)
    let shown = egui::Id::new(("jev-scene-viewer", e));
    if std::env::var_os("TT_SCENE_JEV_ASK").is_some() && std::env::var("TT_SCENE_JEV_VIEWER").as_deref() != Ok("0") && !ui.ctx().data(|m| m.get_temp::<bool>(shown).unwrap_or(false)) {
        ui.ctx().data_mut(|m| m.insert_temp(shown, true));
        open_viewer(ui.ctx(), e);
    }

    ui.separator();
    ui.label(egui::RichText::new("Jev (dev only, experimental)").strong());
    ui.label(
        egui::RichText::new(format!(
            "TypeSafe's Jev ({}) is asked where the painted subject is, on every Nth frame counted from the first paint, in the range you ask about. It sees only the sketch's box. Each frame asked is one request over the network.",
            jev::MODEL
        ))
        .small()
        .color(style::MUTED),
    );

    // The API key.
    if !key {
        ui.horizontal_wrapped(|ui| {
            ui.label("TypeSafe API key:");
            ui.add(egui::TextEdit::singleline(&mut d.key).password(true).desired_width(220.0).hint_text("paste the key"));
            if ui.add_enabled(!d.key.trim().is_empty(), egui::Button::new("Save key")).on_hover_text(format!("Keep it in {}", jev::key_place())).clicked() {
                match jev::save_api_key(&d.key) {
                    Ok(()) => {
                        d.key.clear();
                        d.key_error = None;
                        has_key(ui.ctx(), true);
                    }
                    Err(err) => d.key_error = Some(format!("{err:#}")),
                }
            }
        });
        if let Some(err) = &d.key_error {
            ui.colored_label(style::LOST, err);
        }
        ui.label(egui::RichText::new("Or add it in Terminal (security add-generic-password -U -s trackertools-typesafe -a api -w), or set TYPESAFE_API_KEY.").small().color(style::MUTED));
    }

    // What each answer depends on: edited here, applied with a button (the answers so far are then for the old settings).
    let mut s = d.settings.clone().unwrap_or_else(|| tracker.jev.clone());
    ui.horizontal_wrapped(|ui| {
        ui.label("Every");
        ui.add(egui::DragValue::new(&mut s.every).range(1..=600).suffix(" frames")).on_hover_text("The grid: every Nth frame, counted from the first paint, both ways");
        ui.label("\u{b7} examples:");
        for (style, text, tip) in [
            (JevExamples::Tight, "tight", "Only around the painted area, scaled up (the default: the closest, and the cheapest)"),
            (JevExamples::Dimmed, "darkened", "The paint's whole crop, what isn't painted darkened"),
            (JevExamples::Outlined, "outlined", "The paint's whole crop, the painted area outlined in magenta"),
        ] {
            ui.radio_value(&mut s.examples, style, text).on_hover_text(tip);
        }
    });
    ui.checkbox(&mut s.fill, "Fill between answers").on_hover_text("Frames between two answers (or a paint and an answer) on the straight line between them, lost if either is. Off: left out");
    // (These apply at once, elsewhere: they ask nothing again.)
    (s.min_confidence, s.frames, s.ask_from, s.ask_to) = (tracker.jev.min_confidence, tracker.jev.frames, tracker.jev.ask_from, tracker.jev.ask_to);
    let changed = s != tracker.jev;
    d.settings = changed.then(|| s.clone());
    if changed {
        ui.horizontal(|ui| {
            if ui.button("Apply").on_hover_text("Its answers so far are for the old settings: ask again with these").clicked() {
                tt_core::history::edit(world, "Edit Jev settings", |tx| tx.modify::<Tracker>(e, |t| t.jev = s.clone()));
                d.settings = None;
            }
            if ui.button("Cancel").clicked() {
                d.settings = None;
            }
        });
    }
    min_slider(ui, world, e, min);

    // Ask: a range, what it covers, and the button.
    let painted = paints(world, e);
    let got = answers(world, e);
    let (lo, hi) = span(world, e);
    let here = world.resource::<Transport>().frame();
    let (mut from, mut to) = d.range.unwrap_or_else(|| last_range(world, e));
    egui::Frame::group(ui.style()).inner_margin(6.0).show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new("Ask frames").strong());
            ui.add(egui::DragValue::new(&mut from).range(lo..=hi));
            ui.label("\u{2013}");
            ui.add(egui::DragValue::new(&mut to).range(lo..=hi));
        });
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            if ui.small_button("Whole sketch").on_hover_text("Every frame the tracker can reach").clicked() {
                (from, to) = (lo, hi);
            }
            let marks = tt_core::marks::marks(world);
            if marks.is_set() {
                let r = marks.frames(world.resource::<Transport>().frame_count);
                if ui.small_button("In\u{2013}Out").on_hover_text(format!("The timeline's in and out points: {}\u{2013}{}", r.start, r.end - 1)).clicked() {
                    (from, to) = (r.start.max(lo), (r.end - 1).min(hi));
                }
            }
            if ui.small_button("From here").on_hover_text("From the playhead to the end of the sketch").clicked() {
                (from, to) = (here.clamp(lo, hi), hi);
            }
            if ui.small_button("\u{b1}60 here").on_hover_text("60 frames either side of the playhead").clicked() {
                (from, to) = ((here - 60).max(lo), (here + 60).min(hi));
            }
        });
        let (a, b) = (from.min(to), from.max(to));
        let first = painted.iter().min().copied();
        let grid = first.map_or_else(Vec::new, |anchor| jev::grid(anchor, tracker.jev.every, &painted, a, b));
        let n = grid.len().min(tracker.jev.frames as usize);
        let per = if got.is_empty() { 2_000.0 } else { got.iter().map(|x| x.tokens as f64).sum::<f64>() / got.len() as f64 };
        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new(format!("{} frames on the grid (every {}) \u{b7} about ${:.4}", grid.len(), tracker.jev.every, n as f64 * per * PRICE_PER_TOKEN)).small(),
            );
            ui.label(egui::RichText::new("\u{b7} at most").small());
            let mut cap = tracker.jev.frames;
            if ui.add(egui::DragValue::new(&mut cap).range(1..=2000).suffix(" an ask")).on_hover_text("A cap on the requests one ask sends (the frames nearest the first paint first)").changed() {
                tt_core::history::edit(world, "Edit Jev's cap", |tx| tx.modify::<Tracker>(e, |t| t.jev.frames = cap));
            }
        });
        let ready = key && first.is_some() && !changed && n > 0;
        let why_not = if !key {
            "Save the API key first"
        } else if first.is_none() {
            "Track tool: brush over the subject on 3 to 5 frames first (each paint is an example)"
        } else if changed {
            "Apply or cancel the settings first"
        } else {
            "No frame of the grid in that range"
        };
        ui.horizontal_wrapped(|ui| {
            if busy(world, e) {
                busy_row(ui, world, e, &got);
            } else if ui
                .add_enabled(ready, egui::Button::new(egui::RichText::new(format!("\u{25B6} Ask Jev about these {n} frames")).strong()))
                .on_hover_text("Send the requests (paid): the grid's frames in the range, asked again if they were. Answers elsewhere stay. It pauses again when done")
                .on_disabled_hover_text(why_not)
                .clicked()
            {
                jev::ask(world, e, a, b);
                open_viewer(ui.ctx(), e);
            }
            retry_buttons(ui, world, e, &got, key && !changed);
        });
    });
    d.range = Some((from, to));

    if ui.button("\u{1F50D} Open the Jev viewer").on_hover_text("Every frame Jev was asked about, as it was sent, with Jev's box on it").clicked() {
        open_viewer(ui.ctx(), e);
    }
    if !got.is_empty() {
        ui.label(summary(&got, min));
        if let Some(a) = got.iter().find(|a| a.frame == here) {
            ui.label(egui::RichText::new(format!("This frame: {}", describe(a))).color(colour(a, min)));
        }
    }
    ui.ctx().data_mut(|m| m.insert_temp(id, d));
}

/// The frames tracker `e` can reach (its sketch's), both included.
fn span(world: &World, e: Entity) -> (i64, i64) {
    world.get::<tt_core::span::Reach>(e).map_or((0, world.resource::<Transport>().frame_count.max(1) - 1), |r| (r.0, r.1))
}

/// The range it was last asked about, else its whole span.
fn last_range(world: &World, e: Entity) -> (i64, i64) {
    match world.get::<Tracker>(e).map(|t| (t.jev.ask_from, t.jev.ask_to)) {
        Some((a, b)) if b >= a => (a, b),
        _ => span(world, e),
    }
}

/// Asking now: how far, and Stop.
fn busy_row(ui: &mut egui::Ui, world: &mut World, e: Entity, got: &[Answer]) {
    ui.spinner();
    ui.label(format!("Asking Jev\u{2026} {} answered", got.len()));
    if ui.button("Stop").on_hover_text("Requests already sent still finish (and are paid)").clicked() {
        tt_track::set_run(world, e, TrackRun::Paused);
    }
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
}

/// Retry failed, Retry the lost (below the least confidence).
fn retry_buttons(ui: &mut egui::Ui, world: &mut World, e: Entity, got: &[Answer], ready: bool) {
    if busy(world, e) {
        return;
    }
    let failed: Vec<i64> = got.iter().filter(|a| a.error.is_some()).map(|a| a.frame).collect();
    if !failed.is_empty() && ui.add_enabled(ready, egui::Button::new(format!("Retry failed ({})", failed.len()))).clicked() {
        jev::retry(world, e, &failed);
    }
    let min = world.get::<Tracker>(e).map_or(0.0, |t| t.jev.min_confidence);
    let lost: Vec<i64> = got.iter().filter(|a| a.error.is_none() && a.confidence < min).map(|a| a.frame).collect();
    if !lost.is_empty()
        && ui
            .add_enabled(ready, egui::Button::new(format!("Retry the lost ({})", lost.len())))
            .on_hover_text(format!("Ask again about the frames where the box's confidence is under {min:.2} (Lost below)"))
            .clicked()
    {
        jev::retry(world, e, &lost);
    }
}

/// The viewer's: Ask again (the last range), the retries, or how far it is.
fn controls(ui: &mut egui::Ui, world: &mut World, e: Entity, got: &[Answer], ready: bool, why_not: &str) {
    if busy(world, e) {
        busy_row(ui, world, e, got);
        return;
    }
    let (a, b) = last_range(world, e);
    if ui
        .add_enabled(ready, egui::Button::new(egui::RichText::new(format!("\u{21BB} Ask again: frames {a}\u{2013}{b}")).strong()))
        .on_hover_text("The range last asked about, again (paid). Another range: the tracker's Inspector")
        .on_disabled_hover_text(why_not)
        .clicked()
    {
        jev::ask(world, e, a, b);
    }
    retry_buttons(ui, world, e, got, ready);
}

/// The answers in a line: how many, sure, times, cost.
fn summary(got: &[Answer], min: f32) -> String {
    let ok: Vec<&Answer> = got.iter().filter(|a| a.error.is_none()).collect();
    let failed = got.len() - ok.len();
    let kept = ok.iter().filter(|a| a.confidence >= min).count();
    let tokens: u64 = got.iter().map(|a| a.tokens).sum();
    let mut ms: Vec<u32> = ok.iter().map(|a| a.ms).collect();
    ms.sort_unstable();
    let mean = if ok.is_empty() { 0.0 } else { ok.iter().map(|a| a.confidence as f64).sum::<f64>() / ok.len() as f64 };
    format!(
        "{} answered \u{b7} {kept} kept (box confidence \u{2265} {min:.2}), {} lost \u{b7} confidence {mean:.2} on average \u{b7} {} ms a request (median) \u{b7} {tokens} tokens (${:.4}){}",
        got.len(),
        ok.len() - kept,
        ms.get(ms.len() / 2).copied().unwrap_or(0),
        tokens as f64 * PRICE_PER_TOKEN,
        if failed > 0 { format!(" \u{b7} {failed} failed") } else { String::new() }
    )
}

/// An answer in a line.
fn describe(a: &Answer) -> String {
    if let Some(e) = &a.error {
        return format!("failed: {e}");
    }
    format!("box {:.2} \u{b7} {} \u{b7} seen {:.2} \u{b7} {} ms \u{b7} {} tokens", a.confidence, view_words(&a.view), a.visible, a.ms, a.tokens)
}

fn view_words(v: &str) -> &str {
    match v {
        "visible" => "visible",
        "partly_hidden" => "partly hidden",
        "not_in_view" => "not in view",
        other => other,
    }
}

/// An answer's colour: lost (below `min`, red, as failed), kept and sure (the accent), kept in between (amber).
fn colour(a: &Answer, min: f32) -> Color32 {
    if a.error.is_some() || a.confidence < min {
        style::LOST
    } else if a.confidence < SURE.max(min) {
        Color32::from_rgb(0xf5, 0xb0, 0x3a)
    } else {
        style::ACCENT
    }
}

/// A PNG as a texture, cached by `key` (and the image it is).
fn texture(ctx: &egui::Context, key: impl std::hash::Hash + std::fmt::Debug, png: &std::sync::Arc<Vec<u8>>) -> Option<TextureHandle> {
    let id = egui::Id::new(("jev-image", key, std::sync::Arc::as_ptr(png) as usize));
    if let Some(t) = ctx.data(|d| d.get_temp::<TextureHandle>(id)) {
        return Some(t);
    }
    let (w, h, rgba) = tt_track::icons::read_png(png).ok()?;
    let t = ctx.load_texture(format!("jev-{id:?}"), egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba), egui::TextureOptions::LINEAR);
    ctx.data_mut(|d| d.insert_temp(id, t.clone()));
    Some(t)
}

/// The Jev viewer window (open from a Jev tracker's Inspector).
pub fn viewer(ctx: &egui::Context, world: &mut World) {
    let Some(e) = ctx.data(|d| d.get_temp::<Option<Entity>>(viewer_id())).flatten() else { return };
    let Some(tracker) = world.get::<Tracker>(e).cloned().filter(|t| t.method == tt_track::Method::Jev) else {
        ctx.data_mut(|d| d.insert_temp::<Option<Entity>>(viewer_id(), None));
        return;
    };
    let name = world.get::<bevy_ecs::name::Name>(e).map_or("a Jev tracker".to_string(), |n| n.to_string());
    let min = tracker.jev.min_confidence;
    let mut open = true;
    let size_id = egui::Id::new("jev-viewer-card");
    let mut card: f32 = ctx.data(|d| d.get_temp(size_id)).unwrap_or(240.0);
    let zoom_id = egui::Id::new("jev-viewer-zoom");
    let mut zoom: bool = ctx.data(|d| d.get_temp(zoom_id)).unwrap_or(true);
    egui::Window::new(format!("Jev viewer \u{2014} {name}"))
        .id(egui::Id::new("jev-viewer-window"))
        .open(&mut open)
        .default_size([1100.0, 760.0])
        .resizable(true)
        .collapsible(true)
        .show(ctx, |ui| {
            let got = answers(world, e);
            let key = has_key(ctx, false);
            let last = paints(world, e).into_iter().max();
            let ready = key && last.is_some();
            let why_not = if !key { "Save the API key first (the tracker's Inspector)" } else { "Paint over the subject first" };

            // What it asks, and the buttons.
            let s = &tracker.jev;
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new(format!("{} \u{b7} every {} frames, {} in all \u{b7} {:?} examples", jev::MODEL, s.every, s.frames, s.examples)).color(style::MUTED));
            });
            ui.horizontal_wrapped(|ui| {
                controls(ui, world, e, &got, ready, why_not);
                ui.separator();
                ui.label("Card size");
                ui.add(egui::Slider::new(&mut card, 140.0..=520.0).show_value(false));
                ui.separator();
                min_slider(ui, world, e, min);
                ui.separator();
                ui.checkbox(&mut zoom, "Zoom to the box").on_hover_text("Show each crop around Jev's box (3 times its size) instead of the whole crop");
            });
            if !got.is_empty() {
                ui.label(summary(&got, min));
            }
            ui.horizontal_wrapped(|ui| {
                for (c, t) in [
                    (style::ACCENT, format!("kept, sure (box confidence \u{2265} {SURE})")),
                    (Color32::from_rgb(0xf5, 0xb0, 0x3a), format!("kept ({min:.2}\u{2013}{SURE})")),
                    (style::LOST, format!("lost (below {min:.2}) or failed")),
                ] {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(12.0), egui::Sense::hover());
                    ui.painter().rect_stroke(r.shrink(1.0), 2.0, Stroke::new(2.0, c), StrokeKind::Middle);
                    ui.label(egui::RichText::new(t).small());
                }
                ui.label(egui::RichText::new("\u{b7} \"seen\" (can it be seen) is not reliable yet: Jev says about 0.8 even when the subject is out of the picture; trust the box's confidence.").small().color(style::MUTED));
            });
            ui.separator();

            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                // What Jev was shown.
                let examples = world.get::<JevAnswers>(e).map(|a| a.examples.clone()).unwrap_or_default();
                if !examples.is_empty() {
                    ui.label(egui::RichText::new(format!("The examples Jev was given ({})", examples.len())).strong());
                    ui.horizontal_wrapped(|ui| {
                        for (i, ex) in examples.iter().enumerate() {
                            if let Some(t) = texture(ctx, ("ex", e, i), ex) {
                                let sz = t.size_vec2();
                                let k = (card * 0.6 / sz.x.max(sz.y)).min(2.0);
                                ui.image((t.id(), sz * k));
                            }
                        }
                    });
                    ui.add_space(6.0);
                }
                if got.is_empty() {
                    ui.label(egui::RichText::new(if ready { "No answers yet: click Ask Jev." } else { why_not }).color(style::MUTED));
                    return;
                }
                ui.label(egui::RichText::new(format!("Every frame Jev was asked about ({}): the crop it was sent, its box on it", got.len())).strong());
                let here = world.resource::<Transport>().frame();
                let mut seek = None;
                let mut retry = None;
                let cols = ((ui.available_width() + 8.0) / (card + 8.0)).floor().max(1.0) as usize;
                for row in got.chunks(cols) {
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Min), |ui| {
                    for a in row {
                        ui.allocate_ui_with_layout(Vec2::new(card, card * 0.75 + 64.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                            egui::Frame::new()
                                .stroke(Stroke::new(if a.frame == here { 2.0 } else { 1.0 }, if a.frame == here { style::TEXT } else { colour(a, min).gamma_multiply(0.6) }))
                                .corner_radius(4.0)
                                .inner_margin(4.0)
                                .show(ui, |ui| {
                                    ui.set_width(card - 10.0);
                                    let w = card - 10.0;
                                    let h = w * 0.75;
                                    let (rect, r) = ui.allocate_exact_size(Vec2::new(w, h), egui::Sense::click());
                                    // The part of the crop shown (fractions): all of it, or around the box, the crop's shape.
                                    let uv = if zoom && a.error.is_none() {
                                        let [l, t, rr, b] = a.edges;
                                        let c = egui::pos2((l + rr) / 2.0, (t + b) / 2.0);
                                        let half_w = ((rr - l) * 1.5).max((b - t) * 1.5 * 0.75).max(0.06);
                                        let half = Vec2::new(half_w, half_w / 0.75);
                                        let r = Rect::from_center_size(c, half * 2.0);
                                        r.translate(Vec2::new((-r.min.x).max(0.0) - (r.max.x - 1.0).max(0.0), (-r.min.y).max(0.0) - (r.max.y - 1.0).max(0.0)))
                                    } else {
                                        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0))
                                    };
                                    match a.image.as_ref().and_then(|img| texture(ctx, ("frame", e, a.frame), img)) {
                                        Some(t) => {
                                            ui.painter().image(t.id(), rect, uv, Color32::WHITE);
                                        }
                                        None => {
                                            ui.painter().rect_filled(rect, 2.0, Color32::from_gray(30));
                                        }
                                    }
                                    if a.error.is_none() {
                                        let [l, t, rr, b] = a.edges;
                                        let to = |x: f32, y: f32| rect.min + Vec2::new((x - uv.min.x) / uv.width() * w, (y - uv.min.y) / uv.height() * h);
                                        let bx = Rect::from_min_max(to(l, t), to(rr, b));
                                        ui.painter().rect_stroke(bx, 1.0, Stroke::new(2.0, colour(a, min)), StrokeKind::Middle);
                                        ui.painter().circle_filled(bx.center(), 2.5, colour(a, min));
                                    }
                                    if r.on_hover_text("Go to this frame").clicked() {
                                        seek = Some(a.frame);
                                    }
                                    ui.label(egui::RichText::new(format!("frame {}", a.frame)).strong().color(colour(a, min)));
                                    let line = match &a.error {
                                        Some(err) => format!("failed: {err}"),
                                        None => format!("box {:.2} \u{b7} {} \u{b7} seen {:.2}\n{} ms \u{b7} {} tokens", a.confidence, view_words(&a.view), a.visible, a.ms, a.tokens),
                                    };
                                    ui.add(egui::Label::new(egui::RichText::new(line).small()).wrap());
                                    ui.horizontal(|ui| {
                                        if ui.small_button("Go to").clicked() {
                                            seek = Some(a.frame);
                                        }
                                        if ui.add_enabled(ready && !busy(world, e), egui::Button::new("Retry").small()).on_hover_text("Ask Jev about this frame again (paid)").on_disabled_hover_text(why_not).clicked() {
                                            retry = Some(a.frame);
                                        }
                                    });
                                });
                        });
                    }
                    });
                    ui.add_space(6.0);
                }
                if let Some(f) = seek {
                    world.resource_mut::<PendingActions>().push(Action::Seek(f));
                }
                if let Some(f) = retry {
                    jev::retry(world, e, &[f]);
                }
            });
        });
    ctx.data_mut(|d| {
        d.insert_temp(size_id, card);
        d.insert_temp(zoom_id, zoom);
    });
    if !open {
        ctx.data_mut(|d| d.insert_temp::<Option<Entity>>(viewer_id(), None));
    }
}

/// The least box confidence a frame needs (else it is lost): applied at once,
/// as the output is composed, asking nothing again. One undo step a drag.
fn min_slider(ui: &mut egui::Ui, world: &mut World, e: Entity, min: f32) {
    ui.horizontal(|ui| {
        ui.label("Lost below");
        let mut v = min;
        let r = ui
            .add(egui::Slider::new(&mut v, 0.0..=1.0).step_by(0.01).fixed_decimals(2).text("box confidence"))
            .on_hover_text("Jev's boxes it is less sure of than this count as lost (red; left out of what follows the tracker). Moving it asks nothing again: the frames change at once");
        if r.drag_started() {
            world.resource_mut::<tt_core::history::History>().begin("Edit Jev's least confidence".to_string());
        }
        if r.changed() {
            tt_core::history::edit(world, "Edit Jev's least confidence", |tx| tx.modify::<Tracker>(e, |t| t.jev.min_confidence = v));
        }
        if r.drag_stopped() {
            world.resource_mut::<tt_core::history::History>().end();
        }
    });
}
