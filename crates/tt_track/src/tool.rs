//! The Track tool (`T`, DESIGN §6.3): show a tracker what to follow.
//!
//! - **Drag** a rectangle on the video: a new tracker whose first look is that
//!   pattern, on the shown frame.
//! - **Click**: a point tracker, its pattern the brush's size around the
//!   click (the wheel sizes the brush; the overlay shows it dashed).
//! - **Shift** + drag or click with a tracker selected: another look for it,
//!   on this frame (a cursor that changed icon).
//!
//! The guide (search region and motion prior) is the selected sketch, else
//! the smallest sketch whose box holds the pattern on this frame.

use bevy_ecs::prelude::*;
use tt_core::input::KeysHeld;
use tt_core::selection::Selection;
use tt_core::sketch::{pick_sketch, sketch_of};
use tt_core::time::FrameIndex;
use tt_core::tool::{ActiveTool, CLICK_MOVE, PointerFrame, Tool};
use tt_core::transport::Transport;
use tt_core::view::{ActiveView, map_at};

use crate::look::Look;
use crate::{add_look, add_tracker_with_look, is_tracker};

#[derive(Resource, Debug, Clone)]
pub struct TrackTool {
    /// A click's pattern: half-size in screen points.
    pub brush: f32,
    /// A press in progress: where (shown space's pixels), on which frame, with Shift.
    pub drag: Option<([f64; 2], FrameIndex, bool)>,
    /// Why the last press made nothing (for the HUD), cleared by the next.
    pub refused: Option<String>,
}

impl Default for TrackTool {
    fn default() -> Self {
        Self { brush: 14.0, drag: None, refused: None }
    }
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
        world.resource_mut::<TrackTool>().drag = None;
        return;
    }
    let p = world.resource::<PointerFrame>().clone();
    let frame = world.resource::<Transport>().frame();
    let shift = world.resource::<KeysHeld>().mods.shift;
    let mut tool = world.resource::<TrackTool>().clone();
    if p.wheel != 0.0 && tool.drag.is_none() {
        tool.brush = (tool.brush * 1.15f32.powf(p.wheel)).clamp(3.0, 400.0);
        world.resource_mut::<PointerFrame>().wheel_taken = true;
    }
    if let Some(t) = p.pressed
        && let Some(at) = p.samples.iter().find(|s| s[0] >= t).map(|s| [s[1], s[2]]).or(p.hover)
    {
        tool.drag = Some((at, frame, shift));
        tool.refused = None;
    }
    let ended = p.released.is_some() || !p.down;
    if ended && let Some((start, f, shift)) = tool.drag.take() {
        let end = p.samples.last().map(|s| [s[1], s[2]]).or(p.hover).unwrap_or(start);
        let (c, h) = tool.pattern(start, end, p.scale);
        // The pointer is in the shown space's pixels; looks live in source pixels.
        let map = map_at(world, world.resource::<ActiveView>().0, f);
        let look = Look::new(f, map.to_source(c), [h[0] * map.a, h[1] * map.a]);
        tool.refused = place(world, look, shift).err();
    }
    *world.resource_mut::<TrackTool>() = tool;
}

/// Make a tracker with `look` (or add it to the selected tracker). Err = why not.
fn place(world: &mut World, look: Look, add: bool) -> Result<(), String> {
    let primary = world.resource::<Selection>().primary();
    if add {
        let tracker = primary.filter(|e| is_tracker(world, *e)).ok_or("Shift adds a look to the selected tracker: select one first")?;
        add_look(world, tracker, look);
        return Ok(());
    }
    let guide = match primary.and_then(|e| sketch_of(world, e)) {
        Some(s) => Some(s),
        None => pick_sketch(world, look.frame, look.center()),
    };
    let guide = guide.ok_or("Draw a sketch over the subject first (D): the tracker searches inside it")?;
    add_tracker_with_look(world, guide, look).map(|_| ()).ok_or_else(|| "That sketch has no frames to track in".to_string())
}
