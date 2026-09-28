//! What the user has selected (DESIGN §12): session state shared by tools,
//! panels and overlays. Not undoable on its own; transactions will snapshot
//! it so undo restores the context you were working in.

use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::prelude::*;

use crate::app::{AppBuilder, Module, Set};
use crate::input::{Action, PendingActions};
use crate::meta::Class;

#[derive(Resource, Debug, Default, Clone, PartialEq)]
pub struct Selection {
    /// Selected entities; the last one is the primary (inspected) one.
    pub entities: Vec<Entity>,
}

impl Selection {
    pub fn primary(&self) -> Option<Entity> {
        self.entities.last().copied()
    }

    pub fn is_selected(&self, e: Entity) -> bool {
        self.entities.contains(&e)
    }

    pub fn select_only(&mut self, e: Entity) {
        self.entities = vec![e];
    }

    pub fn toggle(&mut self, e: Entity) {
        if let Some(i) = self.entities.iter().position(|x| *x == e) {
            self.entities.remove(i);
        } else {
            self.entities.push(e);
        }
    }

    pub fn clear(&mut self) {
        self.entities.clear();
    }
}

/// Deleted (disabled) or despawned entities drop out of the selection.
fn prune(mut sel: ResMut<Selection>, alive: Query<(), Without<Disabled>>) {
    if sel.entities.iter().any(|e| alive.get(*e).is_err()) {
        sel.entities.retain(|e| alive.get(*e).is_ok());
    }
}

fn apply_selection_actions(mut actions: ResMut<PendingActions>, mut sel: ResMut<Selection>) {
    if !actions.take(|a| a == Action::DeselectAll).is_empty() {
        sel.clear();
    }
}

pub struct SelectionModule;

impl Module for SelectionModule {
    fn build(&self, app: &mut AppBuilder) {
        app.declare::<Selection>(Class::Session).init_resource::<Selection>()
            // Before the tools, which read the selection when a stroke starts.
            .add_systems(apply_selection_actions.in_set(Set::Input))
            .add_systems(prune.in_set(Set::Prepare));
    }
}
