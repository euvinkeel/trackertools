//! Transactions and undo (DESIGN §12).
//!
//! Every document change goes through [`edit`]: the closure mutates the world
//! through a [`Tx`], which records how to undo and redo each step. Derived
//! state is never recorded — operators recompute from what undo restores.
//!
//! - Components: typed before/after values.
//! - Entities: "delete" disables (bevy's `Disabled` marker hides an entity
//!   from every query) instead of despawning, so entity ids — and every edge
//!   pointing at them — stay valid across undo/redo.
//! - Signals: a copy-on-write snapshot (chunk pointers) taken before the first
//!   write; undo swaps it back and invalidates exactly the chunks that differ.
//! - Gestures: [`History::begin`] keeps one transaction open across frames so
//!   a drag or a whole capture is a single undo step.

use std::collections::HashMap;
use std::sync::Arc;

use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::prelude::*;

use crate::app::{AppBuilder, Module, Set};
use crate::input::{Action, PendingActions};
use crate::meta::Class;
use crate::op::{Invalidations, Output};
use crate::signal::{Signal, SignalId, SignalStore};

type Apply = Arc<dyn Fn(&mut World) + Send + Sync>;

struct Step {
    undo: Apply,
    redo: Apply,
}

pub struct Transaction {
    pub label: String,
    steps: Vec<Step>,
}

impl Transaction {
    fn undo(&self, world: &mut World) {
        for s in self.steps.iter().rev() {
            (s.undo)(world);
        }
    }

    fn redo(&self, world: &mut World) {
        for s in &self.steps {
            (s.redo)(world);
        }
    }
}

#[derive(Resource)]
pub struct History {
    undo: Vec<Transaction>,
    redo: Vec<Transaction>,
    open: Option<Transaction>,
    pub limit: usize,
}

impl Default for History {
    fn default() -> Self {
        Self { undo: Vec::new(), redo: Vec::new(), open: None, limit: 500 }
    }
}

impl History {
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn undo_label(&self) -> Option<&str> {
        self.undo.last().map(|t| t.label.as_str())
    }

    pub fn redo_label(&self) -> Option<&str> {
        self.redo.last().map(|t| t.label.as_str())
    }

    /// Start a gesture: edits until [`History::end`] form one undo step.
    pub fn begin(&mut self, label: impl Into<String>) {
        self.end();
        self.open = Some(Transaction { label: label.into(), steps: Vec::new() });
    }

    /// Finish the open gesture (a no-op gesture leaves no undo step).
    pub fn end(&mut self) {
        if let Some(t) = self.open.take() {
            self.push(t);
        }
    }

    pub fn in_gesture(&self) -> bool {
        self.open.is_some()
    }

    fn push(&mut self, t: Transaction) {
        if t.steps.is_empty() {
            return;
        }
        self.redo.clear();
        self.undo.push(t);
        if self.undo.len() > self.limit {
            self.undo.remove(0);
        }
    }
}

/// Records steps while an edit runs.
pub struct Tx<'w> {
    world: &'w mut World,
    steps: Vec<Step>,
    signals: HashMap<SignalId, Option<Signal>>,
}

impl Tx<'_> {
    pub fn world(&self) -> &World {
        self.world
    }

    /// Insert (or replace) a component.
    pub fn insert<C: Component<Mutability = bevy_ecs::component::Mutable> + Clone>(&mut self, e: Entity, value: C) {
        let before = self.world.get::<C>(e).cloned();
        self.world.entity_mut(e).insert(value.clone());
        self.steps.push(Step {
            undo: Arc::new(move |w| restore(w, e, before.clone())),
            redo: Arc::new(move |w| restore(w, e, Some(value.clone()))),
        });
    }

    /// Change a component in place.
    pub fn modify<C: Component<Mutability = bevy_ecs::component::Mutable> + Clone>(&mut self, e: Entity, f: impl FnOnce(&mut C)) {
        let Some(before) = self.world.get::<C>(e).cloned() else { return };
        f(&mut self.world.get_mut::<C>(e).unwrap());
        let after = self.world.get::<C>(e).cloned().unwrap();
        self.steps.push(Step {
            undo: Arc::new(move |w| restore(w, e, Some(before.clone()))),
            redo: Arc::new(move |w| restore(w, e, Some(after.clone()))),
        });
    }

    pub fn remove<C: Component<Mutability = bevy_ecs::component::Mutable> + Clone>(&mut self, e: Entity) {
        let Some(before) = self.world.get::<C>(e).cloned() else { return };
        self.world.entity_mut(e).remove::<C>();
        self.steps.push(Step {
            undo: Arc::new(move |w| restore(w, e, Some(before.clone()))),
            redo: Arc::new(move |w| restore::<C>(w, e, None)),
        });
    }

    /// Spawn a document entity. Undo disables it; redo re-enables the same id.
    pub fn spawn(&mut self, bundle: impl Bundle) -> Entity {
        let e = self.world.spawn(bundle).id();
        self.steps.push(Step { undo: Arc::new(move |w| set_enabled(w, e, false)), redo: Arc::new(move |w| set_enabled(w, e, true)) });
        e
    }

    /// Delete a document entity (disable; undo brings back the same id).
    pub fn delete(&mut self, e: Entity) {
        if self.world.get::<Disabled>(e).is_some() {
            return;
        }
        set_enabled(self.world, e, false);
        self.steps.push(Step { undo: Arc::new(move |w| set_enabled(w, e, true)), redo: Arc::new(move |w| set_enabled(w, e, false)) });
    }

    /// Write access to a signal; its prior state is snapshotted on first use.
    pub fn signal(&mut self, id: SignalId) -> &mut Signal {
        if !self.signals.contains_key(&id) {
            let before = self.world.resource::<SignalStore>().get(id).cloned();
            self.signals.insert(id, before);
        }
        self.world.resource_mut::<SignalStore>().into_inner().get_mut(id).expect("signal exists")
    }

    /// Create a signal as part of the edit (undo removes it).
    pub fn create_signal(&mut self, channels: usize) -> SignalId {
        let id = self.world.resource_mut::<SignalStore>().create(channels);
        self.signals.insert(id, None);
        id
    }

    fn finish(mut self) -> Vec<Step> {
        for (id, before) in std::mem::take(&mut self.signals) {
            let after = self.world.resource::<SignalStore>().get(id).cloned();
            let unchanged = match (&before, &after) {
                (Some(b), Some(a)) => a.same_as(b),
                (None, None) => true,
                _ => false,
            };
            if unchanged {
                continue;
            }
            notify_signal_change(self.world, id, before.as_ref(), after.as_ref());
            self.steps.push(Step {
                undo: Arc::new(move |w| swap_signal(w, id, before.clone())),
                redo: Arc::new(move |w| swap_signal(w, id, after.clone())),
            });
        }
        self.steps
    }
}

fn restore<C: Component<Mutability = bevy_ecs::component::Mutable> + Clone>(w: &mut World, e: Entity, value: Option<C>) {
    let Ok(mut entity) = w.get_entity_mut(e) else { return };
    match value {
        Some(v) => {
            entity.insert(v);
        }
        None => {
            entity.remove::<C>();
        }
    }
}

fn set_enabled(w: &mut World, e: Entity, enabled: bool) {
    let Ok(mut entity) = w.get_entity_mut(e) else { return };
    if enabled {
        entity.remove::<Disabled>();
    } else {
        entity.insert(Disabled);
    }
    // Consumers of this entity's output must recompute (it appeared or vanished).
    if w.get::<Output>(e).is_some() {
        let frames = crate::op::extent(w);
        w.resource_mut::<Invalidations>().output_changed(e, frames);
    }
    w.resource_mut::<crate::op::OpGraph>().mark_stale();
}

fn swap_signal(w: &mut World, id: SignalId, value: Option<Signal>) {
    let current = w.resource::<SignalStore>().get(id).cloned();
    notify_signal_change(w, id, current.as_ref(), value.as_ref());
    let mut store = w.resource_mut::<SignalStore>();
    match value {
        Some(s) => store.insert(id, s),
        None => {
            store.remove(id);
        }
    }
}

/// Tell the operator graph which frames of a producer's output changed.
fn notify_signal_change(w: &mut World, id: SignalId, before: Option<&Signal>, after: Option<&Signal>) {
    let frames = match (before, after) {
        (Some(b), Some(a)) => a.differing_chunks(b),
        _ => crate::ranges::RangeSet::from_range(crate::op::extent(w)),
    };
    let producer = w.query::<(Entity, &Output)>().iter(w).find(|(_, o)| o.0 == id).map(|(e, _)| e);
    if let Some(p) = producer {
        let mut inv = w.resource_mut::<Invalidations>();
        for r in frames.ranges() {
            inv.output_changed(p, r.clone());
        }
    }
}

/// Run a document edit as one undo step (or as part of the open gesture).
/// Returns whether anything was recorded.
pub fn edit(world: &mut World, label: &str, f: impl FnOnce(&mut Tx<'_>)) -> bool {
    let steps = {
        let mut tx = Tx { world, steps: Vec::new(), signals: HashMap::new() };
        f(&mut tx);
        tx.finish()
    };
    if steps.is_empty() {
        return false;
    }
    let mut history = world.resource_mut::<History>();
    match &mut history.open {
        Some(open) => open.steps.extend(steps),
        None => {
            let t = Transaction { label: label.to_string(), steps };
            history.push(t);
        }
    }
    true
}

pub fn undo(world: &mut World) -> Option<String> {
    let t = {
        let mut h = world.resource_mut::<History>();
        h.end();
        h.undo.pop()?
    };
    t.undo(world);
    let label = t.label.clone();
    world.resource_mut::<History>().redo.push(t);
    Some(label)
}

pub fn redo(world: &mut World) -> Option<String> {
    let t = world.resource_mut::<History>().redo.pop()?;
    t.redo(world);
    let label = t.label.clone();
    world.resource_mut::<History>().undo.push(t);
    Some(label)
}

fn apply_history_actions(world: &mut World) {
    let actions = world.resource_mut::<PendingActions>().take(|a| matches!(a, Action::Undo | Action::Redo));
    for a in actions {
        let label = if a == Action::Undo { undo(world) } else { redo(world) };
        if let Some(label) = label {
            tracing::info!("{} {label}", if a == Action::Undo { "undo" } else { "redo" });
        }
    }
}

pub struct HistoryModule;

impl Module for HistoryModule {
    fn build(&self, app: &mut AppBuilder) {
        app.declare::<History>(Class::Session).init_resource::<History>().add_systems(apply_history_actions.in_set(Set::Intents));
    }
}
