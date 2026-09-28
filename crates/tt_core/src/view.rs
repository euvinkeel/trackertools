//! Derived views (DESIGN §10): a sketch's region becomes a virtual camera.
//!
//! - The `frame` operator turns a sketch (input "box") into a per-frame crop
//!   of the source: `[cx, cy, crop_w, crop_h, canvas_w, canvas_h]`, all in
//!   source pixels. The *canvas* is the view's own pixel grid, as big as its
//!   largest crop ("display size = max size"), so 1 view pixel = 1 source
//!   pixel at the widest framing and the view magnifies as the crop shrinks.
//! - Every view maps straight to the source ([`SpaceMap`]); nesting is
//!   provenance, not a transform chain. A sketch drawn inside view V records
//!   V's mapping for every frame it touched (`Through`), so its path lives in
//!   source pixels and re-tuning V later never moves it. Its framing may
//!   still lean on V (the `parent` input: influence and zoom limits).
//! - [`ActiveView`] is what the viewport shows (None = the source). Tab
//!   enters the selected sketch's view (creating it on first use, undoably)
//!   and clears the selection, so the next hold nests a new sketch there;
//!   Shift+Tab goes back to its parent and selects the sketch just left.

use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::name::Name;
use bevy_ecs::prelude::*;
use bevy_reflect::Reflect;

use crate::app::{AppBuilder, Module, Set};
use crate::history::edit;
use crate::input::{Action, PendingActions};
use crate::meta::Class;
use crate::op::{EvalCtx, Footprint, Inputs, Operator, OperatorKind, Output};
use crate::selection::Selection;
use crate::signal::{Signal, SignalStore};
use crate::sketch::{gauss, gauss_trend, sketch_of};
use crate::time::FrameIndex;
use crate::transport::Transport;

/// Channels of a view: `[cx, cy, crop_w, crop_h, canvas_w, canvas_h]` (source px).
pub const VIEW_CHANNELS: usize = 6;

/// The source video's size in pixels (set by the host when media opens).
#[derive(Resource, Debug, Clone, Copy)]
pub struct SourceSize {
    pub width: f64,
    pub height: f64,
}

impl Default for SourceSize {
    fn default() -> Self {
        Self { width: 1920.0, height: 1080.0 }
    }
}

/// What the viewport shows: a view (a `frame` operator) or, with None, the source.
#[derive(Resource, Debug, Default, Clone, Copy, PartialEq)]
pub struct ActiveView(pub Option<Entity>);

/// A space's pixels → source pixels at one frame: `source = a · p + b`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpaceMap {
    pub a: f64,
    pub b: [f64; 2],
    /// The space's canvas size (its pixel grid).
    pub canvas: [f64; 2],
}

impl SpaceMap {
    pub fn identity(size: &SourceSize) -> Self {
        Self { a: 1.0, b: [0.0, 0.0], canvas: [size.width, size.height] }
    }

    /// From a view's value `[cx, cy, crop_w, crop_h, canvas_w, canvas_h]`.
    pub fn of_view(v: &[f32]) -> Self {
        let [cx, cy, cw, _, w, h] = [v[0], v[1], v[2], v[3], v[4], v[5]].map(|x| x as f64);
        let a = cw / w;
        Self { a, b: [cx - a * w / 2.0, cy - a * h / 2.0], canvas: [w, h] }
    }

    pub fn to_source(&self, p: [f64; 2]) -> [f64; 2] {
        [self.a * p[0] + self.b[0], self.a * p[1] + self.b[1]]
    }

    pub fn from_source(&self, p: [f64; 2]) -> [f64; 2] {
        [(p[0] - self.b[0]) / self.a, (p[1] - self.b[1]) / self.a]
    }

    /// A box `[x, y, left, top, right, bottom]` in this space → source.
    pub fn box_to_source(&self, v: [f64; 6]) -> [f64; 6] {
        let (a, [bx, by]) = (self.a, self.b);
        [a * v[0] + bx, a * v[1] + by, a * v[2] + bx, a * v[3] + by, a * v[4] + bx, a * v[5] + by]
    }

    /// A box in source → this space.
    pub fn box_from_source(&self, v: [f64; 6]) -> [f64; 6] {
        let (a, [bx, by]) = (self.a, self.b);
        [(v[0] - bx) / a, (v[1] - by) / a, (v[2] - bx) / a, (v[3] - by) / a, (v[4] - bx) / a, (v[5] - by) / a]
    }

    /// As channels `[a, bx, by]` (a capture's `Through` signal).
    pub fn channels(&self) -> [f32; 3] {
        [self.a as f32, self.b[0] as f32, self.b[1] as f32]
    }
}

/// A view's mapping at `f` (None = the source). Outside the frames its sketch
/// covers, a view holds the nearest framing, so it never vanishes.
pub fn map_at(world: &World, view: Option<Entity>, f: FrameIndex) -> SpaceMap {
    let size = world.get_resource::<SourceSize>().copied().unwrap_or_default();
    let Some(sig) = view.filter(|v| is_live(world, *v)).and_then(|v| signal_of(world, v)) else { return SpaceMap::identity(&size) };
    match nearest(sig, f) {
        Some(v) => SpaceMap::of_view(v),
        None => SpaceMap::identity(&size),
    }
}

/// The value at `f`, or at the nearest frame that has one.
fn nearest(sig: &Signal, f: FrameIndex) -> Option<&[f32]> {
    if let Some(v) = sig.get(f) {
        return Some(v);
    }
    // A view covers one contiguous range: outside it, its nearest end.
    let (lo, hi) = sig.present_hull()?;
    if let Some(v) = sig.get(f.clamp(lo, hi)) {
        return Some(v);
    }
    let runs = sig.runs(FrameIndex::MIN / 4..FrameIndex::MAX / 4);
    let present = runs.iter().filter(|(_, s)| *s != crate::signal::FrameState::Absent).map(|(r, _)| r.clone());
    let best = present.map(|r| if f < r.start { (r.start - f, r.start) } else { (f - (r.end - 1), r.end - 1) }).min_by_key(|(d, _)| *d)?;
    sig.get(best.1)
}

fn signal_of(world: &World, e: Entity) -> Option<&Signal> {
    world.resource::<SignalStore>().get(world.get::<Output>(e)?.0)
}

fn is_live(world: &World, e: Entity) -> bool {
    world.get_entity(e).is_ok_and(|r| !r.contains::<Disabled>())
}

/// Whether `e` is a view (a `frame` operator).
pub fn is_view(world: &World, e: Entity) -> bool {
    world.get::<Operator>(e).is_some_and(|o| o.kind == "frame")
}

/// The view framing `sketch`, if there is one.
pub fn view_of(world: &mut World, sketch: Entity) -> Option<Entity> {
    let mut q = world.query::<(Entity, &Operator, &Inputs)>();
    q.iter(world).find(|(_, o, i)| o.kind == "frame" && i.0.iter().any(|(s, p)| s == "box" && *p == sketch)).map(|(e, _, _)| e)
}

/// The sketch a view frames.
pub fn sketch_framed(world: &World, view: Entity) -> Option<Entity> {
    world.get::<Inputs>(view)?.0.iter().find(|(s, _)| s == "box").map(|(_, e)| *e)
}

/// The view a sketch was drawn in (its home space); None = the source.
pub fn home_of(world: &World, sketch: Entity) -> Option<Entity> {
    world.get::<Inputs>(sketch)?.0.iter().find(|(s, _)| s == "space").map(|(_, e)| *e).filter(|e| is_live(world, *e))
}

/// A view's parent view (the home of the sketch it frames); None = the source.
pub fn parent_of(world: &World, view: Entity) -> Option<Entity> {
    home_of(world, sketch_framed(world, view)?)
}

/// The views from the outermost down to `view`.
pub fn chain(world: &World, view: Option<Entity>) -> Vec<Entity> {
    let mut out = Vec::new();
    let mut v = view;
    while let Some(e) = v {
        if out.contains(&e) || out.len() > 64 {
            break;
        }
        out.push(e);
        v = parent_of(world, e);
    }
    out.reverse();
    out
}

/// The view framing `sketch`, created (as one undo step) if it has none yet.
pub fn ensure_view(world: &mut World, sketch: Entity) -> Entity {
    if let Some(v) = view_of(world, sketch) {
        return v;
    }
    let name = world.get::<Name>(sketch).map_or("sketch".to_string(), |n| n.to_string());
    let home = home_of(world, sketch);
    let params = world.get_resource::<ViewDefaults>().map(|d| d.params.clone()).unwrap_or_default();
    let mut view = None;
    edit(world, &format!("View of {name}"), |tx| {
        let out = tx.create_signal(VIEW_CHANNELS);
        let mut inputs = vec![("box".to_string(), sketch)];
        inputs.extend(home.map(|h| ("parent".to_string(), h)));
        view = Some(tx.spawn((Name::new(format!("{name} view")), Operator { kind: "frame".into() }, Inputs(inputs), Output(out), params)));
    });
    view.expect("view created")
}

// ---- the frame operator --------------------------------------------------------------------

/// How a view frames its sketch (Cinemachine's framing controls, DESIGN §10.1).
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component)]
pub struct FrameParams {
    /// How much of the view the region fills (0–1).
    pub fit: f32,
    /// After the region shrinks, the view zooms back in slowly: by half per
    /// this many seconds (video).
    pub hold: f32,
    /// Before the region grows, the view zooms out ahead of it: by double per
    /// this many seconds (video).
    #[reflect(default = "default_lead")]
    pub lead: f32,
    /// Pan smoothing, seconds (video), zero-phase.
    pub pan_damping: f32,
    /// Zoom smoothing, seconds (video), zero-phase; it never lets the region
    /// out of the view.
    pub zoom_damping: f32,
    /// The point may wander this fraction of the view from its centre before the view follows.
    pub dead_zone: f32,
    /// Location influence: 0 = the parent's framing, 1 = follow the sketch.
    pub follow: f32,
    /// Zoom influence: 0 = the parent's zoom, 1 = the sketch's.
    pub zoom: f32,
    /// Zoom limits relative to the parent view (1 = the parent's zoom). The
    /// region always fits, so it wins where the limits would crop it.
    pub min_zoom: f32,
    pub max_zoom: f32,
    /// Keep one zoom over the whole sketch: the widest crop it needs at any
    /// frame, so a region that jitters in size never makes the view zoom.
    /// (Still limited by the parent's crop, as always. Off: the zoom follows
    /// the region, smoothed by `lead`, `hold` and `zoom_damping`.)
    #[reflect(default = "yes")]
    pub lock_zoom: bool,
}

impl Default for FrameParams {
    fn default() -> Self {
        Self { fit: 0.6, hold: 1.0, lead: default_lead(), pan_damping: 0.1, zoom_damping: 0.5, dead_zone: 0.0, follow: 1.0, zoom: 1.0, min_zoom: 1.0, max_zoom: 32.0, lock_zoom: true }
    }
}

fn default_lead() -> f32 {
    0.25
}

fn yes() -> bool {
    true
}

/// What new views start with (a user setting; the app remembers it).
#[derive(Resource, Debug, Clone, Default)]
pub struct ViewDefaults {
    pub params: FrameParams,
}

/// `frame`: a sketch (input "box") and optionally its parent view (input
/// "parent") → the view's per-frame crop.
pub struct FrameKind;

impl OperatorKind for FrameKind {
    fn name(&self) -> &'static str {
        "frame"
    }

    fn channels(&self) -> usize {
        VIEW_CHANNELS
    }

    fn footprint(&self, _: EntityRef<'_>) -> Footprint {
        Footprint::Global
    }

    fn evaluate(&self, ctx: &EvalCtx<'_>, range: std::ops::Range<FrameIndex>, out: &mut Signal) -> anyhow::Result<()> {
        out.clear(range.clone());
        let p = ctx.params::<FrameParams>().cloned().unwrap_or_default();
        let fps = ctx.world.get_resource::<Transport>().map_or(60.0, |t| t.fps.as_f64());
        let size = ctx.world.get_resource::<SourceSize>().copied().unwrap_or_default();
        let Some(sketch) = ctx.input("box") else { return Ok(()) };
        let parent = ctx.input("parent");
        let Some((first, frames)) = frame_views(sketch, parent, &p, fps, &size) else { return Ok(()) };
        for (i, v) in frames.iter().enumerate() {
            let f = first + i as FrameIndex;
            if range.contains(&f) {
                out.set(f, &v.map(|x| x as f32));
            }
        }
        Ok(())
    }
}

/// The framing computation: for every frame from the sketch's first to its
/// last (gaps interpolated), `[cx, cy, crop_w, crop_h, canvas_w, canvas_h]`.
pub fn frame_views(sketch: &Signal, parent: Option<&Signal>, p: &FrameParams, fps: f64, size: &SourceSize) -> Option<(FrameIndex, Vec<[f64; 6]>)> {
    let runs = sketch.runs(FrameIndex::MIN / 4..FrameIndex::MAX / 4);
    let present: Vec<_> = runs.iter().filter(|(_, s)| *s != crate::signal::FrameState::Absent).map(|(r, _)| r.clone()).collect();
    let (first, last) = (present.first()?.start, present.last()?.end - 1);
    let n = (last - first + 1) as usize;
    let aspect = size.width / size.height;

    // Point and half extents per frame; gaps interpolated linearly.
    let mut known: Vec<Option<[f64; 4]>> = (first..=last)
        .map(|f| {
            sketch.get(f).map(|v| {
                let [x, y, l, t, r, b] = [v[0], v[1], v[2], v[3], v[4], v[5]].map(|c| c as f64);
                [x, y, (x - l).max(r - x), (y - t).max(b - y)]
            })
        })
        .collect();
    fill_gaps(&mut known);
    let pts: Vec<[f64; 4]> = known.into_iter().map(|v| v.expect("filled")).collect();

    // Zoom: the crop height the region needs, as an envelope that never dips below it
    // (locked: its widest, on every frame).
    let need: Vec<f64> = pts.iter().map(|v| (2.0 * v[3]).max(2.0 * v[2] / aspect).max(1.0) / p.fit.clamp(0.05, 1.0) as f64).collect();
    let mut crop_h: Vec<f64> = zoom_envelope(&need, p, fps);
    if p.lock_zoom {
        let widest = crop_h.iter().copied().fold(1.0, f64::max);
        crop_h.fill(widest);
    }

    // Centre: the point, through a dead zone (both ways, so no lag) and zero-phase damping.
    let radius: Vec<f64> = crop_h.iter().map(|h| p.dead_zone.max(0.0) as f64 * h / 2.0).collect();
    let lazy = |order: &mut dyn Iterator<Item = usize>| {
        let mut out = vec![[0.0; 2]; n];
        let mut anchor: Option<[f64; 2]> = None;
        for i in order {
            let q = [pts[i][0], pts[i][1]];
            let a = anchor.get_or_insert(q);
            let (dx, dy) = (q[0] - a[0], q[1] - a[1]);
            let d = dx.hypot(dy);
            if d > radius[i] {
                let s = (d - radius[i]) / d;
                *a = [a[0] + dx * s, a[1] + dy * s];
            }
            out[i] = *a;
        }
        out
    };
    let fwd = lazy(&mut (0..n));
    let bwd = lazy(&mut (0..n).rev());
    let sigma = p.pan_damping as f64 * fps;
    let cx = gauss_trend(&(0..n).map(|i| (fwd[i][0] + bwd[i][0]) / 2.0).collect::<Vec<_>>(), sigma);
    let cy = gauss_trend(&(0..n).map(|i| (fwd[i][1] + bwd[i][1]) / 2.0).collect::<Vec<_>>(), sigma);

    // Influence and zoom limits against the parent's framing (the whole source for a root view).
    let mut centre: Vec<[f64; 2]> = Vec::with_capacity(n);
    for i in 0..n {
        let f = first + i as FrameIndex;
        let (pc, ph) = match parent.and_then(|s| nearest(s, f)) {
            Some(v) => ([v[0] as f64, v[1] as f64], v[3] as f64),
            None => ([size.width / 2.0, size.height / 2.0], size.height),
        };
        let follow = p.follow.clamp(0.0, 1.0) as f64;
        centre.push([pc[0] + follow * (cx[i] - pc[0]), pc[1] + follow * (cy[i] - pc[1])]);
        let zoom = p.zoom.clamp(0.0, 1.0) as f64;
        let h = (ph.ln() + zoom * (crop_h[i].ln() - ph.ln())).exp();
        let (lo, hi) = (ph / p.max_zoom.max(1e-3) as f64, ph / p.min_zoom.max(1e-3) as f64);
        // The region always fits: fit wins over influence and the zoom limits.
        crop_h[i] = h.clamp(lo.min(hi), hi.max(lo)).max(need[i]);
    }

    // The canvas: as big as the widest framing.
    let canvas_h = crop_h.iter().copied().fold(1.0, f64::max);
    let out = (0..n).map(|i| [centre[i][0], centre[i][1], crop_h[i] * aspect, crop_h[i], canvas_h * aspect, canvas_h]).collect();
    Some((first, out))
}

/// The crop height over time, in log space (zoom is perceived as ratios):
/// 1. zoom out ahead of the region growing: rising at most ×2 per `lead`
///    seconds before a larger need (a backward decaying max);
/// 2. zoom back in slowly after it shrinks: falling at most ×½ per `hold`
///    seconds (a forward decaying max);
/// 3. smooth without ever dipping below the need: a max filter over
///    ±`zoom_damping`, then a Gaussian (σ = a third of that) whose reach stays
///    inside the filter's window, so every value it averages covers the need.
///
/// The region always fits, and a jittery region size makes no zoom jitter.
pub fn zoom_envelope(need: &[f64], p: &FrameParams, fps: f64) -> Vec<f64> {
    let n = need.len();
    let rate = |secs: f32| if secs > 0.0 { std::f64::consts::LN_2 / (secs as f64 * fps) } else { f64::INFINITY };
    let (up, down) = (rate(p.lead), rate(p.hold));
    let log: Vec<f64> = need.iter().map(|v| v.max(1e-6).ln()).collect();
    let mut fwd = log.clone();
    for i in 1..n {
        fwd[i] = fwd[i].max(fwd[i - 1] - down);
    }
    let mut env = log.clone();
    for i in (0..n.saturating_sub(1)).rev() {
        env[i] = env[i].max(env[i + 1] - up);
    }
    for (e, f) in env.iter_mut().zip(&fwd) {
        *e = e.max(*f);
    }
    let w = (p.zoom_damping.max(0.0) as f64 * fps).round() as usize;
    if w > 0 {
        let dilated: Vec<f64> = (0..n).map(|i| env[i.saturating_sub(w)..(i + w + 1).min(n)].iter().copied().fold(f64::NEG_INFINITY, f64::max)).collect();
        env = gauss(&dilated, w as f64 / 3.0);
    }
    // (Numerically the envelope already covers the need; the max only guards rounding.)
    env.iter().zip(need).map(|(e, v)| e.exp().max(*v)).collect()
}

/// Linear interpolation across `None` runs between known values (ends held).
fn fill_gaps(v: &mut [Option<[f64; 4]>]) {
    let known: Vec<usize> = (0..v.len()).filter(|i| v[*i].is_some()).collect();
    for w in known.windows(2) {
        let (a, b) = (w[0], w[1]);
        let (va, vb) = (v[a].unwrap(), v[b].unwrap());
        for (k, slot) in v[a + 1..b].iter_mut().enumerate() {
            let u = (k + 1) as f64 / (b - a) as f64;
            *slot = Some(std::array::from_fn(|c| va[c] + (vb[c] - va[c]) * u));
        }
    }
}

// ---- entering and leaving views --------------------------------------------------------------

fn apply_view_actions(world: &mut World) {
    let actions = world.resource_mut::<PendingActions>().take(|a| matches!(a, Action::EnterView | Action::ExitView));
    for a in actions {
        match a {
            Action::EnterView => {
                let Some(sketch) = world.resource::<Selection>().primary().and_then(|e| sketch_of(world, e)) else { continue };
                let view = ensure_view(world, sketch);
                world.resource_mut::<ActiveView>().0 = Some(view);
                // Inside a view, a hold starts a new sketch nested in it; editing the
                // sketch that defines the view takes selecting it first (a click on its box).
                world.resource_mut::<Selection>().clear();
            }
            Action::ExitView => {
                let current = world.resource::<ActiveView>().0;
                let up = current.and_then(|v| parent_of(world, v));
                // Leaving selects the sketch whose view we were in, so Tab goes straight back.
                if let Some(s) = current.and_then(|v| sketch_framed(world, v)) {
                    world.resource_mut::<Selection>().select_only(s);
                }
                world.resource_mut::<ActiveView>().0 = up;
            }
            _ => {}
        }
    }
}

/// A view that was deleted (or undone) hands the viewport to its nearest live ancestor.
fn prune_active_view(world: &mut World) {
    let Some(v) = world.resource::<ActiveView>().0 else { return };
    if is_live(world, v) && is_view(world, v) {
        return;
    }
    // Walk up through the (possibly disabled) chain to the first live view.
    let mut up = parent_of(world, v);
    while let Some(u) = up {
        if is_live(world, u) {
            break;
        }
        up = parent_of(world, u);
    }
    world.resource_mut::<ActiveView>().0 = up;
}

pub struct ViewModule;

impl Module for ViewModule {
    fn build(&self, app: &mut AppBuilder) {
        app.declare::<ActiveView>(Class::Session)
            .declare::<SourceSize>(Class::Session)
            .declare::<ViewDefaults>(Class::Session)
            .init_resource::<ActiveView>()
            .init_resource::<SourceSize>()
            .init_resource::<ViewDefaults>()
            .operator(FrameKind)
            .operator_params::<FrameParams>()
            .add_systems(apply_view_actions.in_set(Set::Intents))
            .add_systems(prune_active_view.in_set(Set::Prepare));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signal(values: &[(FrameIndex, [f32; 6])]) -> Signal {
        let mut s = Signal::new(6);
        for (f, v) in values {
            s.set(*f, v);
        }
        s
    }

    #[test]
    fn the_region_always_fits_even_under_a_tight_parent() {
        // A parent zoomed in to a 100 px tall crop; the child's region needs 400 px.
        let parent = signal(&(0..10).map(|f| (f, [500.0, 300.0, 177.8, 100.0, 1920.0, 1080.0])).collect::<Vec<_>>());
        let sketch = signal(&(0..10).map(|f| (f, [500.0, 300.0, 400.0, 180.0, 600.0, 420.0])).collect::<Vec<_>>());
        for lock_zoom in [false, true] {
            let p = FrameParams { lock_zoom, ..FrameParams::default() };
            let (_, frames) = frame_views(&sketch, Some(&parent), &p, 60.0, &SourceSize::default()).unwrap();
            for v in frames {
                let (crop_w, crop_h) = (v[2], v[3]);
                assert!(crop_h >= 240.0 && crop_w >= 200.0, "the 200×240 region fits: crop {crop_w:.0}×{crop_h:.0}");
            }
        }
    }

    /// A pseudo-random walk in [0, 1).
    fn noise(i: usize) -> f64 {
        let x = (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ 0x2545_f491_4f6c_dd1d;
        ((x >> 11) as f64) / ((1u64 << 53) as f64)
    }

    #[test]
    fn a_jittery_region_size_makes_a_smooth_zoom_that_always_fits() {
        // A region whose needed crop jitters ±25% from frame to frame, around 400 px.
        let need: Vec<f64> = (0..600).map(|i| 400.0 * (1.0 + 0.5 * (noise(i) - 0.5))).collect();
        let crop = zoom_envelope(&need, &FrameParams::default(), 60.0);
        let worst_need = need.windows(2).map(|w| (w[1] / w[0]).ln().abs()).fold(0.0, f64::max);
        let worst_crop = crop.windows(2).map(|w| (w[1] / w[0]).ln().abs()).fold(0.0, f64::max);
        println!("largest frame-to-frame zoom change: region {:.1}%, view {:.2}%", worst_need * 100.0, worst_crop * 100.0);
        assert!(crop.iter().zip(&need).all(|(c, n)| c >= n), "the region always fits");
        assert!(worst_crop < 0.01, "the zoom barely moves between frames: {:.2}%", worst_crop * 100.0);
    }

    #[test]
    fn the_view_zooms_out_ahead_and_back_in_slowly() {
        // The region doubles for one second (frames 300-360), then shrinks back.
        let need: Vec<f64> = (0..900).map(|i| if (300..360).contains(&i) { 800.0 } else { 400.0 }).collect();
        let p = FrameParams::default();
        let crop = zoom_envelope(&need, &p, 60.0);
        assert!(crop[295] > 600.0, "already zooming out before the growth: {:.0}", crop[295]);
        assert!((300..360).all(|i| crop[i] >= 800.0), "the region fits throughout");
        let jump = crop[250..420].windows(2).map(|w| (w[1] / w[0]).ln().abs()).fold(0.0, f64::max);
        assert!(jump < 0.03, "no snap at the edges of the growth: {:.2}% per frame", jump * 100.0);
        // Back in slowly: still well zoomed out half a second after, settled after ~3 hold times.
        assert!(crop[390] > 650.0, "half a second later: {:.0}", crop[390]);
        assert!(crop[600] < 450.0, "settled back: {:.0}", crop[600]);
        let steps: Vec<f64> = crop[360..600].windows(2).map(|w| w[0] / w[1]).collect();
        assert!(steps.iter().all(|s| *s >= 0.999), "zooming back in never overshoots");
    }

    #[test]
    fn space_maps_round_trip() {
        let m = SpaceMap::of_view(&[500.0, 300.0, 320.0, 180.0, 640.0, 360.0]);
        assert_eq!(m.to_source([320.0, 180.0]), [500.0, 300.0], "the canvas centre is the crop centre");
        assert_eq!(m.a, 0.5, "the crop is half the canvas: magnified 2×");
        let p = [123.0, 45.0];
        let q = m.from_source(m.to_source(p));
        assert!((q[0] - p[0]).abs() < 1e-9 && (q[1] - p[1]).abs() < 1e-9);
    }
}
