//! Project files: generic component round trip with entity remapping, signal
//! data, incremental chunk writes, deleted entities skipped, version guard.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::prelude::*;
use bevy_reflect::Reflect;
use tt_core::op::{EvalBudget, EvalCtx, Footprint, Inputs, Operator, OperatorKind, Output, spawn_op};
use tt_core::persist::{ProjectMeta, load, save};
use tt_core::signal::{Signal, SignalStore};
use tt_core::time::FrameIndex;
use tt_core::transport::Transport;
use tt_core::{AppBuilder, Class, Core, CoreModules};

const N: FrameIndex = 70_000;

#[derive(Component, Reflect, Clone, Debug, PartialEq, Default)]
#[reflect(Component)]
struct Val {
    n: i32,
    label: String,
}

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
    app.add_module(CoreModules).operator(OffsetKind).operator_params::<Offset>().component::<Val>(Class::Document);
    let mut core = app.build();
    core.world.resource_mut::<Transport>().frame_count = N;
    *core.world.resource_mut::<EvalBudget>() = EvalBudget { millis: 1e9, step_frames: 1 << 20 };
    core
}

fn temp_project() -> PathBuf {
    static COUNT: AtomicU32 = AtomicU32::new(0);
    std::env::temp_dir().join(format!("tt_test_{}_{}.ttproj", std::process::id(), COUNT.fetch_add(1, Ordering::Relaxed)))
}

fn output_values(core: &mut Core, op: Entity, frames: &[FrameIndex]) -> Vec<Option<f32>> {
    let id = core.world.get::<Output>(op).unwrap().0;
    let s = core.world.resource::<SignalStore>().get(id).unwrap();
    frames.iter().map(|f| s.get_valid(*f).map(|v| v[0])).collect()
}

#[test]
fn round_trip_with_remapped_edges() {
    let mut a = core();
    let values: Vec<f32> = (0..N).map(|f| (f % 101) as f32).collect();
    let sig = a.world.resource_mut::<SignalStore>().create(1);
    a.world.resource_mut::<SignalStore>().get_mut(sig).unwrap().write(0, &values);
    // Padding entities so saved and restored entity ids differ.
    for _ in 0..5 {
        a.world.spawn_empty();
    }
    let src = a.world.spawn((Output(sig), Val { n: 1, label: "source".into() })).id();
    let op = spawn_op(&mut a.world, "offset", vec![("in", src)], Offset { k: 0.5 });
    a.world.spawn((Val { n: 2, label: "deleted".into() }, Disabled));
    a.world.insert_resource(ProjectMeta([("media.path".to_string(), "C:/clips/p5.mp4".to_string())].into()));
    a.run_pre_ui();
    let probe = [0, 1, 999, 42_424, N - 1];
    let before = output_values(&mut a, op, &probe);

    let path = temp_project();
    let stats = save(&mut a.world, &path).unwrap();
    assert_eq!(stats.entities, 2, "source and operator; the deleted entity is not saved");
    assert_eq!(stats.signals, 2);

    let mut b = core();
    load(&mut b.world, &path).unwrap();
    assert_eq!(b.world.resource::<ProjectMeta>().0.get("media.path").map(String::as_str), Some("C:/clips/p5.mp4"));
    let vals: Vec<Val> = b.world.query::<&Val>().iter(&b.world).cloned().collect();
    assert_eq!(vals, vec![Val { n: 1, label: "source".into() }]);

    // The operator's input edge points at the *restored* source entity.
    let (op_b, inputs) = b.world.query::<(Entity, &Inputs)>().iter(&b.world).map(|(e, i)| (e, i.clone())).next().unwrap();
    assert_eq!(b.world.get::<Operator>(op_b).unwrap().kind, "offset");
    assert_eq!(b.world.get::<Offset>(op_b), Some(&Offset { k: 0.5 }));
    let src_b = inputs.0[0].1;
    assert_eq!(b.world.get::<Val>(src_b).map(|v| v.n), Some(1));
    assert_eq!(b.world.get::<Output>(src_b).unwrap().0, sig, "signal ids are preserved");

    // Source data survives; the operator recomputes to the same values.
    let s = b.world.resource::<SignalStore>().get(sig).unwrap();
    assert_eq!(s.get(42_424), Some(&[(42_424 % 101) as f32][..]));
    b.run_pre_ui();
    assert_eq!(output_values(&mut b, op_b, &probe), before);
    let _ = std::fs::remove_file(path);
}

#[test]
fn saves_are_incremental() {
    let mut a = core();
    // Identical chunks are stored once (content addressing).
    let flat = a.world.resource_mut::<SignalStore>().create(1);
    a.world.resource_mut::<SignalStore>().get_mut(flat).unwrap().write(0, &vec![1.5; 256 * 10]);
    a.world.spawn(Output(flat));
    // Every chunk of this one is different.
    let sig = a.world.resource_mut::<SignalStore>().create(2);
    let values: Vec<f32> = (0..N * 2).map(|i| i as f32).collect();
    a.world.resource_mut::<SignalStore>().get_mut(sig).unwrap().write(0, &values);
    a.world.spawn(Output(sig));
    let path = temp_project();
    let first = save(&mut a.world, &path).unwrap();
    assert_eq!(first.chunks, 10 + (N as usize).div_ceil(256));
    assert_eq!(first.new_chunks, 1 + (N as usize).div_ceil(256), "the 10 identical chunks share one blob");

    // One frame changes: one chunk of new content.
    a.world.resource_mut::<SignalStore>().get_mut(sig).unwrap().set(12_345, &[9.0, 9.0]);
    let second = save(&mut a.world, &path).unwrap();
    assert_eq!(second.chunks, first.chunks);
    assert_eq!(second.new_chunks, 1, "only the edited chunk is written");

    // Nothing changed: nothing written.
    let third = save(&mut a.world, &path).unwrap();
    assert_eq!(third.new_chunks, 0);
    let _ = std::fs::remove_file(path);
}

#[test]
fn newer_format_is_refused() {
    let mut a = core();
    let path = temp_project();
    save(&mut a.world, &path).unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute("UPDATE meta SET value = '999' WHERE key = 'version'", []).unwrap();
    drop(conn);
    let mut b = core();
    let err = load(&mut b.world, &path).unwrap_err().to_string();
    assert!(err.contains("newer version"), "{err}");
    let _ = std::fs::remove_file(path);
}

#[test]
fn a_view_saved_before_a_setting_existed_still_loads() {
    use bevy_ecs::reflect::AppTypeRegistry;
    use bevy_reflect::FromReflect;
    use bevy_reflect::serde::TypedReflectDeserializer;
    use serde::de::DeserializeSeed;
    use tt_core::view::FrameParams;

    let mut app = AppBuilder::new();
    app.add_module(CoreModules);
    let core = app.build();
    let registry = core.world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let registration = registry.get_with_type_path(std::any::type_name::<FrameParams>()).expect("registered");
    // FrameParams as saved before `lead` existed.
    let old = "(fit: 0.5, hold: 2.0, pan_damping: 0.1, zoom_damping: 0.5, dead_zone: 0.0, follow: 1.0, zoom: 1.0, min_zoom: 1.0, max_zoom: 32.0)";
    let mut de = ron::Deserializer::from_str(old).unwrap();
    let value = TypedReflectDeserializer::new(registration, &registry).deserialize(&mut de).expect("an old save deserializes");
    let p = FrameParams::from_reflect(value.as_ref()).expect("and converts");
    assert_eq!((p.fit, p.hold, p.lead), (0.5, 2.0, FrameParams::default().lead), "saved values kept, the new one defaulted");
}
