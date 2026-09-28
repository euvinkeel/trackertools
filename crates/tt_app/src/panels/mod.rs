//! Panels are functions of the world (DESIGN §1, §14): they read state and
//! queue actions/intents; they never own state or mutate the document directly.

mod inspector;
mod menu;
pub mod outliner;
mod overlay;
pub mod timeline;
pub mod viewport;

use bevy_ecs::prelude::*;
use tt_core::input::{Action, PendingActions};
use tt_core::time::timecode;
use tt_core::tool::{ActiveTool, Tool};
use tt_core::transport::Transport;

use crate::layout::{Layout, Pane};
use crate::media::{Media, OpenRequest, StatusLine, proxy_status};
use crate::session::Session;
use crate::style;

pub fn draw(ui: &mut egui::Ui, world: &mut World) {
    egui::Panel::top("top_bar").show(ui, |ui| top_bar(ui, world));
    egui::CentralPanel::no_frame().show(ui, |ui| {
        world.resource_scope(|world, mut layout: Mut<Layout>| {
            let mut behavior = Behavior { world };
            layout.tree.ui(&mut behavior, ui);
        });
    });
}

fn top_bar(ui: &mut egui::Ui, world: &mut World) {
    let mut open = false;
    let mut reopen = None;
    let mut history_action = None;
    let mut new_sketch = false;
    let t = world.resource::<Transport>();
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("trackertools").strong().color(style::ACCENT));
        ui.label(egui::RichText::new("v2 · M3").weak());
        ui.separator();
        open = ui.button("Open…").on_hover_text("Open a video (Ctrl+O), or drop a file on the window").clicked();
        let recent = world.resource::<Session>().recent();
        ui.add_enabled_ui(!recent.is_empty(), |ui| {
            ui.menu_button("Recent ⏷", |ui| {
                for path in recent {
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    if ui.button(name).on_hover_text(path.display().to_string()).clicked() {
                        reopen = Some(path.clone());
                    }
                }
            });
        });
        match world.get_resource::<Media>() {
            Some(m) => {
                let i = m.index();
                ui.label(egui::RichText::new(&m.name).strong());
                ui.label(
                    egui::RichText::new(format!(
                        "{}×{} · {} · {} frames @ {:.3} fps",
                        i.width,
                        i.height,
                        i.codec,
                        i.frame_count(),
                        i.fps.as_f64()
                    ))
                    .color(style::MUTED),
                );
                if let Some(status) = proxy_status(m) {
                    ui.separator();
                    ui.label(egui::RichText::new(status).color(style::MUTED));
                }
            }
            None => {
                ui.label(egui::RichText::new("no media (demo clock)").color(style::MUTED));
            }
        }
        if let Some((msg, error)) = &world.resource::<StatusLine>().0 {
            ui.label(egui::RichText::new(msg).color(if *error { egui::Color32::from_rgb(0xf4, 0x3f, 0x5e) } else { style::MUTED }));
        }
        let history = world.resource::<tt_core::history::History>();
        ui.separator();
        let undo_tip = history.undo_label().map_or("Nothing to undo".to_string(), |l| format!("Undo {l} (Ctrl+Z)"));
        let redo_tip = history.redo_label().map_or("Nothing to redo".to_string(), |l| format!("Redo {l} (Ctrl+Shift+Z)"));
        if ui.add_enabled(history.can_undo(), egui::Button::new("⟲")).on_hover_text(undo_tip).on_disabled_hover_text("Nothing to undo").clicked() {
            history_action = Some(Action::Undo);
        }
        if ui.add_enabled(history.can_redo(), egui::Button::new("⟳")).on_hover_text(redo_tip).on_disabled_hover_text("Nothing to redo").clicked() {
            history_action = Some(Action::Redo);
        }
        ui.separator();
        let sketching = world.resource::<ActiveTool>().0 == Tool::Sketch;
        let chord = world.resource::<tt_core::input::Keymap>().chord_for(Action::Tool(Tool::Sketch)).unwrap_or_default();
        if ui
            .selectable_label(sketching, "✏ Sketch")
            .on_hover_text(format!(
                "Sketch tool ({chord})\n\
                 • hold on the video: record into the selected sketch at the shown frame (paused: edit that instant)\n\
                 • Space while holding: play and record across frames (at the playback speed: Q slower, E faster)\n\
                 • wheel while holding: falloff to neighbouring frames\n\
                 • Ctrl+hold: move only (keep the box size) · Shift+hold: new sketch\n\
                 • click: select the sketch under the cursor · Alt+A: deselect · Esc: cancel
                 • Tab: enter the selected sketch's view (sketch inside it for detail) · Shift+Tab: back up"
            ))
            .clicked()
        {
            history_action = Some(Action::Tool(Tool::Sketch));
        }
        let sketch_selected = world.resource::<tt_core::selection::Selection>().primary().is_some_and(|e| tt_core::sketch::is_sketch(world, e));
        if ui
            .add_enabled(sketch_selected || !sketching, egui::Button::new("✚ New sketch"))
            .on_hover_text("The next stroke starts a new sketch instead of editing the selected one (deselects; Shift+hold does the same for one stroke)")
            .on_disabled_hover_text("Nothing is selected: the next stroke already starts a new sketch")
            .clicked()
        {
            new_sketch = true;
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // egui repaints on demand, so frame time only means something while playing.
            if t.playing {
                let dt = ui.ctx().input(|i| i.stable_dt).max(1e-4);
                ui.monospace(format!("{:>5.1} ms/frame", dt * 1000.0));
            } else {
                ui.label(egui::RichText::new("idle").weak());
            }
            ui.separator();
            ui.monospace(timecode(t.frame(), t.fps));
        });
    });
    if open {
        world.resource_mut::<PendingActions>().push(Action::OpenFile);
    }
    if let Some(a) = history_action {
        world.resource_mut::<PendingActions>().push(a);
    }
    if new_sketch {
        world.resource_mut::<tt_core::selection::Selection>().clear();
        world.resource_mut::<ActiveTool>().0 = Tool::Sketch;
    }
    if let Some(path) = reopen {
        world.resource_mut::<OpenRequest>().0 = Some(path);
    }
}

struct Behavior<'w> {
    world: &'w mut World,
}

impl egui_tiles::Behavior<Pane> for Behavior<'_> {
    fn pane_ui(&mut self, ui: &mut egui::Ui, _tile: egui_tiles::TileId, pane: &mut Pane) -> egui_tiles::UiResponse {
        match pane {
            Pane::Viewport => viewport::ui(ui, self.world),
            Pane::Timeline => timeline::ui(ui, self.world),
            Pane::Inspector => inspector::ui(ui, self.world),
            Pane::Outliner => outliner::ui(ui, self.world),
        }
        egui_tiles::UiResponse::None
    }

    fn tab_title_for_pane(&mut self, pane: &Pane) -> egui::WidgetText {
        pane.title().into()
    }

    fn simplification_options(&self) -> egui_tiles::SimplificationOptions {
        egui_tiles::SimplificationOptions { all_panes_must_have_tabs: true, ..Default::default() }
    }
}
