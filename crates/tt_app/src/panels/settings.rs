//! Settings: how the tools behave (remembered between launches, in the
//! session file) and every key, so nothing has to be memorized.

use bevy_ecs::prelude::*;
use tt_core::autospeed::{AutoSpeed, Foresight};
use tt_core::input::{Action, Keymap};
use tt_core::view::ViewDefaults;

use super::viewport::PointerView;
use crate::style;

pub fn ui(ui: &mut egui::Ui, world: &mut World) {
    egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
        ui.heading("Sketching");
        ui.label(egui::RichText::new("The next stroke's size, falloff, the wheel and the box are in the Brush tab.").color(style::MUTED).small());
        ui.add_space(6.0);
        ui.label("While holding a stroke");
        let mut pv = world.resource::<PointerView>().clone();
        ui.checkbox(&mut pv.hide_pointer, "Hide the pointer while holding a stroke");
        ui.horizontal(|ui| {
            ui.label("Clear window around the pointer");
            ui.add(egui::DragValue::new(&mut pv.clear_radius).range(0.0..=200.0).speed(0.5).suffix(" pt"))
                .on_hover_text("The video inside this radius is shown raw: no boxes, trails or text over it. 0 = off.");
        });
        if pv != *world.resource::<PointerView>() {
            *world.resource_mut::<PointerView>() = pv;
        }

        ui.separator();
        auto_speed(ui, world);

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

/// Anticipatory speed: the switch and its knobs (tt_core::autospeed).
fn auto_speed(ui: &mut egui::Ui, world: &mut World) {
    let mut a = world.resource::<AutoSpeed>().clone();
    ui.heading("Anticipatory speed");
    ui.checkbox(&mut a.enabled, "Anticipatory speed: set the playback speed for me while I hold a stroke").on_hover_text(
        "While you hold a stroke with the video playing, the speed follows the subject: slower when your hand has to move fast or starts to jiggle, \
         or before a stretch that was fast or erratic in the parent sketch (the one whose view you're drawing in); faster through still parts. \
         Q/E during a stroke takes the speed back until you release.",
    );
    ui.add_enabled_ui(a.enabled, |ui| {
        egui::Grid::new("auto-speed").num_columns(2).show(ui, |ui| {
            let row = |ui: &mut egui::Ui, label: &str, tip: &str, value: &mut f32, range: std::ops::RangeInclusive<f32>, speed: f64, prefix: &str, suffix: &str| {
                ui.label(label).on_hover_text(tip);
                ui.add(egui::DragValue::new(value).range(range).speed(speed).prefix(prefix).suffix(suffix)).on_hover_text(tip);
                ui.end_row();
            };
            row(ui, "slowest", "The slowest it goes.", &mut a.slowest, 0.02..=1.0, 0.005, "×", "");
            row(ui, "fastest", "The fastest it goes, through still parts.", &mut a.fastest, 0.5..=4.0, 0.01, "×", "");
            row(ui, "comfortable hand speed", "The fastest your hand should have to move: a subject faster than this on screen slows playback until it isn't.", &mut a.comfort, 20.0..=3000.0, 2.0, "", " pt/s");
            row(ui, "jiggle sensitivity", "How strongly the box growing past its recent calm size (you started to jiggle: the subject turned erratic) slows playback. 0 ignores it; 1 halves the speed at three times its calm size (up to 1.5× is ignored).", &mut a.jiggle, 0.0..=4.0, 0.01, "", "");
            row(ui, "slow down within", "How quickly it slows down: short, so it brakes in time.", &mut a.slow_down, 0.01..=2.0, 0.005, "", " s");
            row(ui, "speed up within", "How quickly it speeds back up: long, so it doesn't lurch.", &mut a.speed_up, 0.05..=10.0, 0.01, "", " s");
            row(ui, "look ahead", "How far ahead the foresight sketch is read (video time), so playback slows before a fast or erratic stretch arrives. 0 = off.", &mut a.look_ahead, 0.0..=5.0, 0.01, "", " s");
            row(ui, "erratic ahead", "How strongly a stretch ahead where the foresight sketch's box grows past its typical size (someone was unsure there) slows playback. At 2, three times its typical size runs at a quarter of the speed.", &mut a.erratic, 0.0..=6.0, 0.01, "", "");
        });
        ui.label("Read ahead in");
        ui.horizontal_wrapped(|ui| {
            ui.radio_value(&mut a.foresight, Foresight::Parent, "the parent sketch").on_hover_text("The sketch whose view you're drawing in: its box shows where the subject was hard to follow. On the source, the sketch you're editing.");
            ui.radio_value(&mut a.foresight, Foresight::Editing, "the sketch I'm editing");
            ui.radio_value(&mut a.foresight, Foresight::Both, "both");
        });
        if ui.button("Defaults").on_hover_text("Put the knobs back (keeps it on or off)").clicked() {
            a = AutoSpeed { enabled: a.enabled, ..AutoSpeed::default() };
        }
    });
    if a != *world.resource::<AutoSpeed>() {
        *world.resource_mut::<AutoSpeed>() = a;
    }
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
        Track => "Track the selected sketch from here (on a tracker: re-seed it here)",
    }
}
