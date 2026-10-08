//! The Track tool (`T`, DESIGN §6.3): show a tracker what to follow.
//!
//! - **Drag** a rectangle on the video: a new tracker whose first look is that
//!   pattern, on the shown frame.
//! - **Click**: a point tracker, its pattern the brush's size around the
//!   click (the overlay shows it dashed; Ctrl+wheel sizes it, the wheel
//!   zooms as always).
//! - With a **tracker selected**, a drag or click *patches* it instead:
//!   another look, on this frame, where the subject really is (the tracker
//!   is pinned there and goes on from it; a cursor that changed icon is
//!   learned too). `Shift` makes a new tracker instead.
//! - After *Re-seed here* on a frame without a look, the next drag is the
//!   look the tracker starts again from.
//! - **CoTracker** follows one pixel, not a pattern: a press makes a point
//!   where it is let go (a new CoTracker, or with one selected its *reset
//!   point* on this frame: the pixel it follows from here on, moved if it
//!   already has one here).
//!
//! - **Paint** trackers: hold and brush over the subject (the brush is the
//!   click's size: Ctrl+wheel). Let go: the area brushed is a paint (a look
//!   with that mask): a new paint tracker, or with one selected another
//!   paint on this frame, its reset paint ([`crate::job::paint`]).
//!
//! New template looks get their mask painted automatically ([`crate::look::LookDefaults`]).
//!
//! The guide (search region and motion prior) is the selected sketch (or
//! the selected tracker's guide), else the smallest sketch whose box holds
//! the pattern on this frame; with none there, the tracker has no guide and
//! searches the whole frame.

use bevy_ecs::prelude::*;
use tt_core::input::KeysHeld;
use tt_core::selection::Selection;
use tt_core::sketch::{pick_sketch, sketch_of};
use tt_core::time::FrameIndex;
use tt_core::tool::{ActiveTool, CLICK_MOVE, PointerFrame, Tool};
use tt_core::transport::Transport;
use tt_core::view::{ActiveView, map_at};

use crate::look::{Look, auto_masked};
use crate::{Method, NewTrackers, Tracker, add_look, add_tracker_with_look, add_unguided_tracker, guide_of, is_tracker, reseed_with_look, set_reset_point};

#[derive(Resource, Debug, Clone)]
pub struct TrackTool {
    /// A click's pattern: half-size in screen points (a drag makes its own).
    pub brush: f32,
    /// A press in progress: where (shown space's pixels), on which frame, with Shift.
    pub drag: Option<([f64; 2], FrameIndex, bool)>,
    /// Why the last press made nothing (for the HUD), cleared by the next.
    pub refused: Option<String>,
    /// *Re-seed here* found no look on its frame: the next drag makes the
    /// look this tracker starts again from.
    pub reseed: Option<Entity>,
    /// A paint stroke in progress: the pointer's path (shown space's pixels).
    pub stroke: Vec<[f64; 2]>,
}

impl Default for TrackTool {
    fn default() -> Self {
        Self { brush: 14.0, drag: None, refused: None, reseed: None, stroke: Vec::new() }
    }
}

/// The paint a brush stroke makes: the stroke `path` with radius `r` (both in
/// one space's pixels), as `(centre, half-size, mask)` of a look there.
pub fn paint_look(path: &[[f64; 2]], r: f64) -> ([f64; 2], [f64; 2], Vec<u8>) {
    use crate::look::MASK_N;
    let r = r.max(0.5);
    let (mut lo, mut hi) = ([f64::INFINITY; 2], [f64::NEG_INFINITY; 2]);
    for p in path {
        for k in 0..2 {
            lo[k] = lo[k].min(p[k] - r);
            hi[k] = hi[k].max(p[k] + r);
        }
    }
    let centre = [(lo[0] + hi[0]) / 2.0, (lo[1] + hi[1]) / 2.0];
    let half = [(hi[0] - lo[0]) / 2.0, (hi[1] - lo[1]) / 2.0];
    // Distance from a point to the stroke (a dot: its first point).
    let near = |q: [f64; 2]| -> f64 {
        let mut best = (q[0] - path[0][0]).hypot(q[1] - path[0][1]);
        for w in path.windows(2) {
            let (a, b) = (w[0], w[1]);
            let d = [b[0] - a[0], b[1] - a[1]];
            let len2 = d[0] * d[0] + d[1] * d[1];
            let t = if len2 > 0.0 { (((q[0] - a[0]) * d[0] + (q[1] - a[1]) * d[1]) / len2).clamp(0.0, 1.0) } else { 0.0 };
            best = best.min((q[0] - a[0] - t * d[0]).hypot(q[1] - a[1] - t * d[1]));
        }
        best
    };
    let mut mask = vec![0u8; MASK_N * MASK_N];
    for y in 0..MASK_N {
        for x in 0..MASK_N {
            let q = [lo[0] + (x as f64 + 0.5) / MASK_N as f64 * 2.0 * half[0], lo[1] + (y as f64 + 0.5) / MASK_N as f64 * 2.0 * half[1]];
            if near(q) <= r {
                mask[y * MASK_N + x] = 255;
            }
        }
    }
    (centre, half, mask)
}

impl TrackTool {
    /// The rectangle a press from `start` to `end` makes (shown space's
    /// pixels): `(centre, half-size)`. A click makes the brush's.
    pub fn pattern(&self, start: [f64; 2], end: [f64; 2], scale: f64) -> ([f64; 2], [f64; 2]) {
        let scale = if scale > 0.0 { scale } else { 1.0 };
        if (end[0] - start[0]).hypot(end[1] - start[1]) * scale < CLICK_MOVE {
            let h = self.brush as f64 / scale;
            (start, [h, h])
        } else {
            ([(start[0] + end[0]) / 2.0, (start[1] + end[1]) / 2.0], [(end[0] - start[0]).abs() / 2.0, (end[1] - start[1]).abs() / 2.0])
        }
    }
}

/// `Set::Tools`: the Track tool's presses.
pub fn track_tool(world: &mut World) {
    if world.resource::<ActiveTool>().0 != Tool::Track {
        let mut tool = world.resource_mut::<TrackTool>();
        (tool.drag, tool.reseed) = (None, None);
        return;
    }
    // A re-seed waits for its tracker to stay selected.
    if let Some(t) = world.resource::<TrackTool>().reseed
        && world.resource::<Selection>().primary() != Some(t)
    {
        world.resource_mut::<TrackTool>().reseed = None;
    }
    let p = world.resource::<PointerFrame>().clone();
    let frame = world.resource::<Transport>().frame();
    let shift = world.resource::<KeysHeld>().mods.shift;
    let mut tool = world.resource::<TrackTool>().clone();
    // Ctrl+wheel sizes the click's pattern; the plain wheel stays the viewport's zoom.
    if p.wheel != 0.0 && tool.drag.is_none() && world.resource::<KeysHeld>().mods.ctrl {
        tool.brush = (tool.brush * 1.15f32.powf(p.wheel)).clamp(3.0, 400.0);
        world.resource_mut::<PointerFrame>().wheel_taken = true;
    }
    if let Some(t) = p.pressed
        && let Some(at) = p.samples.iter().find(|s| s[0] >= t).map(|s| [s[1], s[2]]).or(p.hover)
    {
        tool.drag = Some((at, frame, shift));
        tool.refused = None;
        tool.stroke = vec![at];
    }
    // A paint tracker's press brushes: the path, kept until it is let go.
    let painting = tool.drag.is_some_and(|(_, _, shift)| method_for(world, shift) == Method::Paint);
    if painting {
        let since = p.pressed.unwrap_or(f64::NEG_INFINITY);
        tool.stroke.extend(p.samples.iter().filter(|s| s[0] >= since).map(|s| [s[1], s[2]]));
    }
    let ended = p.released.is_some() || !p.down;
    if ended && painting && let Some((start, f, shift)) = tool.drag.take() {
        let path = if tool.stroke.is_empty() { vec![start] } else { std::mem::take(&mut tool.stroke) };
        let (c, h, mask) = paint_look(&path, tool.brush as f64 / p.scale.max(1e-9));
        let map = map_at(world, world.resource::<ActiveView>().0, f);
        let mut look = Look::new(f, map.to_source(c), [h[0] * map.a, h[1] * map.a]);
        look.mask = mask;
        tool.refused = place(world, look, shift, tool.reseed.take()).err();
    }
    if ended && let Some((start, f, shift)) = tool.drag.take() {
        tool.stroke.clear();
        let end = p.samples.last().map(|s| [s[1], s[2]]).or(p.hover).unwrap_or(start);
        // CoTracker follows a pixel: the point where the press is let go.
        let point = method_for(world, shift) == Method::CoTracker;
        let (c, h) = if point { (end, [tool.brush as f64 / p.scale.max(1e-9); 2]) } else { tool.pattern(start, end, p.scale) };
        // The pointer is in the shown space's pixels; looks live in source pixels.
        let map = map_at(world, world.resource::<ActiveView>().0, f);
        let look = Look::new(f, map.to_source(c), [h[0] * map.a, h[1] * map.a]);
        tool.refused = place(world, look, shift, tool.reseed.take()).err();
    }
    *world.resource_mut::<TrackTool>() = tool;
}

/// What a press makes: the selected tracker's kind (unless `new`), else the kind chosen for new ones.
pub fn method_for(world: &World, new: bool) -> Method {
    let selected = world.resource::<Selection>().primary().filter(|e| is_tracker(world, *e)).filter(|_| !new);
    match selected.and_then(|t| world.get::<Tracker>(t)) {
        Some(t) => t.method,
        None => world.get_resource::<NewTrackers>().map_or(Method::Template, |n| n.method),
    }
}

/// Patch the selected tracker with `look` (a CoTracker: its reset point on
/// that frame), re-seed it (`reseed`), or make a new tracker with it
/// (`new`, or nothing selected). Err = why not.
fn place(world: &mut World, look: Look, new: bool, reseed: Option<Entity>) -> Result<(), String> {
    let primary = world.resource::<Selection>().primary();
    let tracker = primary.filter(|e| is_tracker(world, *e));
    if let (Some(t), false) = (tracker, new) {
        match world.get::<Tracker>(t).map_or(Method::Template, |p| p.method) {
            Method::Manual => return Err("A manual dot has no automatic tracking: draw it with the Draw tool (M), or Shift+drag for a new tracker".into()),
            Method::CoTracker if reseed == Some(t) => {
                reseed_with_look(world, t, look);
            }
            Method::CoTracker => {
                set_reset_point(world, t, look);
            }
            Method::Template if reseed == Some(t) => {
                reseed_with_look(world, t, auto_masked(world, look));
            }
            Method::Template => {
                add_look(world, t, auto_masked(world, look));
            }
            // Another paint (on a frame with one already: both count).
            Method::Paint => {
                add_look(world, t, look);
            }
        }
        return Ok(());
    }
    let look = if method_for(world, true) == Method::Template { auto_masked(world, look) } else { look };
    let guide = match primary.and_then(|e| sketch_of(world, e).or_else(|| guide_of(world, e))) {
        Some(s) => Some(s),
        None => pick_sketch(world, look.frame, look.center()),
    };
    match guide {
        Some(g) => add_tracker_with_look(world, g, look).map(|_| ()).ok_or_else(|| "That sketch has no frames to track in".to_string()),
        // No sketch here: the tracker searches the whole frame.
        None => add_unguided_tracker(world, look).map(|_| ()).ok_or_else(|| "There is no video to track in".to_string()),
    }
}
