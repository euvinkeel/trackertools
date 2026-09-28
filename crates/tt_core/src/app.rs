//! World bootstrap: schedules, system sets, and the [`Module`] registration API.
//!
//! Each app frame runs [`PreUi`], then the UI pass (egui, in `tt_app`), then
//! [`PostUi`]. The order of sets inside a schedule is fixed (DESIGN §2), so a
//! module only says *which phase* its systems belong to, never how phases relate.

use std::any::{TypeId, type_name};

use bevy_ecs::prelude::*;
use bevy_ecs::reflect::AppTypeRegistry;
use bevy_ecs::schedule::ScheduleLabel;
use bevy_ecs::system::ScheduleSystem;
use bevy_reflect::GetTypeRegistration;

use crate::meta::{Class, ComponentMetas, Created, CreationCounter, stamp_created};

/// Schedule run before the UI pass.
#[derive(ScheduleLabel, Debug, Clone, PartialEq, Eq, Hash)]
pub struct PreUi;

/// Schedule run after the UI pass (latency-critical intents, e.g. drags).
#[derive(ScheduleLabel, Debug, Clone, PartialEq, Eq, Hash)]
pub struct PostUi;

/// Phases of [`PreUi`], in execution order.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub enum Set {
    /// Raw input becomes timestamped events and actions.
    Input,
    /// The playhead advances from the wall clock; captures record clock maps.
    Transport,
    /// The active tool reads input; gestures open/update/commit edits.
    Tools,
    /// Intents emitted by panels and keys are applied.
    Intents,
    /// Dirty frame ranges propagate through the operator graph.
    Invalidate,
    /// Demanded dirty ranges of cheap operators are recomputed (budgeted).
    Evaluate,
    /// Background jobs start/stop; finished results merge into the world.
    Jobs,
    /// Decodes are requested for visible frames.
    Media,
    /// Render data is derived for panels (timeline summaries, overlays).
    Prepare,
}

/// Phases of [`PostUi`], in execution order.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub enum PostSet {
    Intents,
    Media,
}

/// A feature. Modules register components, resources and systems; they never
/// reach into each other directly — they meet in the world.
pub trait Module {
    fn build(&self, app: &mut AppBuilder);
}

pub struct AppBuilder {
    world: World,
    pre_ui: Schedule,
    post_ui: Schedule,
    modules: Vec<&'static str>,
}

impl Default for AppBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl AppBuilder {
    pub fn new() -> Self {
        let mut world = World::new();
        world.init_resource::<AppTypeRegistry>();
        world.init_resource::<ComponentMetas>();

        world.init_resource::<CreationCounter>();

        let mut pre_ui = Schedule::new(PreUi);
        pre_ui.configure_sets(
            (
                Set::Input,
                Set::Transport,
                Set::Tools,
                Set::Intents,
                Set::Invalidate,
                Set::Evaluate,
                Set::Jobs,
                Set::Media,
                Set::Prepare,
            )
                .chain(),
        );
        let mut post_ui = Schedule::new(PostUi);
        post_ui.configure_sets((PostSet::Intents, PostSet::Media).chain());

        let mut app = Self { world, pre_ui, post_ui, modules: Vec::new() };
        app.component::<Created>(Class::Document);
        app
    }

    /// Add a module once; adding the same module type twice is a no-op.
    pub fn add_module<M: Module + 'static>(&mut self, module: M) -> &mut Self {
        let name = type_name::<M>();
        if !self.modules.contains(&name) {
            self.modules.push(name);
            module.build(self);
        }
        self
    }

    /// Register a component type for reflection (inspector, persistence, undo)
    /// and declare its [`Class`]. An entity getting a document component is
    /// stamped with its creation order ([`Created`]).
    pub fn component<C: Component + GetTypeRegistration>(&mut self, class: Class) -> &mut Self {
        self.world.resource::<AppTypeRegistry>().write().register::<C>();
        self.world.resource_mut::<ComponentMetas>().insert::<C>(class);
        if class == Class::Document && TypeId::of::<C>() != TypeId::of::<Created>() {
            self.world.add_observer(stamp_created::<C>);
        }
        self
    }

    /// Register a resource type for reflection and declare its [`Class`].
    pub fn resource_type<R: Resource + GetTypeRegistration>(&mut self, class: Class) -> &mut Self {
        self.world.resource::<AppTypeRegistry>().write().register::<R>();
        self.world.resource_mut::<ComponentMetas>().insert::<R>(class);
        self
    }

    /// Declare the class of a type that is not reflected (e.g. UI layout trees).
    pub fn declare<T: 'static>(&mut self, class: Class) -> &mut Self {
        self.world.resource_mut::<ComponentMetas>().insert::<T>(class);
        self
    }

    /// Register a plain reflected type (field types of components, enums, …).
    pub fn register_type<T: GetTypeRegistration>(&mut self) -> &mut Self {
        self.world.resource::<AppTypeRegistry>().write().register::<T>();
        self
    }

    pub fn insert_resource<R: Resource>(&mut self, resource: R) -> &mut Self {
        self.world.insert_resource(resource);
        self
    }

    pub fn init_resource<R: Resource + FromWorld>(&mut self) -> &mut Self {
        self.world.init_resource::<R>();
        self
    }

    /// Add systems to [`PreUi`]; place them with `.in_set(Set::…)`.
    pub fn add_systems<M>(&mut self, systems: impl IntoScheduleConfigs<ScheduleSystem, M>) -> &mut Self {
        self.pre_ui.add_systems(systems);
        self
    }

    /// Add systems to [`PostUi`]; place them with `.in_set(PostSet::…)`.
    pub fn add_post_ui_systems<M>(&mut self, systems: impl IntoScheduleConfigs<ScheduleSystem, M>) -> &mut Self {
        self.post_ui.add_systems(systems);
        self
    }

    pub fn world_mut(&mut self) -> &mut World {
        &mut self.world
    }

    pub fn modules(&self) -> &[&'static str] {
        &self.modules
    }

    pub fn build(self) -> Core {
        let mut world = self.world;
        world.add_schedule(self.pre_ui);
        world.add_schedule(self.post_ui);
        world.insert_resource(ModuleList(self.modules));
        Core { world }
    }
}

/// Names of the modules a world was built from (for the dev panel).
#[derive(Resource, Debug, Clone)]
pub struct ModuleList(pub Vec<&'static str>);

/// A built world and its schedules.
pub struct Core {
    pub world: World,
}

impl Core {
    pub fn run_pre_ui(&mut self) {
        self.world.run_schedule(PreUi);
    }

    pub fn run_post_ui(&mut self) {
        self.world.run_schedule(PostUi);
    }
}
