//! Document commands on the selection (DESIGN §12): delete, duplicate,
//! rename, select all. Each is one transaction (one undo step) and headless.
//!
//! - Delete takes what belongs to an entity with it: a sketch's strokes and
//!   its view. A stroke on its own leaves its sketch, which re-derives
//!   without it. Sketches drawn inside a deleted view keep working: their
//!   paths live in source pixels (`Through`).
//! - Duplicate copies sketches with all their strokes (signals are shared
//!   copy-on-write until either side changes), for trying other settings.

use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::name::Name;
use bevy_ecs::prelude::*;

use crate::app::{AppBuilder, Module, Set};
use crate::capture::LiveCapture;
use crate::history::edit;
use crate::input::{Action, PendingActions};
use crate::meta::{Class, creation_order};
use crate::op::{Inputs, Operator, Output};
use crate::selection::Selection;
use crate::signal::{Signal, SignalStore};
use crate::sketch::{BOX_CHANNELS, Capture, ClockMap, SketchParams, Stroke, Through, is_sketch, sketch_of};
use crate::view::view_of;

/// The entity the outliner should start renaming (F2); the outliner takes it.
#[derive(Resource, Debug, Default)]
pub struct RenameRequest(pub Option<Entity>);

fn is_live(world: &World, e: Entity) -> bool {
    world.get_entity(e).is_ok_and(|r| !r.contains::<Disabled>())
}

/// A sketch's strokes, in layer order.
pub fn strokes_of(world: &World, sketch: Entity) -> Vec<Entity> {
    world
        .get::<Inputs>(sketch)
        .map(|i| i.0.iter().filter(|(s, _)| s == "stroke" || s == "capture").map(|(_, e)| *e).filter(|e| is_live(world, *e)).collect())
        .unwrap_or_default()
}

fn name_of(world: &World, e: Entity) -> String {
    world.get::<Name>(e).map_or_else(|| "item".to_string(), |n| n.to_string())
}

/// Delete `targets` and what belongs to them, as one undo step. Returns how many entities went.
pub fn delete(world: &mut World, targets: &[Entity]) -> usize {
    let mut doomed: Vec<Entity> = Vec::new();
    let live: Vec<Entity> = targets.iter().copied().filter(|e| is_live(world, *e)).collect();
    for e in live {
        doomed.push(e);
        if is_sketch(world, e) {
            doomed.extend(strokes_of(world, e));
            doomed.extend(view_of(world, e));
        }
    }
    doomed.sort();
    doomed.dedup();
    if doomed.is_empty() {
        return 0;
    }
    // Strokes leaving a sketch that stays: out of its inputs.
    let mut detach: Vec<(Entity, Entity)> = Vec::new();
    for &e in &doomed {
        if world.get::<Capture>(e).is_some()
            && let Some(s) = sketch_of(world, e).filter(|s| !doomed.contains(s))
        {
            detach.push((s, e));
        }
    }
    let label = match targets {
        [one] => format!("Delete {}", name_of(world, *one)),
        _ => format!("Delete {} items", targets.len()),
    };
    edit(world, &label, |tx| {
        for (s, stroke) in &detach {
            tx.modify::<Inputs>(*s, |i| i.0.retain(|(_, p)| p != stroke));
        }
        for e in &doomed {
            tx.delete(*e);
        }
    });
    doomed.len()
}

struct StrokeCopy {
    capture: Capture,
    clock: ClockMap,
    stroke: Stroke,
    stream: Signal,
    through: Option<Signal>,
}

/// Duplicate the sketches among `targets` (a stroke counts as its sketch),
/// with copies of all their strokes, as one undo step. The copies are selected.
pub fn duplicate(world: &mut World, targets: &[Entity]) -> Vec<Entity> {
    // Each sketch once, in the order first seen (a sketch and its strokes may both be selected).
    let mut sketches: Vec<Entity> = Vec::new();
    for &e in targets {
        if is_live(world, e)
            && let Some(s) = sketch_of(world, e)
            && !sketches.contains(&s)
        {
            sketches.push(s);
        }
    }
    let store = world.resource::<SignalStore>();
    let signal = |id: crate::signal::SignalId| store.get(id).cloned();
    let mut copies = Vec::new();
    for &s in &sketches {
        let strokes: Vec<StrokeCopy> = strokes_of(world, s)
            .into_iter()
            .filter_map(|c| {
                Some(StrokeCopy {
                    capture: world.get::<Capture>(c)?.clone(),
                    clock: world.get::<ClockMap>(c)?.clone(),
                    stroke: world.get::<Stroke>(c).cloned().unwrap_or_default(),
                    stream: signal(world.get::<Output>(c)?.0)?,
                    through: world.get::<Through>(c).and_then(|t| signal(t.0)),
                })
            })
            .collect();
        let space = world.get::<Inputs>(s).and_then(|i| i.0.iter().find(|(slot, _)| slot == "space").map(|(_, v)| *v));
        let params = world.get::<SketchParams>(s).cloned().unwrap_or_default();
        copies.push((name_of(world, s), params, space, strokes));
    }
    if copies.is_empty() {
        return Vec::new();
    }
    let (mut names, n_strokes) = (live_names(world), world.query::<&Capture>().iter(world).count());
    let mut made = Vec::new();
    edit(world, "Duplicate", |tx| {
        for (name, params, space, strokes) in copies {
            let mut inputs = Vec::new();
            for st in strokes {
                let stream = tx.create_signal(st.stream.channels());
                *tx.signal(stream) = st.stream;
                let stroke_name = numbered(&names, "Stroke", n_strokes + 1);
                names.push(stroke_name.clone());
                let c = tx.spawn((Name::new(stroke_name), st.capture, st.clock, st.stroke, Output(stream)));
                if let Some(t) = st.through {
                    let id = tx.create_signal(t.channels());
                    *tx.signal(id) = t;
                    tx.insert(c, Through(id));
                }
                inputs.push(("stroke".to_string(), c));
            }
            inputs.extend(space.map(|v| ("space".to_string(), v)));
            let copy_name = unique(&names, &format!("{name} copy"));
            names.push(copy_name.clone());
            let out = tx.create_signal(BOX_CHANNELS);
            made.push(tx.spawn((Name::new(copy_name), Operator { kind: "sketch".into() }, Inputs(inputs), Output(out), params)));
        }
    });
    world.resource_mut::<Selection>().entities = made.clone();
    made
}

/// `base`, or `base 2`, `base 3`… whichever isn't taken.
fn unique(taken: &[String], base: &str) -> String {
    if !taken.iter().any(|t| t == base) {
        return base.to_string();
    }
    numbered(taken, base, 2)
}

/// `prefix n` for the first `n` from `from` on that isn't taken (after a
/// delete, counting what is left would name a new one like an old one).
pub(crate) fn numbered(taken: &[String], prefix: &str, from: usize) -> String {
    (from..).map(|i| format!("{prefix} {i}")).find(|n| !taken.iter().any(|t| t == n)).expect("some name is free")
}

/// The names of all live entities.
pub(crate) fn live_names(world: &mut World) -> Vec<String> {
    world.query::<&Name>().iter(world).map(|n| n.to_string()).collect()
}

/// Rename an entity (one undo step). Returns whether it changed.
pub fn rename(world: &mut World, e: Entity, name: &str) -> bool {
    let name = name.trim();
    if name.is_empty() || world.get::<Name>(e).is_some_and(|n| n.as_str() == name) {
        return false;
    }
    edit(world, &format!("Rename to {name}"), |tx| tx.insert(e, Name::new(name.to_string())))
}

/// Select every sketch, in creation order (the newest is the primary).
pub fn select_all(world: &mut World) {
    let mut q = world.query::<(Entity, &Operator)>();
    let mut all: Vec<Entity> = q.iter(world).filter(|(_, o)| o.kind == "sketch").map(|(e, _)| e).collect();
    creation_order(world, &mut all);
    world.resource_mut::<Selection>().entities = all;
}

fn apply_command_actions(world: &mut World) {
    let actions = world.resource_mut::<PendingActions>().take(|a| matches!(a, Action::Delete | Action::Duplicate | Action::SelectAll | Action::Rename));
    // A stroke in progress keeps its sketch and view (the menu greys these out too).
    let busy = world.resource::<LiveCapture>().0.is_some();
    for a in actions {
        let selected = world.resource::<Selection>().entities.clone();
        match a {
            Action::Delete | Action::Duplicate if busy => tracing::info!("{a:?} ignored: a stroke is in progress"),
            Action::Delete => {
                delete(world, &selected);
            }
            Action::Duplicate => {
                duplicate(world, &selected);
            }
            Action::SelectAll => select_all(world),
            Action::Rename => world.resource_mut::<RenameRequest>().0 = world.resource::<Selection>().primary(),
            _ => {}
        }
    }
}

pub struct CommandsModule;

impl Module for CommandsModule {
    fn build(&self, app: &mut AppBuilder) {
        app.declare::<RenameRequest>(Class::Session).init_resource::<RenameRequest>().add_systems(apply_command_actions.in_set(Set::Intents));
    }
}
