//! Component classes (DESIGN §12): what gets persisted, undone, or recomputed.
//! Also the creation order of document entities ([`Created`]).

use std::any::{TypeId, type_name};
use std::collections::HashMap;

use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::prelude::*;
use bevy_reflect::Reflect;

/// How the editor treats a component or resource type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Class {
    /// Part of the project: persisted and undoable.
    Document,
    /// In the world and persisted with session settings, but not undoable
    /// (viewport zoom, panel layout, hover, the playhead).
    Session,
    /// Recomputed from other state; never persisted or undone.
    Derived,
}

#[derive(Clone, Debug)]
pub struct ComponentMeta {
    pub class: Class,
    pub name: &'static str,
}

/// Registry of declared classes, keyed by Rust type.
#[derive(Resource, Default, Debug)]
pub struct ComponentMetas {
    by_type: HashMap<TypeId, ComponentMeta>,
}

impl ComponentMetas {
    pub fn insert<T: 'static>(&mut self, class: Class) {
        self.by_type.insert(TypeId::of::<T>(), ComponentMeta { class, name: type_name::<T>() });
    }

    pub fn get(&self, id: TypeId) -> Option<&ComponentMeta> {
        self.by_type.get(&id)
    }

    pub fn class_of<T: 'static>(&self) -> Option<Class> {
        self.get(TypeId::of::<T>()).map(|m| m.class)
    }

    pub fn iter(&self) -> impl Iterator<Item = &ComponentMeta> {
        self.by_type.values()
    }
}

/// When a document entity was made, relative to the others (a document
/// component, saved). Lists in creation order (the outliner, the timeline's
/// lanes, Select All) sort by it: entity ids are no order, since bevy reuses
/// freed ones, last freed first.
///
/// Stamped on every entity the moment it gets its first document component,
/// whatever made it (a stroke, a view, a duplicate, `spawn_op`, a module's own
/// spawn); a loaded project is stamped anew in its saved order.
#[derive(Component, Reflect, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[reflect(Component)]
pub struct Created(pub u64);

/// The next [`Created`] stamp.
#[derive(Resource, Debug, Default)]
pub struct CreationCounter {
    next: u64,
    /// The entity stamped last (a bundle adds several document components at once).
    last: Option<Entity>,
}

impl CreationCounter {
    /// Take the next stamp.
    pub fn stamp(&mut self) -> Created {
        self.next += 1;
        Created(self.next - 1)
    }

    /// Start over (a new document).
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Observer of document component `C`: an entity without a creation stamp gets one.
pub(crate) fn stamp_created<C: Component>(
    add: On<Add, C>,
    mut counter: ResMut<CreationCounter>,
    stamped: Query<(), (With<Created>, Allow<Disabled>)>,
    mut commands: Commands,
) {
    let e = add.entity;
    if counter.last == Some(e) || stamped.contains(e) {
        return;
    }
    counter.last = Some(e);
    let stamp = counter.stamp();
    commands.entity(e).insert_if_new(stamp);
}

/// Sort document entities by creation ([`Created`]; unstamped ones last).
pub fn creation_order(world: &World, entities: &mut [Entity]) {
    entities.sort_by_key(|e| (world.get::<Created>(*e).map_or(u64::MAX, |c| c.0), e.index_u32()));
}
