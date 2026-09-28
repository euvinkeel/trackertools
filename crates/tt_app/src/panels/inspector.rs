//! Inspector: every reflected component of the selected entity, editable with
//! generic widgets. Edits go through transactions (undoable); a drag is one
//! undo step. Below, read-only views of core resources.

use bevy_ecs::prelude::*;
use bevy_ecs::reflect::{AppTypeRegistry, ReflectComponent};
use bevy_reflect::PartialReflect;
use tt_core::capture::SketchDefaults;
use tt_core::history::{History, edit};
use tt_core::meta::{Class, ComponentMetas};
use tt_core::selection::Selection;
use tt_core::sketch::SketchParams;
use tt_core::time::WallClock;
use tt_core::transport::Transport;

use crate::reflect_ui;

pub fn ui(ui: &mut egui::Ui, world: &mut World) {
    egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
        match world.resource::<Selection>().primary() {
            Some(e) => entity_section(ui, world, e),
            None => {
                ui.label(egui::RichText::new("Nothing selected. Select something in the Outliner.").weak());
            }
        }
        ui.separator();
        egui::CollapsingHeader::new("Transport").default_open(false).show(ui, |ui| {
            reflect_ui::show(ui, world.resource::<Transport>().as_partial_reflect())
        });
        egui::CollapsingHeader::new("Wall clock").default_open(false).show(ui, |ui| {
            reflect_ui::show(ui, world.resource::<WallClock>().as_partial_reflect())
        });
    });
}

fn entity_section(ui: &mut egui::Ui, world: &mut World, e: Entity) {
    ui.heading(crate::panels::outliner::label(world, e));
    if world.get::<SketchParams>(e).is_some() {
        sketch_presets(ui, world, e);
    }
    components(ui, world, e);
    // A sketch's view (its framing) is edited right here too.
    if let Some(view) = tt_core::view::view_of(world, e) {
        ui.separator();
        egui::CollapsingHeader::new("View (Tab to enter)").id_salt(("view", view)).default_open(true).show(ui, |ui| components(ui, world, view));
    }
}

/// Every reflected document/session component of `e`, editable.
fn components(ui: &mut egui::Ui, world: &mut World, e: Entity) {
    // Editable copies of each reflected document/session component on the entity.
    let mut components: Vec<(String, String, Box<dyn PartialReflect>, Class)> = {
        let registry = world.resource::<AppTypeRegistry>().read();
        let metas = world.resource::<ComponentMetas>();
        let Ok(entity) = world.get_entity(e) else { return };
        registry
            .iter()
            .filter_map(|r| {
                let class = metas.get(r.type_id())?.class;
                // Derived state isn't edited; the name is the heading; the creation order is bookkeeping.
                if class == Class::Derived || [std::any::TypeId::of::<bevy_ecs::name::Name>(), std::any::TypeId::of::<tt_core::meta::Created>()].contains(&r.type_id()) {
                    return None;
                }
                let rc = r.data::<ReflectComponent>()?;
                let value = rc.reflect(entity)?;
                let info = r.type_info().type_path_table();
                Some((info.path().to_string(), info.short_path().to_string(), value.to_dynamic(), class))
            })
            .collect()
    };
    components.sort_by(|a, b| a.1.cmp(&b.1));

    let mut pending: Option<PendingEdit> = None;
    for (path, short, mut value, class) in components {
        egui::CollapsingHeader::new(&short).id_salt((&path, e)).default_open(true).show(ui, |ui| {
            if class == Class::Session {
                ui.label(egui::RichText::new("session (not undoable)").weak().small());
            }
            let r = reflect_ui::edit(ui, value.as_mut());
            if r.changed || r.drag_started || r.drag_stopped {
                pending = Some((path.clone(), short.clone(), value, r, class));
            }
        });
    }

    let Some((path, short, value, r, class)) = pending else { return };
    if class == Class::Session {
        // Session state applies directly: not part of the undo history.
        let rc = world.resource::<AppTypeRegistry>().read().get_with_type_path(&path).and_then(|t| t.data::<ReflectComponent>().cloned());
        if let (Some(rc), true) = (rc, r.changed) {
            rc.apply(world.entity_mut(e), value.as_ref());
        }
        return;
    }
    // A drag is one undo step: open a gesture on drag start, close it on release.
    if r.drag_started {
        world.resource_mut::<History>().begin(format!("Edit {short}"));
    }
    if r.changed {
        edit(world, &format!("Edit {short}"), |tx| {
            tx.set_reflected(e, &path, value.as_ref());
        });
    }
    if r.drag_stopped {
        world.resource_mut::<History>().end();
    }
}

/// Preset buttons over a sketch's numbers, and "use for new sketches".
fn sketch_presets(ui: &mut egui::Ui, world: &mut World, e: Entity) {
    let current = world.get::<SketchParams>(e).cloned().unwrap_or_default();
    let mut chosen = None;
    let mut make_default = false;
    ui.horizontal(|ui| {
        ui.label("Preset");
        for name in SketchParams::PRESETS {
            let preset = SketchParams::preset(name).expect("listed preset");
            if ui.selectable_label(preset == current, name).clicked() && preset != current {
                chosen = Some((name, preset));
            }
        }
        let is_default = world.resource::<SketchDefaults>().params == current;
        make_default = ui
            .add_enabled(!is_default, egui::Button::new("Use for new sketches").small())
            .on_hover_text("New captures start with these numbers")
            .on_disabled_hover_text("New captures already start with these numbers")
            .clicked();
    });
    if let Some((name, preset)) = chosen {
        edit(world, &format!("Preset {name}"), |tx| tx.insert(e, preset));
    }
    if make_default {
        world.resource_mut::<SketchDefaults>().params = current;
    }
}

/// An edit made this frame: (type path, short name, new value, what happened, class).
type PendingEdit = (String, String, Box<dyn PartialReflect>, reflect_ui::Edited, Class);
