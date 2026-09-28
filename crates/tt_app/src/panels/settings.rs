//! Settings: how the tools behave (remembered between launches, in the
//! session file) and every key, so nothing has to be memorized.

use bevy_ecs::prelude::*;
use tt_core::autospeed::{AutoSpeed, AutoSpeedState, Foresight};
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
        ui.heading("Trackers");
        let mut auto_mask = world.resource::<tt_track::look::LookDefaults>().auto_mask;
        if ui
            .checkbox(&mut auto_mask, "Paint a new look's mask automatically")
            .on_hover_text("When you drag a look, the pixels that stand out from its rectangle's border (a cursor over the game) become its mask, so only they count. You can still repaint it in the Look editor.")
            .changed()
        {
            world.resource_mut::<tt_track::look::LookDefaults>().auto_mask = auto_mask;
        }

        ui.separator();
        ui.heading("Views");
        ui.label("New views (Tab into a sketch)");
        let (pan, lock) = {
            let p = &world.resource::<ViewDefaults>().params;
            (p.pan_only, p.lock_zoom)
        };
        let mut mode = if pan { 0 } else if lock { 1 } else { 2 };
        ui.radio_value(&mut mode, 0, "only pan: follow the subject; the zoom is yours (the wheel)");
        ui.radio_value(&mut mode, 1, "steady zoom: the widest the sketch needs");
        ui.radio_value(&mut mode, 2, "zoom with the sketch's size (smoothed)");
        if mode != if pan { 0 } else if lock { 1 } else { 2 } {
            let mut d = world.resource_mut::<ViewDefaults>();
            (d.params.pan_only, d.params.lock_zoom) = (mode == 0, mode != 2);
        }
        ui.label(egui::RichText::new("Each view has its own \"pan only\" and \"lock zoom\" in the Inspector.").color(style::MUTED).small());
        let views: Vec<Entity> = {
            let mut q = world.query_filtered::<(Entity, &tt_core::view::FrameParams), bevy_ecs::query::Without<bevy_ecs::entity_disabling::Disabled>>();
            q.iter(world).filter(|(_, p)| !p.pan_only).map(|(e, _)| e).collect()
        };
        if ui.add_enabled(!views.is_empty(), egui::Button::new(format!("Make the {} zooming view(s) in this project only pan", views.len()))).clicked() {
            tt_core::history::edit(world, "Views only pan", |tx| {
                for v in views {
                    tx.modify::<tt_core::view::FrameParams>(v, |p| p.pan_only = true);
                }
            });
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

/// Anticipatory speed (tt_core::autospeed): the switch, the speed range and
/// how far ahead it reads; the rest under Advanced.
fn auto_speed(ui: &mut egui::Ui, world: &mut World) {
    let mut a = world.resource::<AutoSpeed>().clone();
    let (bias, comfort) = {
        let s = world.resource::<AutoSpeedState>();
        (s.bias, s.calibrated_comfort())
    };
    ui.heading("Anticipatory speed");
    ui.checkbox(&mut a.enabled, "Set the playback speed for me while I hold a stroke").on_hover_text(
        "While you hold a stroke with the video playing, it reads ahead in the parent sketch (the one whose view you're drawing in):          where its box is bigger than usual for it, the subject was hard to follow, so playback slows before that arrives;          where it is as small as usual, it plays fast. It measures what \"usual\" is on the sketch itself (the 20th to 80th percentile of its box sizes).          Q/E during a stroke multiply its speed.",
    );
    ui.add_enabled_ui(a.enabled, |ui| {
        egui::Grid::new("auto-speed").num_columns(2).show(ui, |ui| {
            ui.label("busy stretches at").on_hover_text("The speed where the sketch ahead was at its busiest (its box at or above its 80th percentile).");
            ui.add(egui::Slider::new(&mut a.slowest, 0.02..=1.0).logarithmic(true).max_decimals(2).prefix("×"));
            ui.end_row();
            ui.label("calm stretches at").on_hover_text("The speed where the sketch ahead was as calm as it gets (its box at or below its 20th percentile). In between, the speed goes smoothly from one to the other.");
            ui.add(egui::Slider::new(&mut a.fastest, 0.25..=4.0).logarithmic(true).max_decimals(2).prefix("×"));
            ui.end_row();
            ui.label("look ahead").on_hover_text("How far ahead it reads (seconds of video): the busiest moment in this window sets the speed, so it slows this long before a busy stretch.");
            ui.add(egui::Slider::new(&mut a.look_ahead, 0.0..=5.0).max_decimals(2).suffix(" s"));
            ui.end_row();
        });
        a.fastest = a.fastest.max(a.slowest);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(format!("Q/E while it drives: your multiplier ×{bias:.2}")).color(style::MUTED).small());
            if ui.add_enabled((bias - 1.0).abs() > 1e-9, egui::Button::new("Reset").small()).clicked() {
                world.resource_mut::<AutoSpeedState>().bias = 1.0;
            }
        });

        egui::CollapsingHeader::new("Advanced").id_salt("auto-speed-advanced").show(ui, |ui| {
            ui.label("Read ahead in");
            ui.horizontal_wrapped(|ui| {
                ui.radio_value(&mut a.foresight, Foresight::Parent, "the parent sketch").on_hover_text("The sketch whose view you're drawing in: its box shows where the subject was hard to follow. On the source, the sketch you're editing.");
                ui.radio_value(&mut a.foresight, Foresight::Editing, "the sketch I'm editing");
                ui.radio_value(&mut a.foresight, Foresight::Both, "both (the busier)");
            });
            ui.add_space(4.0);
            ui.checkbox(&mut a.react_to_hand, "Also slow down when my hand races or starts to jiggle").on_hover_text(
                "Reacts to your hand right now (it can't see ahead): slower while the subject moves faster on screen than your comfortable hand speed,                  or while your jiggle grows. Also drives the speed where there's nothing to read ahead.",
            );
            ui.add_enabled_ui(a.react_to_hand, |ui| {
                egui::Grid::new("auto-speed-hand").num_columns(2).show(ui, |ui| {
                    ui.label("comfortable hand speed").on_hover_text("How fast your hand follows comfortably, on screen. Calibrate sets it from your recent strokes.");
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut a.comfort).range(20.0..=5000.0).speed(2.0).suffix(" pt/s"));
                        let tip = match comfort {
                            Some(c) => format!("Your hand moved at up to about {c:.0} pt/s (on screen, at 1×) in most of your recent strokes: use that."),
                            None => "Hold a few strokes with the video playing first: it measures how fast your hand moves.".to_string(),
                        };
                        if ui.add_enabled(comfort.is_some(), egui::Button::new("Calibrate from my recent strokes")).on_hover_text(tip.clone()).on_disabled_hover_text(tip).clicked()
                            && let Some(c) = comfort
                        {
                            a.comfort = c.round() as f32;
                        }
                    });
                    ui.end_row();
                    ui.label("jiggle sensitivity").on_hover_text("How strongly your jiggle growing slows it. 0 ignores it; 1 halves the speed at three times its calm size.");
                    ui.add(egui::Slider::new(&mut a.jiggle, 0.0..=4.0).max_decimals(2));
                    ui.end_row();
                });
            });
            ui.add_space(4.0);
            egui::Grid::new("auto-speed-smooth").num_columns(2).show(ui, |ui| {
                ui.label("slow down within").on_hover_text("How quickly it slows down (real seconds): short, so it brakes in time.");
                ui.add(egui::DragValue::new(&mut a.slow_down).range(0.01..=2.0).speed(0.005).suffix(" s"));
                ui.end_row();
                ui.label("speed up within").on_hover_text("How quickly it speeds back up (real seconds): long, so it doesn't lurch.");
                ui.add(egui::DragValue::new(&mut a.speed_up).range(0.05..=10.0).speed(0.01).suffix(" s"));
                ui.end_row();
            });
            if ui.button("Defaults").on_hover_text("Put everything back (keeps it on or off)").clicked() {
                a = AutoSpeed { enabled: a.enabled, ..AutoSpeed::default() };
            }
        });
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
        ShuttleForward => "Play forward; again: twice as fast (up to 8×)",
        ShuttleBackward => "Play backward; again: twice as fast (up to 8×)",
        FrameAll => "Fit the video in the viewport",
        OpenFile => "Open a video",
        Undo => "Undo",
        Redo => "Redo",
        Tool(tt_core::tool::Tool::Track) => "Track tool on/off: drag a pattern or click a point on the video",
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
        ToggleSnap => "Snap the playhead to the start and end of things on the timeline while scrubbing (Ctrl inverts)",
    }
}
