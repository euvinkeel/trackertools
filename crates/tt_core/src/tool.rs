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
    /// Show a tracker what to follow: drag a pattern, click a point (tt_track).
    Track,
    /// Draw a tracker's point by hand, frame by frame: over a tracker's
    /// automatic results, or a manual dot of its own (tt_track::human).
    Draw,
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

/// The pointer's last second over the viewport (`[t, x, y]`, the shown
/// space's pixels, oldest first): what a tool's preview reads (the Sketch
/// tool sizes its dashed box by the hand's jiggle before a press).
#[derive(Resource, Debug, Default, Clone)]
pub struct PointerTrail(pub std::collections::VecDeque<[f64; 3]>);

/// How long [`PointerTrail`] keeps samples (seconds).
const TRAIL: f64 = 1.0;

fn keep_trail(pointer: Res<PointerFrame>, mut trail: ResMut<PointerTrail>, clock: Res<crate::time::WallClock>) {
    if pointer.hover.is_none() {
        trail.0.clear();
        return;
    }
    trail.0.extend(pointer.samples.iter().copied());
    while trail.0.front().is_some_and(|s| s[0] < clock.now - TRAIL) && trail.0.len() > 1 {
        trail.0.pop_front();
    }
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


fn apply_tool_actions(world: &mut World) {
    for a in world.resource_mut::<PendingActions>().take(|a| matches!(a, Action::Tool(_))) {
        if let Action::Tool(t) = a {
            let mut active = world.resource_mut::<ActiveTool>();
            active.0 = if active.0 == t { Tool::Select } else { t };
        }
    }
    // Esc (a sketch stroke takes its own first): a drag in progress never
    // happened (its owner sees its gesture gone and stops); with none, it leaves the tool.
    if world.resource::<crate::capture::LiveCapture>().0.is_none() && !world.resource_mut::<PendingActions>().take(|a| a == Action::Cancel).is_empty() && !crate::history::cancel(world) {
        world.resource_mut::<ActiveTool>().0 = Tool::Select;
    }
}

/// The tracker (a `track` operator) whose point on `frame` is nearest `pos`
/// (source px), within `radius`.
pub fn pick_tracker(world: &mut World, frame: crate::time::FrameIndex, pos: [f64; 2], radius: f64) -> Option<Entity> {
    let mut q = world.query_filtered::<(Entity, &crate::op::Operator), Without<bevy_ecs::entity_disabling::Disabled>>();
    let trackers: Vec<Entity> = q.iter(world).filter(|(_, o)| o.kind == "track").map(|(e, _)| e).collect();
    trackers
        .into_iter()
        .filter_map(|e| {
            let sig = crate::span::output(world, e)?;
            let v = sig.get(frame)?;
            let d = (v[0] as f64 - pos[0]).hypot(v[1] as f64 - pos[1]);
            (d <= radius).then_some((e, d))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(e, _)| e)
}

/// What a press is on, decided once where it lands ([`route_press`]). The
/// Select tool's drags (`layer.rs`, `subject.rs`) and the click
/// ([`select_on_click`]) act on this; none of them hit-tests on its own, so
/// one press never moves two things.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PressOn {
    /// A layer's picture, or the selected layer's handles.
    Layer(Entity, crate::layer::Handle),
    /// A subject's point on the shown frame, or the selected subject's drawn path on this frame.
    Subject(Entity, crate::time::FrameIndex),
    /// The selected bendable path's ([`BendablePath`]: a manual dot's) point on this frame.
    Path(Entity, crate::time::FrameIndex),
    /// Nothing a drag takes: a click selects what is under it.
    Nothing,
}

/// A path its owner's drag can bend on any of its frames (tt_track puts it on
/// manual dots: their path is only what was drawn). Derived, never saved.
#[derive(Component, Debug, Default, Clone, Copy)]
pub struct BendablePath;

/// The last press (kept until the next one: its click reads it on the release).
#[derive(Resource, Debug, Default)]
pub struct Press(pub Option<PressOn>);

/// `Set::Input`: a press lands on the first of these under it, in this order:
/// 1. the selected layer (its handles or its picture);
/// 2. the selected subject's point on the shown frame;
/// 3. the top layer's picture;
/// 4. any subject's point on the shown frame;
/// 5. the selected subject's drawn path (the frame of it under the press);
/// 6. the selected bendable path ([`BendablePath`], a manual dot's): its frame under the press;
///
/// else nothing. Reaches are in screen points ([`crate::layer::HANDLE_REACH`], [`crate::layer::POINT_REACH`]).
fn route_press(world: &mut World) {
    use crate::layer::{HANDLE_REACH, Handle, POINT_REACH, handle_at, is_layer, pick_layer};
    use crate::subject::{is_subject, path_frame, pick_subject};
    let p = world.resource::<PointerFrame>();
    let Some(at) = p.pressed.and_then(|t| p.samples.iter().find(|s| s[0] >= t).map(|s| [s[1], s[2]]).or(p.hover)) else { return };
    let scale = if p.scale > 0.0 { p.scale } else { 1.0 };
    let frame = world.resource::<crate::transport::Transport>().frame();
    let view = world.resource::<crate::view::ActiveView>().0;
    let map = crate::view::map_at(world, view, frame);
    let (src, px) = (map.to_source(at), map.a / scale);
    let selected = world.resource::<crate::selection::Selection>().primary();
    let on = 'on: {
        if let Some(l) = selected.filter(|e| is_layer(world, *e))
            && let Some(h) = handle_at(world, l, frame, src, HANDLE_REACH * px)
        {
            break 'on PressOn::Layer(l, h);
        }
        let subject = pick_subject(world, frame, src, POINT_REACH * px);
        if let Some(s) = subject.filter(|s| Some(*s) == selected) {
            break 'on PressOn::Subject(s, frame);
        }
        if let Some(l) = pick_layer(world, frame, src) {
            break 'on PressOn::Layer(l, Handle::Body);
        }
        if let Some(s) = subject {
            break 'on PressOn::Subject(s, frame);
        }
        if let Some((s, f)) = selected.filter(|e| is_subject(world, *e)).and_then(|s| Some((s, path_frame(world, s, view, frame, at, POINT_REACH / scale)?))) {
            break 'on PressOn::Subject(s, f);
        }
        match selected.filter(|e| world.get::<BendablePath>(*e).is_some()).and_then(|e| Some((e, path_frame(world, e, view, frame, at, POINT_REACH / scale)?))) {
            Some((e, f)) => PressOn::Path(e, f),
            None => PressOn::Nothing,
        }
    };
    world.resource_mut::<Press>().0 = Some(on);
}

/// A click (in any tool but Track and Draw), by what its press was on
/// ([`route_press`]): a layer not selected yet is selected; the selected
/// subject (its point or its path) stays selected, and Alt+click takes out
/// its key drawn nearest the click; else the subject, tracker, SpringFocus
/// or sketch under it is selected (a selected layer's picture lets the click
/// reach what is below it), or nothing is.
fn select_on_click(world: &mut World) {
    let Some(pos) = world.resource::<PointerFrame>().click else { return };
    // In the Track tool a click places a tracker; in the Draw tool it draws.
    if matches!(world.resource::<ActiveTool>().0, Tool::Track | Tool::Draw) {
        return;
    }
    let frame = world.resource::<crate::transport::Transport>().frame();
    let view = world.resource::<crate::view::ActiveView>().0;
    let scale = world.resource::<PointerFrame>().scale;
    let scale = if scale > 0.0 { scale } else { 1.0 };
    let map = crate::view::map_at(world, view, frame);
    let selected = world.resource::<crate::selection::Selection>().primary();
    match world.resource::<Press>().0 {
        Some(PressOn::Layer(l, _)) if Some(l) != selected => {
            world.resource_mut::<crate::selection::Selection>().select_only(l);
            return;
        }
        // The selected bendable path: it stays selected (its drag bends it).
        Some(PressOn::Path(e, _)) if Some(e) == selected => return,
        Some(PressOn::Subject(s, _)) if Some(s) == selected => {
            if world.resource::<crate::input::KeysHeld>().mods.alt {
                let reach = crate::layer::POINT_REACH / scale;
                let keys = world.get::<crate::subject::Subject>(s).map(|p| p.offsets.clone()).unwrap_or_default();
                let near = keys
                    .iter()
                    .filter_map(|k| {
                        let v = crate::subject::value_at(world, s, k.frame)?;
                        let p = crate::view::map_at(world, view, k.frame).from_source([v[0], v[1]]);
                        Some((k.frame, (p[0] - pos[0]).hypot(p[1] - pos[1])))
                    })
                    .filter(|(_, d)| *d <= reach)
                    .min_by(|a, b| a.1.total_cmp(&b.1));
                if let Some((f, _)) = near {
                    crate::subject::remove_offset_key(world, s, f);
                }
            }
            return;
        }
        _ => {}
    }
    // The pointer is in the shown space's pixels; what's picked lives in source pixels.
    let (src, grab) = (map.to_source(pos), crate::layer::POINT_REACH * map.a / scale);
    let picked = crate::subject::pick_subject(world, frame, src, grab)
        .or_else(|| pick_tracker(world, frame, src, grab))
        .or_else(|| crate::focus::pick_focus(world, frame, src, grab))
        .or_else(|| crate::sketch::pick_sketch(world, frame, src));
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
            .declare::<PointerTrail>(Class::Derived)
            .init_resource::<ActiveTool>()
            .init_resource::<PointerFrame>()
            .init_resource::<ClickTracker>()
            .init_resource::<PointerTrail>()
            .declare::<Press>(Class::Derived)
            .declare::<BendablePath>(Class::Derived)
            .init_resource::<Press>()
            .add_systems(((detect_clicks, route_press).chain(), keep_trail).in_set(Set::Input))
            .add_systems((apply_tool_actions, crate::capture::sketch_tool, select_on_click).chain().in_set(Set::Tools));
    }
}
