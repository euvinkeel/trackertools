//! Outliner for M0: a dev view of what the world contains (modules, declared
//! component classes, entities). M2 turns it into media → views → captures.

use bevy_ecs::prelude::*;
use bevy_ecs::resource::IsResource;
use tt_core::ComponentMetas;
use tt_core::app::ModuleList;

pub fn ui(ui: &mut egui::Ui, world: &mut World) {
    egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
        egui::CollapsingHeader::new("Modules").default_open(true).show(ui, |ui| {
            for name in &world.resource::<ModuleList>().0 {
                ui.monospace(short(name));
            }
        });

        egui::CollapsingHeader::new("Declared types").default_open(true).show(ui, |ui| {
            let mut metas: Vec<_> = world.resource::<ComponentMetas>().iter().cloned().collect();
            metas.sort_by_key(|m| m.name);
            for m in metas {
                ui.horizontal(|ui| {
                    ui.monospace(short(m.name));
                    ui.label(egui::RichText::new(format!("{:?}", m.class)).weak());
                });
            }
        });

        // bevy 0.19 stores resources as entities tagged IsResource; list only
        // the entities the editor itself creates.
        let resources = world.query_filtered::<Entity, With<IsResource>>().iter(world).count();
        let entities: Vec<Entity> = world.query_filtered::<Entity, Without<IsResource>>().iter(world).collect();
        ui.label(egui::RichText::new(format!("{resources} resources")).weak());
        egui::CollapsingHeader::new(format!("Entities ({})", entities.len())).default_open(true).show(ui, |ui| {
            if entities.is_empty() {
                ui.label(egui::RichText::new("none yet — media and captures arrive in M1–M3").weak());
            }
            for e in entities {
                ui.monospace(format!("{e}"));
            }
        });
    });
}

/// `tt_core::transport::TransportModule` → `TransportModule`.
fn short(path: &str) -> &str {
    path.rsplit("::").next().unwrap_or(path)
}
