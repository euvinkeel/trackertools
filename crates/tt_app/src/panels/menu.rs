//! The right-click menu for entities, shared by the outliner, the timeline and
//! the viewport. It acts on the selection (a right-click on something
//! unselected selects it first: [`right_clicked`]). Commands go through
//! actions, so they are the same as their keys and undo the same way.

use bevy_ecs::prelude::*;
use tt_core::capture::LiveCapture;
use tt_core::commands::strokes_of;
use tt_core::input::{Action, Keymap, PendingActions};
use tt_core::selection::Selection;
use tt_core::sketch::{Capture, is_sketch, sketch_of};

/// Call on a secondary click of `e`: unless it is already selected, it becomes the selection.
pub fn right_clicked(world: &mut World, e: Entity) {
    if !world.resource::<Selection>().is_selected(e) {
        world.resource_mut::<Selection>().select_only(e);
    }
}

/// The menu's contents (inside `Response::context_menu`).
pub fn entity_menu(ui: &mut egui::Ui, world: &mut World) {
    let selection = world.resource::<Selection>().entities.clone();
    let primary = selection.last().copied();
    let keymap = world.resource::<Keymap>().clone();
    let chord = |a: Action| keymap.chord_for(a).map(|c| format!("  ({c})")).unwrap_or_default();
    let busy = world.resource::<LiveCapture>().0.is_some();
    let sketch = primary.and_then(|e| sketch_of(world, e));
    let any_sketch = selection.iter().any(|e| sketch_of(world, *e).is_some());
    let mut push: Option<Action> = None;

    if selection.is_empty() {
        ui.label(egui::RichText::new("Nothing selected").weak());
    } else {
        let what = match selection.len() {
            1 => crate::panels::outliner::label(world, selection[0]),
            n => format!("{n} selected"),
        };
        ui.label(egui::RichText::new(what).strong());
        ui.separator();
        if let Some(s) = sketch
            && ui.button(format!("Enter view{}", chord(Action::EnterView))).on_hover_text("See this sketch's view; sketch inside it for detail").clicked()
        {
            world.resource_mut::<Selection>().select_only(s);
            push = Some(Action::EnterView);
            ui.close();
        }
        if ui.add_enabled(primary.is_some(), egui::Button::new(format!("Rename…{}", chord(Action::Rename)))).clicked() {
            push = Some(Action::Rename);
            ui.close();
        }
        if ui.add_enabled(any_sketch && !busy, egui::Button::new(format!("Duplicate{}", chord(Action::Duplicate)))).on_hover_text("Copy the sketch with all its strokes").clicked() {
            push = Some(Action::Duplicate);
            ui.close();
        }
        if ui.add_enabled(!busy, egui::Button::new(format!("Delete{}", chord(Action::Delete)))).on_hover_text("A sketch goes with its strokes and view; a stroke leaves its sketch").clicked() {
            push = Some(Action::Delete);
            ui.close();
        }
        ui.separator();
        if let Some(p) = primary {
            if is_sketch(world, p) {
                let strokes = strokes_of(world, p);
                if ui.add_enabled(!strokes.is_empty(), egui::Button::new(format!("Select its {} strokes", strokes.len()))).clicked() {
                    world.resource_mut::<Selection>().entities = strokes;
                    ui.close();
                }
            } else if world.get::<Capture>(p).is_some()
                && let Some(s) = sketch
                && ui.button("Select its sketch").clicked()
            {
                world.resource_mut::<Selection>().select_only(s);
                ui.close();
            }
        }
    }
    // Where the selected thing is on the timeline.
    if let Some((a, b)) = primary.and_then(|p| tt_core::commands::time_span(world, p)) {
        ui.separator();
        if ui.button(format!("Go to its start (frame {a})")).clicked() {
            push = Some(Action::Seek(a));
            ui.close();
        }
        if ui.button(format!("Go to its end (frame {b})")).clicked() {
            push = Some(Action::Seek(b));
            ui.close();
        }
        ui.separator();
    }
    if ui.button(format!("Select all{}", chord(Action::SelectAll))).clicked() {
        push = Some(Action::SelectAll);
        ui.close();
    }
    if ui.add_enabled(!selection.is_empty(), egui::Button::new(format!("Deselect all{}", chord(Action::DeselectAll)))).clicked() {
        push = Some(Action::DeselectAll);
        ui.close();
    }
    if let Some(a) = push {
        world.resource_mut::<PendingActions>().push(a);
    }
}
