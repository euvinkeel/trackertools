//! Component classes (DESIGN §12): what gets persisted, undone, or recomputed.

use std::any::{TypeId, type_name};
use std::collections::HashMap;

use bevy_ecs::prelude::*;

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
