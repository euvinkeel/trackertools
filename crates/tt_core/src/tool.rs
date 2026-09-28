//! Tools and pointer input (DESIGN §8.1, §14).
//!
//! The host fills [`PointerFrame`] once per app frame, before PreUi: every
//! pointer sample since the previous frame (all of them, ~1 kHz, each with
//! its own timestamp), already mapped into source pixels, plus the primary
//! button's transitions. Tools read it in `Set::Tools`; nothing reads the
//! mouse directly, so a tool is testable headless by filling the resource.

use bevy_ecs::prelude::*;
use bevy_reflect::Reflect;

use crate::app::{AppBuilder, Module, Set};
use crate::input::{Action, PendingActions};
use crate::meta::Class;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Reflect)]
pub enum Tool {
    /// Pick and inspect; the viewport's primary button does nothing else yet.
    #[default]
    Select,
    /// Press and hold on the viewport to follow something (capture.rs).
    Sketch,
}

#[derive(Resource, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ActiveTool(pub Tool);

/// This app frame's pointer input, in the viewport's source pixels.
/// A press that ends quickly without moving is a *click* ([`CLICK_TIME`],
/// [`CLICK_MOVE`]): it selects the sketch under it in any tool.
#[derive(Resource, Debug, Default, Clone)]
pub struct PointerFrame {
    /// Samples since the previous app frame: `[wall seconds, x, y]`, oldest first.
    pub samples: Vec<[f64; 3]>,
    /// Where the pointer is now, if over a viewport.
    pub hover: Option<[f64; 2]>,
    /// A primary press landed on a viewport since the previous frame: its wall time.
    pub pressed: Option<f64>,
    /// The primary button is held.
    pub down: bool,
    /// The primary button was released since the previous frame: its wall time.
    pub released: Option<f64>,
    /// Mouse-wheel notches since the previous frame, over a viewport (+ = away from you).
    pub wheel: f32,
    /// Screen points per source pixel in the viewport (for distances on screen).
    pub scale: f64,
    /// Derived in `Set::Input`: a short press that barely moved ended this frame,
    /// at this position. Clicks select; they never record.
    pub click: Option<[f64; 2]>,
    /// Set by a tool that used the wheel this frame (the viewport then doesn't zoom).
    pub wheel_taken: bool,
}

/// A press shorter than this (seconds) that moved less than [`CLICK_MOVE`] is a click.
pub const CLICK_TIME: f64 = 0.18;
/// Screen points.
pub const CLICK_MOVE: f64 = 4.0;

/// The press being watched for a click: (time, position, farthest distance so far in source px).
#[derive(Resource, Debug, Default)]
struct ClickTracker(Option<(f64, [f64; 2], f64)>);

fn detect_clicks(mut pointer: ResMut<PointerFrame>, mut tracker: ResMut<ClickTracker>, clock: Res<crate::time::WallClock>) {
    pointer.click = None;
    if let Some(t) = pointer.pressed {
        let at = pointer.samples.iter().find(|s| s[0] >= t).map(|s| [s[1], s[2]]).or(pointer.hover);
        tracker.0 = at.map(|p| (t, p, 0.0));
    }
    let Some((t0, p0, mut moved)) = tracker.0 else { return };
    for s in pointer.samples.iter().filter(|s| s[0] >= t0) {
        moved = moved.max((s[1] - p0[0]).hypot(s[2] - p0[1]));
    }
    tracker.0 = Some((t0, p0, moved));
    if let Some(t1) = pointer.released.or((!pointer.down).then_some(clock.now)) {
        let scale = if pointer.scale > 0.0 { pointer.scale } else { 1.0 }; // 0 = not reported
        if t1 - t0 < CLICK_TIME && moved * scale < CLICK_MOVE {
            pointer.click = Some(p0);
        }
        tracker.0 = None;
    }
}


fn apply_tool_actions(mut actions: ResMut<PendingActions>, mut active: ResMut<ActiveTool>, live: Res<crate::capture::LiveCapture>) {
    for a in actions.take(|a| matches!(a, Action::Tool(_))) {
        if let Action::Tool(t) = a {
            active.0 = if active.0 == t { Tool::Select } else { t };
        }
    }
    // Esc with no gesture in progress leaves the tool (the capture takes its own Esc first).
    if live.0.is_none() && !actions.take(|a| a == Action::Cancel).is_empty() {
        active.0 = Tool::Select;
    }
}

/// A click (in any tool) selects the sketch whose region is under it, or clears the selection.
fn select_on_click(world: &mut World) {
    let Some(pos) = world.resource::<PointerFrame>().click else { return };
    let frame = world.resource::<crate::transport::Transport>().frame();
    let picked = crate::sketch::pick_sketch(world, frame, pos);
    let mut sel = world.resource_mut::<crate::selection::Selection>();
    match picked {
        Some(e) => sel.select_only(e),
        None => sel.clear(),
    }
}

pub struct ToolModule;

impl Module for ToolModule {
    fn build(&self, app: &mut AppBuilder) {
        app.declare::<ActiveTool>(Class::Session)
            .declare::<PointerFrame>(Class::Derived)
            .declare::<ClickTracker>(Class::Derived)
            .init_resource::<ActiveTool>()
            .init_resource::<PointerFrame>()
            .init_resource::<ClickTracker>()
            .add_systems(detect_clicks.in_set(Set::Input))
            .add_systems((apply_tool_actions, crate::capture::sketch_tool, select_on_click).chain().in_set(Set::Tools));
    }
}
