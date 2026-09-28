//! Brush: how the next stroke behaves, set before drawing (like a paint
//! program's brush settings), and the box settings of the sketch it goes
//! into. Sizes are in the pixels you draw on.

use bevy_ecs::prelude::*;
use tt_core::autospeed::AutoSpeed;
use tt_core::capture::{SCALE_RANGE, SketchDefaults, WheelMode};
use tt_core::history::{History, edit};
use tt_core::selection::Selection;
use tt_core::sketch::{SketchParams, sketch_of};
use tt_core::transport::Transport;
use tt_core::view::{ActiveView, map_at};

use crate::style;

pub fn ui(ui: &mut egui::Ui, world: &mut World) {
    egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
        next_stroke(ui, world);
        ui.separator();
        box_settings(ui, world);
    });
}

fn note(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).color(style::MUTED).small());
}

fn next_stroke(ui: &mut egui::Ui, world: &mut World) {
    let mut d = world.resource::<SketchDefaults>().clone();
    let before = d.clone();
    ui.heading("Next stroke");
    note(
        ui,
        "Hold on the video to record. Paused, a hold retakes that frame: it goes where your mouse is, and the box re-forms around it. \
         The arrow keys while holding retake the next frame; Space plays and retakes as it goes.",
    );
    egui::Grid::new("brush-stroke").num_columns(2).show(ui, |ui| {
        ui.label("Size");
        ui.add(egui::DragValue::new(&mut d.stroke.scale).range(SCALE_RANGE.0..=SCALE_RANGE.1).speed(0.01).prefix("×"))
            .on_hover_text("Multiplies the box your hand makes: padding, jiggle and smallest box.");
        ui.end_row();
        ui.label("Falloff");
        ui.add(egui::DragValue::new(&mut d.stroke.falloff).range(0.0..=5.0).speed(0.01).suffix(" s"))
            .on_hover_text("0: only the frames you touch change (a retake).\nMore: frames beside them are pulled along too, fading out over this long (seconds of video).");
        ui.end_row();
    });
    ui.label("The mouse wheel while holding");
    ui.horizontal_wrapped(|ui| {
        ui.radio_value(&mut d.wheel, WheelMode::Still, "does nothing: the view holds still").on_hover_text("A stray scroll while you follow the subject doesn't zoom the view under your hand.");
        ui.radio_value(&mut d.wheel, WheelMode::Zoom, "zooms the view");
        ui.radio_value(&mut d.wheel, WheelMode::Size, "sets the size");
        ui.radio_value(&mut d.wheel, WheelMode::Falloff, "the falloff");
        ui.radio_value(&mut d.wheel, WheelMode::Both, "both");
    });
    if d.stroke != before.stroke || d.wheel != before.wheel {
        *world.resource_mut::<SketchDefaults>() = d;
    }
    let mut auto = world.resource::<AutoSpeed>().enabled;
    if ui.checkbox(&mut auto, "Anticipatory speed").on_hover_text("While you hold and the video plays, it slows down when things get busy and speeds up when they're calm. Its knobs are in Settings.").changed() {
        world.resource_mut::<AutoSpeed>().enabled = auto;
    }
}

/// The box numbers of the sketch the next stroke goes into (the selected
/// one), or of new sketches.
fn box_settings(ui: &mut egui::Ui, world: &mut World) {
    let primary = world.resource::<Selection>().primary();
    let target = primary.and_then(|e| sketch_of(world, e));
    let name = target.and_then(|e| world.get::<bevy_ecs::name::Name>(e)).map(|n| n.to_string());
    let current = match target {
        Some(e) => world.get::<SketchParams>(e).cloned().unwrap_or_default(),
        None => world.resource::<SketchDefaults>().params.clone(),
    };
    let mut p = current.clone();
    ui.heading(match &name {
        Some(n) => format!("Box of {n}"),
        None => "Box of new sketches".to_string(),
    });
    note(ui, "Half the box = your hand's jiggle × 2.2 × jiggle gain + padding, then clamped to at least half the smallest box (per axis).");
    let mut drag = (false, false);
    let mut track = |r: egui::Response| {
        drag.0 |= r.drag_started();
        drag.1 |= r.drag_stopped();
    };
    egui::Grid::new("brush-box").num_columns(2).show(ui, |ui| {
        ui.label("Padding");
        track(ui.add(egui::DragValue::new(&mut p.pad).range(0.0..=500.0).speed(0.2).suffix(" px")).on_hover_text("Added around the jiggle, on every side."));
        ui.end_row();
        ui.label("Smallest box");
        let (mut w, mut h) = (2.0 * p.min_half, 2.0 * p.min_half_y);
        ui.horizontal(|ui| {
            let tip = "A clamp: the box is never narrower or shorter than this, however still your hand. (Padding adds; this only sets a floor.)";
            track(ui.add(egui::DragValue::new(&mut w).range(2.0..=4000.0).speed(0.5).suffix(" px")).on_hover_text(tip));
            ui.label("×");
            track(ui.add(egui::DragValue::new(&mut h).range(2.0..=4000.0).speed(0.5).suffix(" px")).on_hover_text(tip));
        });
        (p.min_half, p.min_half_y) = (w / 2.0, h / 2.0);
        ui.end_row();
        ui.label("Jiggle gain");
        track(ui.add(egui::DragValue::new(&mut p.gain).range(0.0..=10.0).speed(0.01).prefix("×")).on_hover_text("How much your hand's jiggle grows the box."));
        ui.end_row();
        ui.label("Hand lag");
        track(ui.add(egui::DragValue::new(&mut p.lag).range(0.0..=2.0).speed(0.005).suffix(" s")).on_hover_text("How far (real seconds) your hand trails what it follows. New strokes take this; each stroke keeps its own (Inspector)."));
        ui.end_row();
    });
    let size = |j: f32| (2.0 * (p.gain * 2.2 * j + p.pad).max(p.min_half), 2.0 * (p.gain * 2.2 * j + p.pad).max(p.min_half_y));
    let (still, jiggly) = (size(0.0), size(5.0));
    note(ui, &format!("A still hand: a {:.0}×{:.0} px box. A hand jiggling ±5 px: {:.0}×{:.0} px. (Before the stroke's size ×.)", still.0, still.1, jiggly.0, jiggly.1));
    note(ui, &units(world));
    if target.is_none() {
        ui.horizontal_wrapped(|ui| {
            ui.label("Presets");
            for preset_name in SketchParams::PRESETS {
                let preset = SketchParams::preset(preset_name).expect("listed preset");
                if ui.selectable_label(p == preset, preset_name).clicked() {
                    p = preset;
                }
            }
        });
    }

    if p != current {
        match target {
            Some(e) => {
                // A drag is one undo step.
                if drag.0 {
                    world.resource_mut::<History>().begin(format!("Box of {}", name.as_deref().unwrap_or("sketch")));
                }
                edit(world, "Box settings", |tx| tx.insert(e, p.clone()));
            }
            None => world.resource_mut::<SketchDefaults>().params = p,
        }
    }
    if drag.1 && target.is_some() && world.resource::<History>().in_gesture() {
        world.resource_mut::<History>().end();
    }
    if let Some(e) = target {
        let is_default = world.resource::<SketchDefaults>().params == world.get::<SketchParams>(e).cloned().unwrap_or_default();
        if ui.add_enabled(!is_default, egui::Button::new("Use for new sketches").small()).clicked() {
            world.resource_mut::<SketchDefaults>().params = world.get::<SketchParams>(e).cloned().unwrap_or_default();
        }
    }
}

/// What "px" means where you're drawing.
fn units(world: &World) -> String {
    let view = world.resource::<ActiveView>().0;
    let frame = world.resource::<Transport>().frame();
    match view {
        None => "px = video pixels (you're drawing on the source).".to_string(),
        Some(_) => {
            let a = map_at(world, view, frame).a;
            format!("px = this view's pixels (you're drawing inside a view): here 1 px = {a:.2} video px.")
        }
    }
}
