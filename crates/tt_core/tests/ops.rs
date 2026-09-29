//! Operator graph: footprint-exact invalidation, ordered budgeted evaluation,
//! parameter changes, cycles.

use std::ops::Range;

use bevy_ecs::prelude::*;
use bevy_reflect::Reflect;
use tt_core::op::{EvalBudget, EvalCtx, Footprint, Invalidations, OpError, OperatorKind, Output, spawn_op};
use tt_core::signal::{FrameState, Signal, SignalStore};
use tt_core::time::FrameIndex;
use tt_core::transport::Transport;
use tt_core::{AppBuilder, Core, CoreModules};

const N: FrameIndex = 600;

#[derive(Component, Reflect, Default)]
#[reflect(Component)]
struct Offset {
    k: f32,
}

#[derive(Component, Reflect, Default)]
#[reflect(Component)]
struct Blur {
    radius: FrameIndex,
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

struct BlurKind;
impl OperatorKind for BlurKind {
    fn name(&self) -> &'static str {
        "blur"
    }
    fn channels(&self) -> usize {
        1
    }
    fn footprint(&self, op: EntityRef<'_>) -> Footprint {
        let r = op.get::<Blur>().map_or(0, |b| b.radius);
        Footprint::Window { before: r, after: r }
    }
    fn evaluate(&self, ctx: &EvalCtx<'_>, range: Range<FrameIndex>, out: &mut Signal) -> anyhow::Result<()> {
        let r = ctx.params::<Blur>().map_or(0, |b| b.radius);
        let input = ctx.input("in").expect("connected");
        for f in range {
            let vals: Vec<f32> = (f - r..=f + r).filter_map(|g| input.get_valid(g).map(|v| v[0])).collect();
            if vals.is_empty() {
                out.clear(f..f + 1);
            } else {
                out.set(f, &[vals.iter().sum::<f32>() / vals.len() as f32]);
            }
        }
        Ok(())
    }
}

struct CumSumKind;
impl OperatorKind for CumSumKind {
    fn name(&self) -> &'static str {
        "cumsum"
    }
    fn channels(&self) -> usize {
        1
    }
    fn footprint(&self, _: EntityRef<'_>) -> Footprint {
        Footprint::Causal
    }
    fn evaluate(&self, ctx: &EvalCtx<'_>, range: Range<FrameIndex>, out: &mut Signal) -> anyhow::Result<()> {
        let input = ctx.input("in").expect("connected");
        let mut acc = if range.start > ctx.extent.start {
            out.get_valid(range.start - 1).map(|v| v[0]).expect("previous frame valid (front-to-back evaluation)")
        } else {
            0.0
        };
        for f in range {
            acc += input.get_valid(f).map_or(0.0, |v| v[0]);
            out.set(f, &[acc]);
        }
        Ok(())
    }
}

struct Chain {
    core: Core,
    src: Entity,
    off: Entity,
    blur: Entity,
    cum: Entity,
}

fn chain() -> Chain {
    let mut app = AppBuilder::new();
    app.add_module(CoreModules)
        .operator(OffsetKind)
        .operator_params::<Offset>()
        .operator(BlurKind)
        .operator_params::<Blur>()
        .operator(CumSumKind);
    let mut core = app.build();
    core.world.resource_mut::<Transport>().frame_count = N;
    *core.world.resource_mut::<EvalBudget>() = EvalBudget { millis: 1e9, step_frames: 4096 };

    let values: Vec<f32> = (0..N).map(|f| (f % 17) as f32).collect();
    let sig = core.world.resource_mut::<SignalStore>().create(1);
    core.world.resource_mut::<SignalStore>().get_mut(sig).unwrap().write(0, &values);
    let src = core.world.spawn(Output(sig)).id();
    let off = spawn_op(&mut core.world, "offset", vec![("in", src)], Offset { k: 10.0 });
    let blur = spawn_op(&mut core.world, "blur", vec![("in", off)], Blur { radius: 2 });
    let cum = spawn_op(&mut core.world, "cumsum", vec![("in", blur)], ());
    core.run_pre_ui();
    Chain { core, src, off, blur, cum }
}

fn signal(core: &Core, e: Entity) -> &Signal {
    let id = core.world.get::<Output>(e).unwrap().0;
    core.world.resource::<SignalStore>().get(id).unwrap()
}

/// Reference: the whole pipeline computed directly.
fn expected(values: &[f32], k: f32, r: i64) -> Vec<f32> {
    let off: Vec<f32> = values.iter().map(|v| v + k).collect();
    let n = off.len() as i64;
    let blur: Vec<f32> = (0..n)
        .map(|f| {
            let v: Vec<f32> = (f - r..=f + r).filter(|g| (0..n).contains(g)).map(|g| off[g as usize]).collect();
            v.iter().sum::<f32>() / v.len() as f32
        })
        .collect();
    blur.iter().scan(0.0, |acc, v| {
        *acc += v;
        Some(*acc)
    }).collect()
}

fn stale(core: &Core, e: Entity) -> Vec<Range<FrameIndex>> {
    signal(core, e).runs(0..N).into_iter().filter(|(_, s)| *s == FrameState::Stale).map(|(r, _)| r).collect()
}

fn source_values(core: &Core, src: Entity) -> Vec<f32> {
    let s = signal(core, src);
    (0..N).map(|f| s.get(f).unwrap()[0]).collect()
}

fn assert_matches(c: &Chain, k: f32, r: i64) {
    let want = expected(&source_values(&c.core, c.src), k, r);
    let got = signal(&c.core, c.cum);
    for f in 0..N {
        let v = got.get_valid(f).unwrap_or_else(|| panic!("frame {f} not valid"))[0];
        assert!((v - want[f as usize]).abs() < 1e-2, "frame {f}: {v} vs {}", want[f as usize]);
    }
}

#[test]
fn full_pipeline_computes_on_creation() {
    let c = chain();
    assert_matches(&c, 10.0, 2);
}

#[test]
fn source_edit_invalidates_exactly_the_footprints() {
    let mut c = chain();
    let sig = c.core.world.get::<Output>(c.src).unwrap().0;
    c.core.world.resource_mut::<SignalStore>().get_mut(sig).unwrap().set(50, &[100.0]);
    c.core.world.resource_mut::<Invalidations>().output_changed(c.src, 50..51);

    // Propagate without evaluating: check what became stale.
    c.core.world.resource_mut::<EvalBudget>().millis = 0.0;
    c.core.run_pre_ui();
    assert_eq!(stale(&c.core, c.off), vec![50..51]);
    assert_eq!(stale(&c.core, c.blur), vec![48..53]);
    assert_eq!(stale(&c.core, c.cum), vec![48..N]);

    // Evaluate: everything valid again and equal to a from-scratch computation.
    c.core.world.resource_mut::<EvalBudget>().millis = 1e9;
    c.core.run_pre_ui();
    assert!(stale(&c.core, c.cum).is_empty());
    assert_matches(&c, 10.0, 2);
}

#[test]
fn parameter_change_recomputes() {
    let mut c = chain();
    c.core.world.get_mut::<Offset>(c.off).unwrap().k = -3.0;
    c.core.world.get_mut::<Blur>(c.blur).unwrap().radius = 5;
    c.core.run_pre_ui();
    assert_matches(&c, -3.0, 5);
}

#[test]
fn tiny_budget_converges_upstream_first() {
    let mut c = chain();
    let sig = c.core.world.get::<Output>(c.src).unwrap().0;
    c.core.world.resource_mut::<SignalStore>().get_mut(sig).unwrap().write(100, &[7.0; 50]);
    c.core.world.resource_mut::<Invalidations>().output_changed(c.src, 100..150);
    *c.core.world.resource_mut::<EvalBudget>() = EvalBudget { millis: 1e9, step_frames: 16 };
    // With 16-frame steps the budget is spent per op step; run until done.
    for _ in 0..200 {
        c.core.run_pre_ui();
    }
    assert_matches(&c, 10.0, 2);
}

#[test]
fn cycles_are_flagged_and_skipped() {
    let mut c = chain();
    let a = spawn_op(&mut c.core.world, "offset", vec![], Offset { k: 1.0 });
    let b = spawn_op(&mut c.core.world, "offset", vec![("in", a)], Offset { k: 1.0 });
    c.core.world.get_mut::<tt_core::op::Inputs>(a).unwrap().0.push(("in".into(), b));
    c.core.run_pre_ui();
    assert!(c.core.world.get::<OpError>(a).is_some());
    assert!(c.core.world.get::<OpError>(b).is_some());
    // The rest of the graph is unaffected.
    assert!(c.core.world.get::<OpError>(c.cum).is_none());
    assert_matches(&c, 10.0, 2);
}
