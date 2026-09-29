//! Operators (DESIGN §6): every transformation is an entity with a kind, its
//! parameters (ordinary reflected components), input edges to *producer*
//! entities, and an output signal.
//!
//! - A producer is any entity with an [`Output`]: an operator, or a source
//!   such as a capture or a set of keys.
//! - Each kind declares a [`Footprint`]: how far an input change reaches into
//!   its output. Invalidation is computed from footprints, never hand-written:
//!   an edit on frames `a..b` marks exactly the affected downstream frames
//!   stale and dirty.
//! - Evaluation recomputes dirty frames in topological order (inputs before
//!   dependents), front to back, within a time budget per app frame; the rest
//!   carries over, shown as stale until done.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use std::time::Instant;

use bevy_ecs::prelude::*;
use bevy_reflect::Reflect;

use crate::app::{AppBuilder, Module, Set};
use crate::meta::Class;
use crate::ranges::RangeSet;
use crate::signal::{Signal, SignalId, SignalStore};
use crate::time::FrameIndex;
use crate::transport::Transport;

/// How an output frame depends on input frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Footprint {
    /// Output f depends on input f.
    Pointwise,
    /// Output f depends on inputs `f - before ..= f + after`.
    Window { before: FrameIndex, after: FrameIndex },
    /// Output f depends on inputs at or before f (trackers, integrators).
    Causal,
    /// Output f depends on inputs at or after f (reverse trackers).
    AntiCausal,
    /// Every output frame depends on every input frame (fits, max-over-pass).
    Global,
    /// Output spreads both ways from an anchor frame (a tracker running
    /// forward and backward from where it was seeded): frames after the
    /// anchor depend on inputs from the anchor up to them, frames before it
    /// on inputs from them up to the anchor.
    Radiating(FrameIndex),
}

impl Footprint {
    /// Output frames affected when input frames `dirty` change, within `extent`.
    pub fn map(self, dirty: Range<FrameIndex>, extent: &Range<FrameIndex>) -> Range<FrameIndex> {
        let r = match self {
            Footprint::Pointwise => dirty,
            // Input g affects outputs f with f - before <= g <= f + after.
            Footprint::Window { before, after } => (dirty.start - after)..(dirty.end + before),
            Footprint::Causal => dirty.start..extent.end,
            Footprint::AntiCausal => extent.start..dirty.end,
            Footprint::Global => extent.clone(),
            Footprint::Radiating(anchor) => {
                let start = if dirty.start <= anchor { extent.start } else { dirty.start };
                let end = if dirty.end > anchor { extent.end } else { dirty.end };
                start..end
            }
        };
        r.start.max(extent.start)..r.end.min(extent.end)
    }
}

/// What a kind of operator does. Registered once per kind; the parameters
/// live on each operator entity as ordinary components.
pub trait OperatorKind: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    /// Output channel count.
    fn channels(&self) -> usize;
    /// May depend on the entity's parameters (e.g. a smoothing window).
    fn footprint(&self, op: EntityRef<'_>) -> Footprint;
    /// Compute `range` of the output into `out` (which holds the previous
    /// values, e.g. `out[range.start - 1]` for causal kinds). Frames with no
    /// result must be cleared. Ranges arrive front to back.
    fn evaluate(&self, ctx: &EvalCtx<'_>, range: Range<FrameIndex>, out: &mut Signal) -> anyhow::Result<()>;
    /// Whether this kind is too slow to evaluate inline (a tracker reading
    /// pixels): its dirty frames are left for a job system to take, and its
    /// dependents run on whatever it has so far (stale-while-revalidate).
    fn job(&self) -> bool {
        false
    }
}

/// What an operator sees while evaluating.
pub struct EvalCtx<'w> {
    pub world: &'w World,
    pub entity: Entity,
    pub store: &'w SignalStore,
    pub extent: Range<FrameIndex>,
    /// Inputs trimmed by a span (`span::Span`), as the operator sees them.
    clipped: HashMap<Entity, Signal>,
}

impl EvalCtx<'_> {
    /// The signal connected to input `slot`, without the frames outside its producer's span.
    pub fn input(&self, slot: &str) -> Option<&Signal> {
        let inputs = self.world.get::<Inputs>(self.entity)?;
        let (_, producer) = inputs.0.iter().find(|(s, _)| s == slot)?;
        // A deleted (disabled) producer counts as disconnected.
        if self.world.get::<bevy_ecs::entity_disabling::Disabled>(*producer).is_some() {
            return None;
        }
        if let Some(s) = self.clipped.get(producer) {
            return Some(s);
        }
        let out = self.world.get::<Output>(*producer)?;
        self.store.get(out.0)
    }

    /// Copies of the inputs of `op` that a span trims (cheap: shared chunks).
    fn clip_inputs(world: &World, store: &SignalStore, op: Entity) -> HashMap<Entity, Signal> {
        let mut out = HashMap::new();
        for (_, p) in world.get::<Inputs>(op).map(|i| i.0.as_slice()).unwrap_or_default() {
            let span = crate::span::span_of(world, *p);
            if span.is_trimmed()
                && let Some(sig) = world.get::<Output>(*p).and_then(|o| store.get(o.0))
            {
                out.insert(*p, sig.clipped(span.range()));
            }
        }
        out
    }

    /// The producer entity connected to input `slot` (None if disconnected or deleted).
    pub fn input_entity(&self, slot: &str) -> Option<Entity> {
        let inputs = self.world.get::<Inputs>(self.entity)?;
        let (_, producer) = inputs.0.iter().find(|(s, _)| s == slot)?;
        (self.world.get::<bevy_ecs::entity_disabling::Disabled>(*producer).is_none()).then_some(*producer)
    }

    pub fn params<P: Component>(&self) -> Option<&P> {
        self.world.get::<P>(self.entity)
    }
}

/// Marks an operator entity and names its kind. Every operator carries a
/// [`Dirty`] set (required component), including ones restored from a file.
#[derive(Component, Reflect, Debug, Clone)]
#[reflect(Component)]
#[require(Dirty)]
pub struct Operator {
    pub kind: String,
}

/// Input edges: slot name → producer entity.
#[derive(Component, Reflect, Debug, Clone, Default)]
#[reflect(Component)]
pub struct Inputs(pub Vec<(String, Entity)>);

/// The signal a producer (operator or source) exposes.
#[derive(Component, Reflect, Debug, Clone, Copy)]
#[reflect(Component)]
pub struct Output(pub SignalId);

/// Frames of this operator's output awaiting recomputation (derived).
#[derive(Component, Debug, Default)]
pub struct Dirty(pub RangeSet);

/// Why an operator can't run (cycle, missing input, evaluation error).
#[derive(Component, Debug, Clone)]
pub struct OpError(pub String);

#[derive(Resource, Default)]
pub struct OpRegistry {
    kinds: HashMap<String, Arc<dyn OperatorKind>>,
}

impl OpRegistry {
    pub fn get(&self, name: &str) -> Option<Arc<dyn OperatorKind>> {
        self.kinds.get(name).cloned()
    }
}

/// Changes waiting to propagate: `(producer, frames, recompute_self)`.
/// `recompute_self` = the producer's own output must be recomputed (its
/// parameters or inputs changed); otherwise its output already changed
/// (a source was edited) and only dependents are affected.
#[derive(Resource, Default)]
pub struct Invalidations(pub Vec<(Entity, Range<FrameIndex>, bool)>);

impl Invalidations {
    /// A producer's output changed on `frames` (sources call this after editing).
    pub fn output_changed(&mut self, producer: Entity, frames: Range<FrameIndex>) {
        self.0.push((producer, frames, false));
    }

    /// An operator must recompute `frames` of its output.
    pub fn recompute(&mut self, op: Entity, frames: Range<FrameIndex>) {
        self.0.push((op, frames, true));
    }
}

/// Per-app-frame evaluation budget.
#[derive(Resource, Debug, Clone, Copy)]
pub struct EvalBudget {
    pub millis: f64,
    /// Frames evaluated per call, so a long range doesn't blow the budget in one go.
    pub step_frames: FrameIndex,
}

impl Default for EvalBudget {
    fn default() -> Self {
        Self { millis: 4.0, step_frames: 2048 }
    }
}

/// Topological order and reverse edges, rebuilt when edges change.
#[derive(Resource, Default, Debug)]
pub struct OpGraph {
    /// Operators, inputs before dependents.
    pub order: Vec<Entity>,
    /// Producer → operators that read it.
    pub dependents: HashMap<Entity, Vec<Entity>>,
    stale: bool,
}

impl OpGraph {
    /// Force a rebuild (entities enabled/disabled, edges edited outside `Inputs` change detection).
    pub fn mark_stale(&mut self) {
        self.stale = true;
    }
}

/// The frame range signals cover (the media's grid).
pub fn extent(world: &World) -> Range<FrameIndex> {
    0..world.get_resource::<Transport>().map_or(0, |t| t.frame_count)
}

/// Spawn an operator of a registered kind with its output signal.
pub fn spawn_op(world: &mut World, kind: &str, inputs: Vec<(&str, Entity)>, params: impl Bundle) -> Entity {
    let k = world.resource::<OpRegistry>().get(kind).unwrap_or_else(|| panic!("operator kind {kind} not registered"));
    let signal = world.resource_mut::<SignalStore>().create(k.channels());
    world
        .spawn((
            Operator { kind: kind.to_string() },
            Inputs(inputs.into_iter().map(|(s, e)| (s.to_string(), e)).collect()),
            Output(signal),
            Dirty::default(),
            params,
        ))
        .id()
}

fn mark_graph_stale(changed: Query<(), Changed<Inputs>>, mut removed: RemovedComponents<Inputs>, mut graph: ResMut<OpGraph>) {
    if !changed.is_empty() || removed.read().next().is_some() {
        graph.stale = true;
    }
}

/// New or rewired operators recompute everything.
fn recompute_rewired(ops: Query<Entity, (With<Operator>, Changed<Inputs>)>, mut inv: ResMut<Invalidations>, t: Res<Transport>) {
    for e in &ops {
        inv.recompute(e, 0..t.frame_count);
    }
}

/// Parameter changes recompute the operator's whole output. Registered per
/// parameter type by [`AppBuilder::operator_params`].
pub fn recompute_on_change<P: Component>(ops: Query<Entity, (With<Operator>, Changed<P>)>, mut inv: ResMut<Invalidations>, t: Res<Transport>) {
    for e in &ops {
        inv.recompute(e, 0..t.frame_count);
    }
}

fn rebuild_graph(world: &mut World) {
    if !world.resource::<OpGraph>().stale {
        return;
    }
    let mut q = world.query::<(Entity, &Inputs)>();
    let edges: Vec<(Entity, Vec<Entity>)> =
        q.iter(world).map(|(e, i)| (e, i.0.iter().map(|(_, p)| *p).collect())).collect();
    let mut dependents: HashMap<Entity, Vec<Entity>> = HashMap::new();
    let mut indegree: HashMap<Entity, usize> = HashMap::new();
    let ops: std::collections::HashSet<Entity> = edges.iter().map(|(e, _)| *e).collect();
    for (op, inputs) in &edges {
        indegree.entry(*op).or_insert(0);
        for p in inputs {
            dependents.entry(*p).or_default().push(*op);
            if ops.contains(p) {
                *indegree.entry(*op).or_insert(0) += 1;
            }
        }
    }
    // Kahn's algorithm; deterministic by entity order.
    let mut ready: Vec<Entity> = indegree.iter().filter(|(_, d)| **d == 0).map(|(e, _)| *e).collect();
    ready.sort();
    let mut order = Vec::with_capacity(ops.len());
    while let Some(e) = ready.pop() {
        order.push(e);
        if let Some(ds) = dependents.get(&e) {
            let mut next: Vec<Entity> = Vec::new();
            for d in ds {
                let deg = indegree.get_mut(d).unwrap();
                *deg -= 1;
                if *deg == 0 {
                    next.push(*d);
                }
            }
            next.sort();
            ready.extend(next.into_iter().rev());
        }
    }
    let cyclic: Vec<Entity> = ops.iter().filter(|e| !order.contains(e)).copied().collect();
    for e in &ops {
        if cyclic.contains(e) {
            world.entity_mut(*e).insert(OpError("part of a cycle".into()));
        } else if world.get::<OpError>(*e).is_some_and(|err| err.0 == "part of a cycle") {
            world.entity_mut(*e).remove::<OpError>();
        }
    }
    let mut graph = world.resource_mut::<OpGraph>();
    graph.order = order;
    graph.dependents = dependents;
    graph.stale = false;
}

/// Propagate queued changes through the graph (topological order), marking
/// affected output frames stale and dirty.
pub(crate) fn propagate(world: &mut World) {
    let queued = std::mem::take(&mut world.resource_mut::<Invalidations>().0);
    if queued.is_empty() {
        return;
    }
    let extent = extent(world);
    // Output frames known to change, per producer.
    let mut changed: HashMap<Entity, RangeSet> = HashMap::new();
    for (e, r, recompute) in queued {
        let r = r.start.max(extent.start)..r.end.min(extent.end);
        if r.is_empty() {
            continue;
        }
        if recompute {
            mark_dirty(world, e, r.clone());
        }
        changed.entry(e).or_default().insert(r);
    }
    let registry = world.resource::<OpRegistry>().kinds.clone();
    let order = world.resource::<OpGraph>().order.clone();
    // Sources (non-operators) first, then operators in dependency order.
    let mut sequence: Vec<Entity> = changed.keys().filter(|e| !order.contains(e)).copied().collect();
    sequence.sort();
    sequence.extend(order.iter().copied());
    for producer in sequence {
        let Some(out_changed) = changed.get(&producer).cloned() else { continue };
        let deps = world.resource::<OpGraph>().dependents.get(&producer).cloned().unwrap_or_default();
        for d in deps {
            let Some(kind) = world.get::<Operator>(d).and_then(|o| registry.get(&o.kind).cloned()) else { continue };
            let fp = kind.footprint(world.entity(d));
            let affected = out_changed.map(|r| fp.map(r, &extent));
            for r in affected.ranges() {
                mark_dirty(world, d, r.clone());
            }
            changed.entry(d).or_default().union(&affected);
        }
    }
}

fn mark_dirty(world: &mut World, op: Entity, r: Range<FrameIndex>) {
    let Some(out) = world.get::<Output>(op).copied() else { return };
    if let Some(sig) = world.resource_mut::<SignalStore>().get_mut(out.0) {
        sig.mark_stale(r.clone());
    }
    if let Some(mut d) = world.get_mut::<Dirty>(op) {
        d.0.insert(r);
    }
}

/// Recompute dirty frames, upstream first, within the budget.
fn evaluate(world: &mut World) {
    let budget = *world.resource::<EvalBudget>();
    let start = Instant::now();
    let extent = extent(world);
    let order = world.resource::<OpGraph>().order.clone();
    let registry = world.resource::<OpRegistry>().kinds.clone();
    let mut blocked: std::collections::HashSet<Entity> = std::collections::HashSet::new();
    for op in order {
        let has_dirty = world.get::<Dirty>(op).is_some_and(|d| !d.0.is_empty());
        let inputs: Vec<Entity> = world.get::<Inputs>(op).map(|i| i.0.iter().map(|(_, p)| *p).collect()).unwrap_or_default();
        // Wait for inputs that are still being recomputed (partial budget).
        if inputs.iter().any(|p| blocked.contains(p)) {
            if has_dirty {
                blocked.insert(op);
            }
            continue;
        }
        if !has_dirty || world.get::<OpError>(op).is_some_and(|e| e.0 == "part of a cycle") {
            continue;
        }
        let Some(kind) = world.get::<Operator>(op).and_then(|o| registry.get(&o.kind).cloned()) else { continue };
        if kind.job() {
            continue;
        }
        let Some(out_id) = world.get::<Output>(op).map(|o| o.0) else { continue };
        // A global kind recomputes everything whatever the range, so it takes the whole hull in one call.
        let global = kind.footprint(world.entity(op)) == Footprint::Global;
        while start.elapsed().as_secs_f64() * 1e3 < budget.millis {
            let take = |mut d: Mut<Dirty>| if global { d.0.hull().inspect(|_| d.0 = RangeSet::default()) } else { d.0.take_front(budget.step_frames) };
            let Some(range) = world.get_mut::<Dirty>(op).and_then(take) else { break };
            let range = range.start.max(extent.start)..range.end.min(extent.end);
            if range.is_empty() {
                continue;
            }
            let Some(mut out) = world.resource_mut::<SignalStore>().remove(out_id) else { break };
            let result = {
                let store = world.resource::<SignalStore>();
                let clipped = EvalCtx::clip_inputs(world, store, op);
                let ctx = EvalCtx { world, entity: op, store, extent: extent.clone(), clipped };
                kind.evaluate(&ctx, range.clone(), &mut out)
            };
            world.resource_mut::<SignalStore>().insert(out_id, out);
            match result {
                Ok(()) => {
                    if world.get::<OpError>(op).is_some() {
                        world.entity_mut(op).remove::<OpError>();
                    }
                }
                Err(e) => {
                    world.entity_mut(op).insert(OpError(format!("{e:#}")));
                    break;
                }
            }
        }
        if world.get::<Dirty>(op).is_some_and(|d| !d.0.is_empty()) {
            blocked.insert(op);
        }
    }
}

pub struct OpsModule;

impl Module for OpsModule {
    fn build(&self, app: &mut AppBuilder) {
        app.component::<Operator>(Class::Document)
            .component::<Inputs>(Class::Document)
            .component::<Output>(Class::Document)
            .declare::<Dirty>(Class::Derived)
            .declare::<OpError>(Class::Derived)
            .declare::<SignalStore>(Class::Document)
            .init_resource::<SignalStore>()
            .init_resource::<OpRegistry>()
            .init_resource::<Invalidations>()
            .init_resource::<EvalBudget>()
            .init_resource::<OpGraph>()
            .add_systems(
                (mark_graph_stale, recompute_rewired, rebuild_graph, propagate).chain().in_set(Set::Invalidate),
            )
            .add_systems(evaluate.in_set(Set::Evaluate));
    }
}

impl AppBuilder {
    /// Register an operator kind.
    pub fn operator(&mut self, kind: impl OperatorKind) -> &mut Self {
        let name = kind.name().to_string();
        self.world_mut().resource_mut::<OpRegistry>().kinds.insert(name, Arc::new(kind));
        self
    }

    /// Register an operator parameter component: reflected (inspector, undo,
    /// save) and watched, so edits recompute the operator.
    pub fn operator_params<P: Component + bevy_reflect::GetTypeRegistration>(&mut self) -> &mut Self {
        self.component::<P>(Class::Document).add_systems(recompute_on_change::<P>.in_set(Set::Invalidate).before(propagate))
    }
}
