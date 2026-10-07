//! Drives a core world the way the app does: one PointerFrame per app frame
//! at 175 Hz with 1 kHz pointer samples.
#![allow(dead_code)]

use bevy_ecs::entity::Entity;
use tt_core::input::{Action, KeysHeld, Mods, PendingActions};
use tt_core::op::{Inputs, Operator, Output};
use tt_core::signal::SignalStore;
use tt_core::time::{Rational, WallClock};
use tt_core::tool::{ActiveTool, PointerFrame, Tool};
use tt_core::transport::Transport;
use tt_core::{AppBuilder, Core, CoreModules};

pub const UI_HZ: f64 = 175.0;

pub struct Driver {
    pub core: Core,
    pub now: f64,
}

#[derive(Default, Clone, Copy)]
pub struct Input {
    pub press: bool,
    pub down: bool,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub wheel: f32,
    pub action: Option<Action>,
}

pub const HOLD: Input = Input { press: false, down: true, shift: false, ctrl: false, alt: false, wheel: 0.0, action: None };
pub const PRESS: Input = Input { press: true, ..HOLD };
pub const UP: Input = Input { down: false, ..HOLD };

impl Driver {
    pub fn new() -> Self {
        let mut app = AppBuilder::new();
        app.add_module(CoreModules);
        let mut core = app.build();
        *core.world.resource_mut::<Transport>() =
            Transport { fps: Rational::new(60, 1), frame_count: 600, rate: 0.5, ..Transport::default() };
        core.world.resource_mut::<ActiveTool>().0 = Tool::Sketch;
        // Tests set playback rates themselves; the ones about auto speed turn it on.
        core.world.resource_mut::<tt_core::autospeed::AutoSpeed>().enabled = false;
        Self { core, now: 1.0 }
    }

    /// One app frame: the pointer follows `path(t)` (1 kHz samples since the last frame).
    pub fn frame(&mut self, path: impl Fn(f64) -> [f64; 2], input: Input) {
        let prev = self.now;
        self.now += 1.0 / UI_HZ;
        let w = &mut self.core.world;
        w.resource_mut::<WallClock>().tick(self.now);
        let mut samples = Vec::new();
        let mut t = (prev * 1000.0).floor() / 1000.0 + 0.001;
        while t <= self.now {
            let p = path(t);
            samples.push([t, p[0], p[1]]);
            t += 0.001;
        }
        *w.resource_mut::<PointerFrame>() = PointerFrame {
            samples,
            hover: Some(path(self.now)),
            pressed: input.press.then_some(prev + 0.002),
            down: input.down,
            released: (!input.down).then_some(self.now - 0.001),
            wheel: input.wheel,
            scale: 1.0,
            ..PointerFrame::default()
        };
        *w.resource_mut::<KeysHeld>() = KeysHeld { keys: Vec::new(), mods: Mods { shift: input.shift, ctrl: input.ctrl, alt: input.alt } };
        if let Some(a) = input.action {
            w.resource_mut::<PendingActions>().push(a);
        }
        self.core.run_pre_ui();
        self.core.run_post_ui();
    }

    pub fn frames(&mut self, n: usize, path: impl Fn(f64) -> [f64; 2] + Copy, input: Input) {
        for _ in 0..n {
            self.frame(path, input);
        }
    }

    pub fn sketches(&mut self) -> Vec<Entity> {
        let w = &mut self.core.world;
        let mut q = w.query::<(Entity, &Operator)>();
        let mut v: Vec<Entity> = q.iter(w).filter(|(_, o)| o.kind == "sketch").map(|(e, _)| e).collect();
        v.sort();
        v
    }

    pub fn strokes(&self, sketch: Entity) -> usize {
        self.core.world.get::<Inputs>(sketch).map_or(0, |i| i.0.iter().filter(|(s, _)| s == "stroke").count())
    }

    pub fn value(&self, sketch: Entity, f: i64) -> Option<[f32; 6]> {
        let w = &self.core.world;
        let out = w.get::<Output>(sketch)?.0;
        w.resource::<SignalStore>().get(out)?.get_valid(f).map(|v| v.try_into().unwrap())
    }

    pub fn transport(&self) -> Transport {
        self.core.world.resource::<Transport>().clone()
    }
}

pub fn still(x: f64, y: f64) -> impl Fn(f64) -> [f64; 2] + Copy {
    move |t| [x + 0.5 * (40.0 * t).sin(), y + 0.5 * (37.0 * t).cos()]
}

pub fn circle(t: f64) -> [f64; 2] {
    [500.0 + 100.0 * t.cos(), 300.0 + 100.0 * t.sin()]
}

/// A component read the way `persist::load` reads it from a project file
/// (RON through reflection), e.g. one saved before a field existed.
pub fn load_component<T: bevy_ecs::component::Component + Clone>(world: &mut bevy_ecs::world::World, ron_text: &str) -> T {
    use bevy_ecs::reflect::{AppTypeRegistry, ReflectComponent};
    use serde::de::DeserializeSeed;
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let registration = registry.get(std::any::TypeId::of::<T>()).expect("registered type");
    let mut de = ron::Deserializer::from_str(ron_text).expect("ron");
    let value = bevy_reflect::serde::TypedReflectDeserializer::new(registration, &registry).deserialize(&mut de).expect("deserialize");
    let e = world.spawn_empty().id();
    registration.data::<ReflectComponent>().expect("a component").insert(&mut world.entity_mut(e), value.as_ref(), &registry);
    let out = world.get::<T>(e).expect("inserted").clone();
    world.despawn(e);
    out
}

