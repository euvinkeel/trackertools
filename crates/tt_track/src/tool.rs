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
//!   with that mask): a new paint tracker, or with one selected, added to
//!   its paint on this frame (anywhere on the picture: patches with gaps
//!   between them are one paint), or a new paint on this frame
//!   ([`crate::job::paint`]). Alt+brush erases from its paint here.
//!
//! - **Cursor** trackers paint the same way ([`crate::job::cursor`]), each
//!   paint teaching one of its *patterns* (an arrow, a hand …): a brush
//!   teaches the pattern picked (keys 1–0, a click on it, or the selected
//!   paint's; else the last painted), Shift+brush starts a new one.
//!   Selecting one of its paints and brushing adds to that tracker.
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
    /// Per cursor tracker: the pattern a plain brush teaches (picked).
    pub patterns: std::collections::HashMap<Entity, u32>,
    /// The press in progress starts a new pattern (Shift, on a cursor tracker).
    pub new_pattern: bool,
    /// The selection last seen: selecting a cursor tracker's paint picks its pattern.
    seen: Option<Entity>,
}

impl Default for TrackTool {
    fn default() -> Self {
        Self { brush: 14.0, drag: None, refused: None, reseed: None, stroke: Vec::new(), patterns: Default::default(), new_pattern: false, seen: None }
    }
}

/// The paint tracker a brush adds to: the selected one, or the one whose paint is selected.
pub fn paint_tracker(world: &World) -> Option<Entity> {
    let e = world.resource::<Selection>().primary()?;
    let t = if is_tracker(world, e) {
        e
    } else if world.get::<Look>(e).is_some() {
        crate::look::owner_of(world, e)?
    } else {
        return None;
    };
    world.get::<Tracker>(t).is_some_and(|p| p.method.paints()).then_some(t)
}

/// The cursor tracker a brush teaches (see [`paint_tracker`]).
pub fn cursor_tracker(world: &World) -> Option<Entity> {
    paint_tracker(world).filter(|t| world.get::<Tracker>(*t).is_some_and(|p| p.method == Method::Cursor))
}

/// The pattern a plain brush teaches cursor tracker `t`: the one picked,
/// else the last painted's (0 with none).
pub fn active_pattern(world: &World, tool: &TrackTool, t: Entity) -> u32 {
    if let Some(p) = tool.patterns.get(&t) {
        return *p;
    }
    crate::look::looks_of(world, t).last().and_then(|l| world.get::<Look>(*l)).map_or(0, |l| l.pattern)
}

/// A pattern no paint of `t` teaches yet: the one after the last.
pub fn next_pattern(world: &World, t: Entity) -> u32 {
    crate::look::patterns_of(world, t).last().map_or(0, |p| p + 1)
}

/// What a brush stroke makes of a paint: the stroke `path` with radius `r`
/// added to `old` (a paint's `(centre, half-size, mask)`, all in one space's
/// pixels), or with `erase` taken out of it. A paint over a large area gets
/// finer cells (a mask `n × n`, 32 to 128 a side), so patches far apart stay
/// as painted. None: nothing is left painted.
pub fn brush_paint(old: Option<([f64; 2], [f64; 2], &[u8])>, path: &[[f64; 2]], r: f64, erase: bool) -> Option<([f64; 2], [f64; 2], Vec<u8>)> {
    let r = r.max(0.5);
    let (mut lo, mut hi) = ([f64::INFINITY; 2], [f64::NEG_INFINITY; 2]);
    let mut grow = |p: [f64; 2], h: [f64; 2]| {
        for k in 0..2 {
            lo[k] = lo[k].min(p[k] - h[k]);
            hi[k] = hi[k].max(p[k] + h[k]);
        }
    };
    if let Some((c, h, _)) = old {
        grow(c, h);
    }
    if !erase || old.is_none() {
        for p in path {
            grow(*p, [r, r]);
        }
    }
    let centre = [(lo[0] + hi[0]) / 2.0, (lo[1] + hi[1]) / 2.0];
    let half = [(hi[0] - lo[0]) / 2.0, (hi[1] - lo[1]) / 2.0];
    // Cells about a third of the brush's radius, 32 to 128 a side.
    let n = ((2.0 * half[0].max(half[1]) / (r / 3.0).max(1.0)).ceil() as usize).clamp(crate::look::MASK_N, 128);
    // Distance from a point to the stroke (a dot: its first point).
    let near = |q: [f64; 2]| -> f64 {
        let Some(first) = path.first() else { return f64::INFINITY };
        let mut best = (q[0] - first[0]).hypot(q[1] - first[1]);
        for w in path.windows(2) {
            let (a, b) = (w[0], w[1]);
            let d = [b[0] - a[0], b[1] - a[1]];
            let len2 = d[0] * d[0] + d[1] * d[1];
            let t = if len2 > 0.0 { (((q[0] - a[0]) * d[0] + (q[1] - a[1]) * d[1]) / len2).clamp(0.0, 1.0) } else { 0.0 };
            best = best.min((q[0] - a[0] - t * d[0]).hypot(q[1] - a[1] - t * d[1]));
        }
        best
    };
    // Whether the old paint covers a point (its nearest cell).
    let was = |q: [f64; 2]| -> bool {
        let Some((c, h, mask)) = old else { return false };
        let Some(m) = crate::job::paint::mask_side(mask) else { return false };
        let (u, v) = ((q[0] - (c[0] - h[0])) / (2.0 * h[0]) * m as f64, (q[1] - (c[1] - h[1])) / (2.0 * h[1]) * m as f64);
        let (x, y) = (u.floor() as i64, v.floor() as i64);
        (0..m as i64).contains(&x) && (0..m as i64).contains(&y) && mask[y as usize * m + x as usize] >= 96
    };
    let mut mask = vec![0u8; n * n];
    let mut any = false;
    for y in 0..n {
        for x in 0..n {
            let q = [lo[0] + (x as f64 + 0.5) / n as f64 * 2.0 * half[0], lo[1] + (y as f64 + 0.5) / n as f64 * 2.0 * half[1]];
            let on = near(q) <= r;
            if if erase { was(q) && !on } else { was(q) || on } {
                mask[y * n + x] = 255;
                any = true;
            }
        }
    }
    any.then_some((centre, half, mask))
}

/// The paint a brush stroke makes on its own (`brush_paint` with nothing before it).
pub fn paint_look(path: &[[f64; 2]], r: f64) -> ([f64; 2], [f64; 2], Vec<u8>) {
    brush_paint(None, path, r, false).expect("a stroke paints")
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
    // 1–0: a cursor tracker's pattern (only the Track tool's).
    let picks = world.resource_mut::<tt_core::input::PendingActions>().take(|a| matches!(a, tt_core::input::Action::Pattern(_)));
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
    let cursor = cursor_tracker(world);
    if let (Some(t), Some(tt_core::input::Action::Pattern(n))) = (cursor, picks.last()) {
        tool.patterns.insert(t, *n as u32);
    }
    // Selecting a cursor tracker's paint picks its pattern.
    let primary = world.resource::<Selection>().primary();
    if primary != tool.seen {
        tool.seen = primary;
        if let (Some(t), Some(look)) = (cursor, primary.and_then(|e| world.get::<Look>(e))) {
            tool.patterns.insert(t, look.pattern);
        }
    }
    // Ctrl+wheel sizes the click's pattern; the plain wheel stays the viewport's zoom.
    if p.wheel != 0.0 && tool.drag.is_none() && world.resource::<KeysHeld>().mods.ctrl {
        tool.brush = (tool.brush * 1.15f32.powf(p.wheel)).clamp(3.0, 400.0);
        world.resource_mut::<PointerFrame>().wheel_taken = true;
    }
    if let Some(t) = p.pressed
        && let Some(at) = p.samples.iter().find(|s| s[0] >= t).map(|s| [s[1], s[2]]).or(p.hover)
    {
        // (On a cursor tracker, Shift starts a new pattern, not a new tracker.)
        tool.new_pattern = shift && cursor.is_some();
        tool.drag = Some((at, frame, shift && !tool.new_pattern));
        tool.refused = None;
        tool.stroke = vec![at];
    }
    // A paint tracker's press brushes: the path, kept until it is let go.
    let painting = tool.drag.is_some_and(|(_, _, shift)| method_for(world, shift).paints());
    if painting {
        let since = p.pressed.unwrap_or(f64::NEG_INFINITY);
        tool.stroke.extend(p.samples.iter().filter(|s| s[0] >= since).map(|s| [s[1], s[2]]));
    }
    let ended = p.released.is_some() || !p.down;
    if ended && painting && let Some((start, f, shift)) = tool.drag.take() {
        let path = if tool.stroke.is_empty() { vec![start] } else { std::mem::take(&mut tool.stroke) };
        // In source pixels: a paint lives there (the brush's radius is the shown space's).
        let map = map_at(world, world.resource::<ActiveView>().0, f);
        let path: Vec<[f64; 2]> = path.iter().map(|p| map.to_source(*p)).collect();
        let r = tool.brush as f64 / p.scale.max(1e-9) * map.a;
        let erase = world.resource::<KeysHeld>().mods.alt;
        // A cursor tracker's pattern: the one picked, or a new one.
        let pattern = cursor.filter(|_| !shift).map(|t| (t, if tool.new_pattern { next_pattern(world, t) } else { active_pattern(world, &tool, t) }));
        tool.refused = brush(world, f, &path, r, shift, erase, tool.reseed.take(), pattern.map(|(_, p)| p)).err();
        if let (Some((t, p)), None) = (pattern, &tool.refused) {
            tool.patterns.insert(t, p);
        }
    }
    if ended && let Some((start, f, shift)) = tool.drag.take() {
        tool.stroke.clear();
        let end = p.samples.last().map(|s| [s[1], s[2]]).or(p.hover).unwrap_or(start);
        // CoTracker follows a pixel: the point where the press is let go.
        let point = method_for(world, shift).point();
        let (c, h) = if point { (end, [tool.brush as f64 / p.scale.max(1e-9); 2]) } else { tool.pattern(start, end, p.scale) };
        // The pointer is in the shown space's pixels; looks live in source pixels.
        let map = map_at(world, world.resource::<ActiveView>().0, f);
        let look = Look::new(f, map.to_source(c), [h[0] * map.a, h[1] * map.a]);
        tool.refused = place(world, look, shift, tool.reseed.take()).err();
    }
    *world.resource_mut::<TrackTool>() = tool;
}

/// A brush stroke (source px, radius `r`) on frame `f`: added to the
/// selected paint tracker's paint on this frame (a cursor tracker's of
/// `pattern`), or, `erase`, taken out of it: a paint left empty goes; else
/// a new paint (of `pattern`; with no tracker, `place`). Err = why not.
#[allow(clippy::too_many_arguments)]
fn brush(world: &mut World, f: FrameIndex, path: &[[f64; 2]], r: f64, new: bool, erase: bool, reseed: Option<Entity>, pattern: Option<u32>) -> Result<(), String> {
    let tracker = paint_tracker(world).filter(|_| !new);
    let here = tracker.and_then(|t| {
        crate::look::looks_of(world, t).into_iter().find(|l| world.get::<Look>(*l).is_some_and(|l| l.frame == f && pattern.is_none_or(|p| l.pattern == p)))
    });
    match (here, erase) {
        (Some(l), _) => {
            let old = world.get::<Look>(l).cloned().expect("a look");
            let made = brush_paint(Some((old.center(), old.half(), &old.mask)), path, r, erase);
            match made {
                Some((c, h, mask)) => {
                    tt_core::history::edit(world, if erase { "Erase paint" } else { "Paint" }, |tx| {
                        tx.modify::<Look>(l, |look| {
                            (look.x, look.y, look.half_w, look.half_h) = (c[0] as f32, c[1] as f32, h[0] as f32, h[1] as f32);
                            look.mask = mask;
                        });
                    });
                }
                None => {
                    tt_core::commands::delete(world, &[l]);
                }
            }
            Ok(())
        }
        (None, true) => Err(match pattern {
            Some(p) => format!("Nothing to erase: pattern {} has no paint on this frame", p + 1),
            None => "Nothing to erase: this tracker has no paint on this frame".into(),
        }),
        (None, false) => {
            let (c, h, mask) = paint_look(path, r);
            let mut look = Look::new(f, c, h);
            look.mask = mask;
            look.pattern = pattern.unwrap_or(0);
            match tracker.filter(|t| reseed != Some(*t)) {
                Some(t) => {
                    add_look(world, t, look);
                    Ok(())
                }
                None => place(world, look, new, reseed),
            }
        }
    }
}

/// What a press makes: the selected tracker's kind (unless `new`), else the kind chosen for new ones.
pub fn method_for(world: &World, new: bool) -> Method {
    // (A paint tracker's paint selected: that tracker's.)
    let selected = world.resource::<Selection>().primary().filter(|e| is_tracker(world, *e)).or_else(|| paint_tracker(world)).filter(|_| !new);
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
            Method::CoTracker | Method::TapNext if reseed == Some(t) => {
                reseed_with_look(world, t, look);
            }
            Method::CoTracker | Method::TapNext => {
                set_reset_point(world, t, look);
            }
            Method::Template if reseed == Some(t) => {
                reseed_with_look(world, t, auto_masked(world, look));
            }
            Method::Template => {
                add_look(world, t, auto_masked(world, look));
            }
            // Another paint (on a frame with one already: both count).
            Method::Paint | Method::Cursor => {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::LookSpec;
    use crate::job::paint::on;

    fn spec(made: &([f64; 2], [f64; 2], Vec<u8>)) -> LookSpec {
        LookSpec { frame: 0, center: made.0, half: made.1, mask: Some(made.2.clone()), pattern: 0 }
    }

    /// Strokes far apart are one paint (the gap stays unpainted, with fine
    /// enough cells), and an erasing stroke takes its part out of it.
    #[test]
    fn a_paint_grows_over_gaps_and_erases() {
        let a = brush_paint(None, &[[100.0, 100.0], [120.0, 100.0]], 8.0, false).expect("painted");
        let b = brush_paint(Some((a.0, a.1, &a.2)), &[[900.0, 500.0]], 8.0, false).expect("painted");
        let s = spec(&b);
        assert!(on(&s, [110.0, 100.0]) && on(&s, [900.0, 500.0]), "both patches");
        assert!(!on(&s, [500.0, 300.0]) && !on(&s, [160.0, 100.0]), "not the gap between them");
        assert!(crate::job::paint::mask_side(&b.2).is_some_and(|n| n > crate::look::MASK_N), "finer cells over a large area");
        let c = brush_paint(Some((b.0, b.1, &b.2)), &[[900.0, 500.0]], 12.0, true).expect("some left");
        let s = spec(&c);
        assert!(on(&s, [110.0, 100.0]) && !on(&s, [900.0, 500.0]), "the second patch erased");
        assert!(brush_paint(Some((c.0, c.1, &c.2)), &[[90.0, 100.0], [130.0, 100.0]], 15.0, true).is_none(), "all erased: none left");
    }
}
