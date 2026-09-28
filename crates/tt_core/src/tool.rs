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

pub struct ToolModule;

impl Module for ToolModule {
    fn build(&self, app: &mut AppBuilder) {
        app.declare::<ActiveTool>(Class::Session)
            .declare::<PointerFrame>(Class::Derived)
            .init_resource::<ActiveTool>()
            .init_resource::<PointerFrame>()
            .add_systems(apply_tool_actions.in_set(Set::Tools).before(crate::capture::sketch_tool));
    }
}
