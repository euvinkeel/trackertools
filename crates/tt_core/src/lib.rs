//! Headless core of trackertools v2.
//!
//! Everything the editor knows lives in one bevy_ecs [`World`](bevy_ecs::world::World):
//! document state, session state (playhead, selection, layout) and machinery.
//! Features plug in as [`Module`]s. This crate has no UI or GPU dependencies so
//! all of it is testable with `cargo test`. See docs/v2/DESIGN.md.

pub mod app;
pub mod capture;
pub mod history;
pub mod input;
pub mod meta;
pub mod op;
pub mod persist;
pub mod ranges;
pub mod selection;
pub mod signal;
pub mod sketch;
pub mod time;
pub mod tool;
pub mod transport;
pub mod view;

pub use app::{AppBuilder, Core, Module, PostSet, PostUi, PreUi, Set};
pub use meta::{Class, ComponentMeta, ComponentMetas};

/// The modules every trackertools world starts with.
pub struct CoreModules;

impl Module for CoreModules {
    fn build(&self, app: &mut AppBuilder) {
        app.add_module(time::TimeModule)
            .add_module(input::InputModule)
            .add_module(transport::TransportModule)
            .add_module(op::OpsModule)
            .add_module(history::HistoryModule)
            .add_module(selection::SelectionModule)
            .add_module(NamesModule)
            .add_module(sketch::SketchModule)
            .add_module(tool::ToolModule)
            .add_module(capture::CaptureModule)
            .add_module(view::ViewModule)
            .add_module(persist::PersistModule);
    }
}

/// Entity names (bevy's `Name`) are part of the document: saved, undoable,
/// shown by the outliner.
pub struct NamesModule;

impl Module for NamesModule {
    fn build(&self, app: &mut AppBuilder) {
        app.component::<bevy_ecs::name::Name>(Class::Document);
    }
}
