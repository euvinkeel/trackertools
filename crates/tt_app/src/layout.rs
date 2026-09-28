//! Docked panel layout. The tree is session state in the world (DESIGN §14),
//! so it is saved with the session and any system can open or focus a panel.

use bevy_ecs::prelude::*;
use egui_tiles::{Container, Tile, TileId, Tiles, Tree};
use serde::{Deserialize, Serialize};
use tt_core::{AppBuilder, Class, Module};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Pane {
    Viewport,
    Timeline,
    Inspector,
    Outliner,
}

impl Pane {
    pub fn title(self) -> &'static str {
        match self {
            Pane::Viewport => "Viewport",
            Pane::Timeline => "Timeline",
            Pane::Inspector => "Inspector",
            Pane::Outliner => "Outliner",
        }
    }
}

#[derive(Resource)]
pub struct Layout {
    pub tree: Tree<Pane>,
}

impl Default for Layout {
    fn default() -> Self {
        let mut tiles = Tiles::default();
        let outliner = tiles.insert_pane(Pane::Outliner);
        let viewport = tiles.insert_pane(Pane::Viewport);
        let inspector = tiles.insert_pane(Pane::Inspector);
        let timeline = tiles.insert_pane(Pane::Timeline);
        let top = tiles.insert_horizontal_tile(vec![outliner, viewport, inspector]);
        let root = tiles.insert_vertical_tile(vec![top, timeline]);
        set_shares(&mut tiles, top, &[(outliner, 1.0), (viewport, 4.5), (inspector, 1.4)]);
        set_shares(&mut tiles, root, &[(top, 3.4), (timeline, 1.0)]);
        Self { tree: Tree::new("main_layout", root, tiles) }
    }
}

fn set_shares(tiles: &mut Tiles<Pane>, container: TileId, shares: &[(TileId, f32)]) {
    if let Some(Tile::Container(Container::Linear(linear))) = tiles.get_mut(container) {
        for (id, share) in shares {
            linear.shares.set_share(*id, *share);
        }
    }
}

pub struct LayoutModule;

impl Module for LayoutModule {
    fn build(&self, app: &mut AppBuilder) {
        app.declare::<Layout>(Class::Session)
            .init_resource::<Layout>()
            .declare::<crate::panels::outliner::OutlinerState>(Class::Session)
            .init_resource::<crate::panels::outliner::OutlinerState>();
    }
}
