//! Outliner: the document's entities (named, or described by what they are).
//! Click selects; Ctrl+click adds to the selection. A dev section lists
//! modules and declared types. Grouping (media → views → captures) arrives
//! with those features (M3–M4).

use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::name::Name;
use bevy_ecs::prelude::*;
use bevy_ecs::reflect::{AppTypeRegistry, ReflectComponent};
use bevy_ecs::resource::IsResource;
use tt_core::ComponentMetas;
use tt_core::app::ModuleList;
use tt_core::meta::Class;
use tt_core::op::{OpError, Operator};
use tt_core::selection::Selection;

/// A human label: the Name component, else the operator kind, else the id.
pub fn label(world: &World, e: Entity) -> String {
    if let Some(n) = world.get::<Name>(e) {
        return n.as_str().to_string();
    }
    if let Some(op) = world.get::<Operator>(e) {
        return format!("{} ({e})", op.kind);
    }
    format!("Entity {e}")
}

pub fn ui(ui: &mut egui::Ui, world: &mut World) {
    egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
        let entities = document_entities(world);
        let selection = world.resource::<Selection>().clone();
        let mut clicked: Option<(Entity, bool)> = None;
        if entities.is_empty() {
            ui.label(egui::RichText::new("No document objects yet — captures, views and trackers arrive in M3–M4.").weak());
        }
        for e in entities {
            let text = label(world, e);
            let error = world.get::<OpError>(e).map(|err| err.0.clone());
            let mut rt = egui::RichText::new(text);
            if error.is_some() {
                rt = rt.color(egui::Color32::from_rgb(0xf4, 0x3f, 0x5e));
            }
            let r = ui.selectable_label(selection.is_selected(e), rt);
            let r = match error {
                Some(err) => r.on_hover_text(err),
                None => r,
            };
            if r.clicked() {
                clicked = Some((e, ui.input(|i| i.modifiers.ctrl || i.modifiers.command)));
            }
        }
        if let Some((e, add)) = clicked {
            let mut sel = world.resource_mut::<Selection>();
            if add { sel.toggle(e) } else { sel.select_only(e) }
        }

        ui.add_space(12.0);
        egui::CollapsingHeader::new("Dev: modules & types").default_open(false).show(ui, |ui| {
            for name in &world.resource::<ModuleList>().0 {
                ui.monospace(short(name));
            }
            ui.separator();
            let mut metas: Vec<_> = world.resource::<ComponentMetas>().iter().cloned().collect();
            metas.sort_by_key(|m| m.name);
            for m in metas {
                ui.horizontal(|ui| {
                    ui.monospace(short(m.name));
                    ui.label(egui::RichText::new(format!("{:?}", m.class)).weak());
                });
            }
        });
    });
}

/// Enabled entities that carry at least one document component.
fn document_entities(world: &mut World) -> Vec<Entity> {
    let doc_types: Vec<ReflectComponent> = {
        let registry = world.resource::<AppTypeRegistry>().read();
        let metas = world.resource::<ComponentMetas>();
        registry
            .iter()
            .filter(|r| metas.get(r.type_id()).is_some_and(|m| m.class == Class::Document))
            .filter_map(|r| r.data::<ReflectComponent>().cloned())
            .collect()
    };
    let mut q = world.query_filtered::<EntityRef, (Without<IsResource>, Without<Disabled>)>();
    let mut out: Vec<Entity> = q.iter(world).filter(|e| doc_types.iter().any(|rc| rc.contains(*e))).map(|e| e.id()).collect();
    out.sort();
    out
}

/// `tt_core::transport::TransportModule` → `TransportModule`.
fn short(path: &str) -> &str {
    path.rsplit("::").next().unwrap_or(path)
}
