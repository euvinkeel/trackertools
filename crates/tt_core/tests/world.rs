//! World wiring: set order, module registration, actions reaching the transport.

use bevy_ecs::prelude::*;
use bevy_ecs::reflect::AppTypeRegistry;
use bevy_reflect::Reflect;
use tt_core::input::{Action, PendingActions};
use tt_core::time::{Rational, WallClock};
use tt_core::transport::Transport;
use tt_core::{AppBuilder, Class, ComponentMetas, CoreModules, Module, PostSet, Set};

#[derive(Resource, Default)]
struct Log(Vec<&'static str>);

#[test]
fn sets_run_in_declared_order_regardless_of_registration_order() {
    let mut app = AppBuilder::new();
    app.init_resource::<Log>();
    // Registered in reverse on purpose.
    app.add_systems((|mut l: ResMut<Log>| l.0.push("prepare")).in_set(Set::Prepare));
    app.add_systems((|mut l: ResMut<Log>| l.0.push("evaluate")).in_set(Set::Evaluate));
    app.add_systems((|mut l: ResMut<Log>| l.0.push("tools")).in_set(Set::Tools));
    app.add_systems((|mut l: ResMut<Log>| l.0.push("input")).in_set(Set::Input));
    app.add_post_ui_systems((|mut l: ResMut<Log>| l.0.push("post-media")).in_set(PostSet::Media));
    app.add_post_ui_systems((|mut l: ResMut<Log>| l.0.push("post-intents")).in_set(PostSet::Intents));
    let mut core = app.build();

    core.run_pre_ui();
    core.run_post_ui();
    assert_eq!(
        core.world.resource::<Log>().0,
        ["input", "tools", "evaluate", "prepare", "post-intents", "post-media"]
    );
}

#[derive(Component, Reflect, Default)]
#[reflect(Component)]
struct Probe {
    value: f32,
}

struct ProbeModule;

impl Module for ProbeModule {
    fn build(&self, app: &mut AppBuilder) {
        app.component::<Probe>(Class::Document);
    }
}

#[test]
fn modules_register_reflection_and_class_once() {
    let mut app = AppBuilder::new();
    app.add_module(ProbeModule).add_module(ProbeModule);
    assert_eq!(app.modules().len(), 1);
    let core = app.build();

    let registry = core.world.resource::<AppTypeRegistry>().read();
    let reg = registry.get(std::any::TypeId::of::<Probe>()).expect("Probe registered");
    assert!(reg.data::<bevy_ecs::reflect::ReflectComponent>().is_some());
    drop(registry);
    assert_eq!(core.world.resource::<ComponentMetas>().class_of::<Probe>(), Some(Class::Document));
}

#[test]
fn actions_and_clock_drive_the_transport() {
    let mut app = AppBuilder::new();
    app.add_module(CoreModules);
    let mut core = app.build();
    {
        let mut t = core.world.resource_mut::<Transport>();
        t.fps = Rational::new(60, 1);
        t.frame_count = 600;
    }

    // Space pressed and 0.2 s of wall time pass in the same frame
    // (single ticks are clamped to 0.25 s, see WallClock::tick).
    core.world.resource_mut::<PendingActions>().push(Action::TogglePlay);
    core.world.resource_mut::<WallClock>().tick(0.2);
    core.run_pre_ui();
    let t = core.world.resource::<Transport>();
    assert!(t.playing);
    assert_eq!(t.frame(), 12);
    assert!(core.world.resource::<PendingActions>().0.is_empty());

    // A step pauses and moves exactly one frame.
    core.world.resource_mut::<PendingActions>().push(Action::StepForward);
    core.world.resource_mut::<WallClock>().tick(0.216);
    core.run_pre_ui();
    let t = core.world.resource::<Transport>();
    assert!(!t.playing);
    assert_eq!(t.frame(), 13);
}
