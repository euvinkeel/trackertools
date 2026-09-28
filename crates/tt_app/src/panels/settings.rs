//! Settings: how the tools behave (remembered between launches, in the
//! session file) and every key, so nothing has to be memorized.

use bevy_ecs::prelude::*;
use tt_core::capture::{SCALE_RANGE, SketchDefaults, WheelMode};
use tt_core::input::{Action, Keymap};
use tt_core::sketch::SketchParams;
use tt_core::view::ViewDefaults;

use crate::style;

pub fn ui(ui: &mut egui::Ui, world: &mut World) {
    egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
        let mut d = world.resource::<SketchDefaults>().clone();
        let before = d.clone();

        ui.heading("Sketching");
        ui.label("The mouse wheel while holding a stroke changes");
        ui.radio_value(&mut d.wheel, WheelMode::Size, "the box's size");
        ui.radio_value(&mut d.wheel, WheelMode::Falloff, "the falloff: how far neighbouring frames follow the stroke");
        ui.radio_value(&mut d.wheel, WheelMode::Both, "both together, as one \"roughness\" knob");
        ui.add_space(6.0);
        ui.label("The next stroke starts with (each stroke keeps what it ended with)");
        egui::Grid::new("stroke-defaults").num_columns(2).show(ui, |ui| {
            ui.label("size");
            ui.add(egui::DragValue::new(&mut d.stroke.scale).range(SCALE_RANGE.0..=SCALE_RANGE.1).speed(0.01).prefix("×"));
            ui.end_row();
            ui.label("falloff");
            ui.add(egui::DragValue::new(&mut d.stroke.falloff).range(0.0..=5.0).speed(0.01).suffix(" s"));
            ui.end_row();
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label("New sketches use");
            for name in SketchParams::PRESETS {
                let preset = SketchParams::preset(name).expect("listed preset");
                if ui.selectable_label(d.params == preset, name).clicked() {
                    d.params = preset;
                }
            }
        });
        if d.params != SketchParams::default() && SketchParams::PRESETS.iter().all(|n| SketchParams::preset(n).as_ref() != Some(&d.params)) {
            ui.label(egui::RichText::new("(custom, from a sketch's \"Use for new sketches\")").weak().small());
        }
        if d.wheel != before.wheel || d.stroke != before.stroke || d.params != before.params {
            *world.resource_mut::<SketchDefaults>() = d;
        }

        ui.separator();
        ui.heading("Views");
        let mut lock = world.resource::<ViewDefaults>().params.lock_zoom;
        if ui
            .checkbox(&mut lock, "New views keep a steady zoom (the widest the sketch needs)")
            .on_hover_text("Off: the view zooms with the region, smoothed. Each view has its own \"lock zoom\" in the Inspector.")
            .changed()
        {
            world.resource_mut::<ViewDefaults>().params.lock_zoom = lock;
        }

        ui.separator();
        ui.heading("Keys");
        let keymap = world.resource::<Keymap>();
        egui::Grid::new("keys").num_columns(2).striped(true).show(ui, |ui| {
            for (binding, action) in &keymap.bindings {
                ui.monospace(binding.chord());
                ui.label(describe(*action));
                ui.end_row();
            }
        });
        ui.label(
            egui::RichText::new("With the mouse: hold on the video to sketch (Sketch tool) · click a box to select · right-click for commands · Ctrl+hold: move only · Shift+hold: new sketch · wheel/middle-drag to zoom and pan")
                .color(style::MUTED)
                .small(),
        );

        ui.separator();
        ui.heading("Files");
        let dir = tt_media::proxy::data_dir();
        ui.label(egui::RichText::new(dir.display().to_string()).monospace().small());
        ui.label(egui::RichText::new("Projects (autosaved per video), proxies and the session live here.").weak().small());
        if ui.button("Open the folder").clicked() {
            let _ = std::fs::create_dir_all(&dir);
            if let Err(e) = std::process::Command::new("explorer").arg(&dir).spawn() {
                tracing::warn!("could not open {}: {e}", dir.display());
            }
        }
    });
}

/// What an action does, for the key list.
fn describe(action: Action) -> &'static str {
    use Action::*;
    match action {
        Seek(_) => "Go to a frame",
        SetRate(_) => "Set the playback speed",
        ToggleLoop => "Loop on/off",
        TogglePlay => "Play / pause (also while holding a stroke)",
        StepForward => "Next frame",
        StepBackward => "Previous frame",
        JumpForward => "Jump forward 10 frames",
        JumpBackward => "Jump back 10 frames",
        GoToStart => "Go to the start",
        GoToEnd => "Go to the end",
        FasterPlayback => "Faster playback (= capture speed)",
        SlowerPlayback => "Slower playback (= capture speed)",
        FrameAll => "Fit the video in the viewport",
        OpenFile => "Open a video",
        Undo => "Undo",
        Redo => "Redo",
        Tool(_) => "Sketch tool on/off",
        Cancel => "Cancel the stroke, or leave the tool",
        DeselectAll => "Deselect all",
        EnterView => "Enter the selected sketch's view",
        ExitView => "Back out to the parent view",
        Delete => "Delete the selection",
        Duplicate => "Duplicate the selected sketches",
        SelectAll => "Select all sketches",
        Rename => "Rename the selection",
    }
}
