//! Undo/redo: components, entities (disable, stable ids), signals (with
//! operator invalidation), gestures, and a randomized undo-all/redo-all check.

use std::ops::Range;

use bevy_ecs::prelude::*;
use bevy_reflect::Reflect;
use tt_core::history::{History, edit, redo, undo};
use tt_core::op::{EvalBudget, EvalCtx, Footprint, OperatorKind, Output, spawn_op};
use tt_core::signal::{Signal, SignalStore};
use tt_core::time::FrameIndex;
use tt_core::transport::Transport;
use tt_core::{AppBuilder, Core, CoreModules};

const N: FrameIndex = 1000;

#[derive(Component, Reflect, Clone, Debug, PartialEq, Default)]
#[reflect(Component)]
struct Val(i32);

#[derive(Component, Reflect, Clone, Debug, PartialEq, Default)]
#[reflect(Component)]
struct Offset {
    k: f32,
}

struct OffsetKind;
impl OperatorKind for OffsetKind {
    fn name(&self) -> &'static str {
        "offset"
    }
    fn channels(&self) -> usize {
        1
    }
    fn footprint(&self, _: EntityRef<'_>) -> Footprint {
        Footprint::Pointwise
    }
    fn evaluate(&self, ctx: &EvalCtx<'_>, range: Range<FrameIndex>, out: &mut Signal) -> anyhow::Result<()> {
        let k = ctx.params::<Offset>().map_or(0.0, |p| p.k);
        let input = ctx.input("in");
        for f in range {
            match input.and_then(|s| s.get_valid(f)) {
                Some(v) => out.set(f, &[v[0] + k]),
                None => out.clear(f..f + 1),
            }
        }
        Ok(())
    }
}

fn core() -> Core {
    let mut app = AppBuilder::new();
    app.add_module(CoreModules).operator(OffsetKind).operator_params::<Offset>();
    let mut core = app.build();
    core.world.resource_mut::<Transport>().frame_count = N;
    *core.world.resource_mut::<EvalBudget>() = EvalBudget { millis: 1e9, step_frames: 4096 };
    core
}

#[test]
fn component_round_trip() {
    let mut c = core();
    let e = c.world.spawn(Val(1)).id();
    edit(&mut c.world, "set", |tx| tx.modify::<Val>(e, |v| v.0 = 5));
    edit(&mut c.world, "replace", |tx| tx.insert(e, Val(9)));
    edit(&mut c.world, "remove", |tx| tx.remove::<Val>(e));
    assert_eq!(c.world.get::<Val>(e), None);
    assert_eq!(undo(&mut c.world).as_deref(), Some("remove"));
    assert_eq!(c.world.get::<Val>(e), Some(&Val(9)));
    undo(&mut c.world);
    undo(&mut c.world);
    assert_eq!(c.world.get::<Val>(e), Some(&Val(1)));
    assert!(undo(&mut c.world).is_none());
    redo(&mut c.world);
    redo(&mut c.world);
    assert_eq!(c.world.get::<Val>(e), Some(&Val(9)));
    // A new edit clears the redo stack.
    edit(&mut c.world, "set", |tx| tx.modify::<Val>(e, |v| v.0 = 2));
    assert!(!c.world.resource::<History>().can_redo());
}

#[test]
fn delete_keeps_ids_stable() {
    let mut c = core();
    let mut created = None;
    edit(&mut c.world, "add", |tx| created = Some(tx.spawn(Val(3))));
    let e = created.unwrap();
    let count = |c: &mut Core| c.world.query::<&Val>().iter(&c.world).count();
    assert_eq!(count(&mut c), 1);
    edit(&mut c.world, "delete", |tx| tx.delete(e));
    assert_eq!(count(&mut c), 0, "disabled entities are hidden from queries");
    undo(&mut c.world);
    assert_eq!(count(&mut c), 1);
    assert_eq!(c.world.get::<Val>(e), Some(&Val(3)), "same entity id comes back");
    undo(&mut c.world); // undo the spawn
    assert_eq!(count(&mut c), 0);
    redo(&mut c.world);
    assert_eq!(c.world.get::<Val>(e), Some(&Val(3)));
}

#[test]
fn signal_edits_ripple_through_operators_and_back() {
    let mut c = core();
    let src_sig = c.world.resource_mut::<SignalStore>().create(1);
    c.world.resource_mut::<SignalStore>().get_mut(src_sig).unwrap().write(0, &vec![1.0; N as usize]);
    let src = c.world.spawn(Output(src_sig)).id();
    let off = spawn_op(&mut c.world, "offset", vec![("in", src)], Offset { k: 10.0 });
    c.run_pre_ui();
    let out = |c: &Core, f| {
        let id = c.world.get::<Output>(off).unwrap().0;
        c.world.resource::<SignalStore>().get(id).unwrap().get_valid(f).map(|v| v[0])
    };
    assert_eq!(out(&c, 500), Some(11.0));

    // Editing the source through a transaction invalidates the operator automatically.
    edit(&mut c.world, "nudge", |tx| tx.signal(src_sig).set(500, &[5.0]));
    c.run_pre_ui();
    assert_eq!(out(&c, 500), Some(15.0));
    assert_eq!(out(&c, 499), Some(11.0));

    undo(&mut c.world);
    c.run_pre_ui();
    assert_eq!(out(&c, 500), Some(11.0), "undo restores the source and the operator recomputes");

    redo(&mut c.world);
    c.run_pre_ui();
    assert_eq!(out(&c, 500), Some(15.0));

    // Parameter edits are components too: undoable, and they recompute.
    edit(&mut c.world, "k", |tx| tx.modify::<Offset>(off, |o| o.k = 0.0));
    c.run_pre_ui();
    assert_eq!(out(&c, 10), Some(1.0));
    undo(&mut c.world);
    c.run_pre_ui();
    assert_eq!(out(&c, 10), Some(11.0));

    // Deleting the source disconnects the operator; undo reconnects it.
    edit(&mut c.world, "delete source", |tx| tx.delete(src));
    c.run_pre_ui();
    assert_eq!(out(&c, 10), None);
    undo(&mut c.world);
    c.run_pre_ui();
    assert_eq!(out(&c, 10), Some(11.0));
}

#[test]
fn reflected_edits_are_undoable_and_recompute() {
    let mut c = core();
    let src_sig = c.world.resource_mut::<SignalStore>().create(1);
    c.world.resource_mut::<SignalStore>().get_mut(src_sig).unwrap().write(0, &vec![1.0; N as usize]);
    let src = c.world.spawn(Output(src_sig)).id();
    let off = spawn_op(&mut c.world, "offset", vec![("in", src)], Offset { k: 10.0 });
    c.run_pre_ui();
    let out = |c: &Core, f| {
        let id = c.world.get::<Output>(off).unwrap().0;
        c.world.resource::<SignalStore>().get(id).unwrap().get_valid(f).map(|v| v[0])
    };

    // The inspector's path: a reflected value for a type it only knows by name.
    let new_value = Offset { k: -4.0 };
    let type_path = <Offset as bevy_reflect::TypePath>::type_path();
    assert!(edit(&mut c.world, "k", |tx| {
        assert!(tx.set_reflected(off, type_path, &new_value));
    }));
    c.run_pre_ui();
    assert_eq!(c.world.get::<Offset>(off), Some(&Offset { k: -4.0 }));
    assert_eq!(out(&c, 3), Some(-3.0), "a reflected edit triggers recomputation");
    undo(&mut c.world);
    c.run_pre_ui();
    assert_eq!(c.world.get::<Offset>(off), Some(&Offset { k: 10.0 }));
    assert_eq!(out(&c, 3), Some(11.0));
    redo(&mut c.world);
    assert_eq!(c.world.get::<Offset>(off), Some(&Offset { k: -4.0 }));
    // Unknown types and missing components are refused, not recorded.
    assert!(!edit(&mut c.world, "bad", |tx| {
        assert!(!tx.set_reflected(off, "no::such::Type", &new_value));
        assert!(!tx.set_reflected(src, type_path, &new_value));
    }));
}

#[test]
fn gesture_is_one_undo_step() {
    let mut c = core();
    let e = c.world.spawn(Val(0)).id();
    c.world.resource_mut::<History>().begin("drag");
    for i in 1..=30 {
        edit(&mut c.world, "move", |tx| tx.modify::<Val>(e, |v| v.0 = i));
    }
    c.world.resource_mut::<History>().end();
    assert_eq!(c.world.resource::<History>().undo_label(), Some("drag"));
    undo(&mut c.world);
    assert_eq!(c.world.get::<Val>(e), Some(&Val(0)));
    assert!(!c.world.resource::<History>().can_undo());
}

/// Snapshot of everything the random edits touch.
fn state(c: &mut Core, entities: &[Entity], sig: tt_core::signal::SignalId) -> (Vec<Option<i32>>, Vec<Option<f32>>) {
    let vals = entities
        .iter()
        .map(|e| if c.world.get::<bevy_ecs::entity_disabling::Disabled>(*e).is_some() { None } else { c.world.get::<Val>(*e).map(|v| v.0) })
        .collect();
    let s = c.world.resource::<SignalStore>().get(sig).unwrap();
    let samples = (0..N).step_by(7).map(|f| s.get(f).map(|v| v[0])).collect();
    (vals, samples)
}

#[test]
fn random_edits_undo_all_and_redo_all() {
    let mut c = core();
    let entities: Vec<Entity> = (0..8).map(|i| c.world.spawn(Val(i)).id()).collect();
    let sig = c.world.resource_mut::<SignalStore>().create(1);
    c.world.resource_mut::<SignalStore>().get_mut(sig).unwrap().write(0, &vec![0.0; N as usize]);
    let start = state(&mut c, &entities, sig);

    let mut rng = 0x2545_f491_4f6c_dd1du64;
    let mut next = |n: u64| {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng % n
    };
    let mut steps = 0;
    for i in 0..200 {
        let e = entities[next(entities.len() as u64) as usize];
        let recorded = match next(5) {
            0 => edit(&mut c.world, "modify", |tx| tx.modify::<Val>(e, |v| v.0 = i)),
            1 => edit(&mut c.world, "insert", |tx| tx.insert(e, Val(-i))),
            2 => edit(&mut c.world, "delete", |tx| tx.delete(e)),
            3 => {
                let f = next(N as u64) as i64;
                let len = next(300) as i64 + 1;
                edit(&mut c.world, "write", |tx| tx.signal(sig).write(f, &vec![i as f32; len as usize]))
            }
            _ => {
                let f = next(N as u64) as i64;
                edit(&mut c.world, "clear", |tx| tx.signal(sig).clear(f..f + 50))
            }
        };
        steps += usize::from(recorded);
    }
    let end = state(&mut c, &entities, sig);
    assert_ne!(start, end);

    for _ in 0..steps {
        undo(&mut c.world).expect("undo step");
    }
    assert!(undo(&mut c.world).is_none());
    assert_eq!(state(&mut c, &entities, sig), start, "undo all returns to the start");
    for _ in 0..steps {
        redo(&mut c.world).expect("redo step");
    }
    assert_eq!(state(&mut c, &entities, sig), end, "redo all returns to the end");
}
