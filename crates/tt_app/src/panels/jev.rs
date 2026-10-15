//! A Jev tracker's part of the Inspector (dev only; `tt_track::job::jev`):
//! the API key when there is none, its settings (applied with a button: each
//! change is new requests, paid), Ask Jev (it starts paused and pauses again
//! when done), and what Jev answered.

use bevy_ecs::prelude::*;
use tt_core::input::{Action, PendingActions};
use tt_core::transport::Transport;
use tt_track::job::jev::{self, Answer};
use tt_track::look::{Look, looks_of};
use tt_track::runner::{JevAnswers, settled};
use tt_track::{JevExamples, JevSettings, TrackRun, Tracker};

use crate::style;

/// $ per input token (output is free): $0.042 per million.
const PRICE_PER_TOKEN: f64 = 0.042e-6;

/// What the section keeps between frames (egui's memory, per tracker).
#[derive(Clone, Default)]
struct Draft {
    /// The settings as edited, not applied yet.
    settings: Option<JevSettings>,
    /// The key as typed (never shown), and why saving it failed.
    key: String,
    key_error: Option<String>,
    /// Whether there is a key (asked once: on a Mac, `security` runs).
    has_key: Option<bool>,
}

pub fn section(ui: &mut egui::Ui, world: &mut World, e: Entity) {
    let Some(tracker) = world.get::<Tracker>(e).cloned() else { return };
    let id = egui::Id::new(("jev", e));
    let mut d: Draft = ui.ctx().data_mut(|m| m.get_temp(id)).unwrap_or_default();
    let has_key = *d.has_key.get_or_insert_with(|| jev::api_key().is_some());

    ui.separator();
    ui.label(egui::RichText::new("Jev (dev only, experimental)").strong());
    ui.label(
        egui::RichText::new(format!(
            "TypeSafe's Jev ({}) is asked where the painted subject is, on every Nth frame after the last paint. It gets each paint and each frame as a crop that follows the sketch. Each frame is one request over the network.",
            jev::MODEL
        ))
        .small()
        .color(style::MUTED),
    );

    // The API key.
    if !has_key {
        ui.horizontal_wrapped(|ui| {
            ui.label("TypeSafe API key:");
            ui.add(egui::TextEdit::singleline(&mut d.key).password(true).desired_width(220.0).hint_text("paste the key"));
            if ui.add_enabled(!d.key.trim().is_empty(), egui::Button::new("Save key")).on_hover_text(format!("Keep it in {}", jev::key_place())).clicked() {
                match jev::save_api_key(&d.key) {
                    Ok(()) => {
                        d.key.clear();
                        d.key_error = None;
                        d.has_key = Some(true);
                    }
                    Err(err) => d.key_error = Some(format!("{err:#}")),
                }
            }
        });
        if let Some(err) = &d.key_error {
            ui.colored_label(style::LOST, err);
        }
        ui.label(egui::RichText::new("Or set TYPESAFE_API_KEY before you start trackertools.").small().color(style::MUTED));
    }

    // Its settings: edited here, applied with a button.
    let mut s = d.settings.clone().unwrap_or_else(|| tracker.jev.clone());
    ui.horizontal_wrapped(|ui| {
        ui.label("Every");
        ui.add(egui::DragValue::new(&mut s.every).range(1..=600).suffix(" frames"));
        ui.label("\u{b7}");
        ui.add(egui::DragValue::new(&mut s.frames).range(1..=500).suffix(" frames in all"));
    });
    ui.horizontal_wrapped(|ui| {
        ui.label("Examples:");
        for (style, text, tip) in [
            (JevExamples::Dimmed, "rest darkened", "The paint's whole crop, what isn't painted darkened"),
            (JevExamples::Outlined, "outlined", "The paint's whole crop, the painted area outlined in magenta"),
            (JevExamples::Tight, "tight", "Only around the painted area, scaled up"),
        ] {
            ui.radio_value(&mut s.examples, style, text).on_hover_text(tip);
        }
    });
    let changed = s != tracker.jev;
    d.settings = changed.then(|| s.clone());
    if changed {
        ui.horizontal(|ui| {
            if ui.button("Apply").on_hover_text("Its results so far are for the old settings: Ask Jev asks again with these").clicked() {
                tt_core::history::edit(world, "Edit Jev settings", |tx| tx.modify::<Tracker>(e, |t| t.jev = s.clone()));
                d.settings = None;
            }
            if ui.button("Cancel").clicked() {
                d.settings = None;
            }
        });
    }

    // What it would ask about.
    let paints: Vec<i64> = looks_of(world, e).iter().filter_map(|l| world.get::<Look>(*l)).filter(|l| l.mask.iter().any(|c| *c > 0)).map(|l| l.frame).collect();
    let last = paints.iter().max().copied();
    let s = &tracker.jev;
    match last {
        None => {
            ui.label(egui::RichText::new("Track tool: brush over the subject on 3 to 5 frames first (each paint is an example).").color(style::MUTED));
        }
        Some(last) => {
            let (first, end) = (last + s.every as i64, last + s.every as i64 * s.frames as i64);
            ui.label(egui::RichText::new(format!("{} paints \u{b7} it asks about frames {first}\u{2013}{end}, every {} (as far as the sketch goes): {} requests", paints.len(), s.every, s.frames)).small());
        }
    }

    // Ask, or what it is doing.
    let run = tt_track::run_of(world, e);
    let busy = run != TrackRun::Paused && !settled(world, e);
    // Asked with nothing left to ask (no job, so no "finished"): paused again, as after a run.
    if run != TrackRun::Paused && !busy && world.resource::<tt_track::runner::TrackJobs>().threads() == 0 {
        tt_track::set_run(world, e, TrackRun::Paused);
    }
    let answers: Vec<Answer> = world.get::<JevAnswers>(e).map(|a| a.0.values().cloned().collect()).unwrap_or_default();
    ui.horizontal(|ui| {
        if busy {
            ui.spinner();
            ui.label(format!("Asking Jev\u{2026} {} of {} answered", answers.len(), s.frames));
            if ui.button("Stop").clicked() {
                tt_track::set_run(world, e, TrackRun::Paused);
            }
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
        } else {
            let ready = has_key && last.is_some() && !changed;
            let tip = if !has_key {
                "Save the API key first"
            } else if last.is_none() {
                "Paint over the subject first"
            } else if changed {
                "Apply or cancel the settings first"
            } else {
                "Send the requests (paid). It pauses again when done: new paints or settings don't ask by themselves"
            };
            let label = if answers.is_empty() { "Ask Jev" } else { "Ask Jev again" };
            if ui.add_enabled(ready, egui::Button::new(egui::RichText::new(label).strong())).on_hover_text(tip).on_disabled_hover_text(tip).clicked() {
                // Asked again: every frame again (results that still stand would otherwise be kept).
                if !answers.is_empty() {
                    let frames = tt_core::op::extent(world);
                    world.resource_mut::<tt_core::op::Invalidations>().recompute(e, frames);
                    world.entity_mut(e).remove::<JevAnswers>();
                }
                tt_track::set_run(world, e, TrackRun::Forward);
            }
        }
    });

    // What Jev answered.
    if !answers.is_empty() {
        let n = answers.len();
        let failed: Vec<&Answer> = answers.iter().filter(|a| a.error.is_some()).collect();
        let ok: Vec<&Answer> = answers.iter().filter(|a| a.error.is_none()).collect();
        let seen = ok.iter().filter(|a| a.visible >= 0.5 && a.view != "not_in_view").count();
        let tokens: u64 = answers.iter().map(|a| a.tokens).sum();
        let mean = |f: &dyn Fn(&Answer) -> f64| if ok.is_empty() { 0.0 } else { ok.iter().map(|a| f(a)).sum::<f64>() / ok.len() as f64 };
        let mut ms: Vec<u32> = ok.iter().map(|a| a.ms).collect();
        ms.sort_unstable();
        let median_ms = ms.get(ms.len() / 2).copied().unwrap_or(0);
        ui.label(format!(
            "{n} answered \u{b7} seen on {seen} \u{b7} box confidence {:.2} on average \u{b7} {median_ms} ms a request (median) \u{b7} {tokens} tokens (${:.4}){}",
            mean(&|a| a.confidence as f64),
            tokens as f64 * PRICE_PER_TOKEN,
            if failed.is_empty() { String::new() } else { format!(" \u{b7} {} failed", failed.len()) }
        ));
        if let Some(a) = failed.first() {
            ui.colored_label(style::LOST, format!("Frame {}: {}", a.frame, a.error.as_deref().unwrap_or_default()));
        }
        // This frame's answer, then every frame's.
        let here = world.resource::<Transport>().frame();
        if let Some(a) = answers.iter().find(|a| a.frame == here) {
            ui.label(egui::RichText::new(format!("This frame: {}", describe(a))).color(style::ACCENT));
        }
        egui::CollapsingHeader::new(format!("Every answer ({n})")).id_salt(("jev-answers", e)).show(ui, |ui| {
            egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                for a in &answers {
                    if ui.selectable_label(a.frame == here, format!("{} \u{b7} {}", a.frame, describe(a))).on_hover_text("Go to this frame").clicked() {
                        world.resource_mut::<PendingActions>().push(Action::Seek(a.frame));
                    }
                }
            });
        });
    }
    ui.ctx().data_mut(|m| m.insert_temp(id, d));
}

/// An answer in a line.
fn describe(a: &Answer) -> String {
    if let Some(e) = &a.error {
        return format!("failed: {e}");
    }
    let view = match a.view.as_str() {
        "visible" => "visible",
        "partly_hidden" => "partly hidden",
        "not_in_view" => "not in view",
        other => other,
    };
    format!("{view} \u{b7} seen {:.2} \u{b7} box {:.2} \u{b7} {} ms \u{b7} {} tokens", a.visible, a.confidence, a.ms, a.tokens)
}
