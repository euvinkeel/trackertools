//! Inspector for M0: generic reflection views of core resources. M2 turns this
//! into registry-driven editing of any Document component, through transactions.

use bevy_ecs::prelude::*;
use bevy_reflect::PartialReflect;
use tt_core::time::WallClock;
use tt_core::transport::Transport;

use crate::reflect_ui;

pub fn ui(ui: &mut egui::Ui, world: &mut World) {
    egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
        section(ui, "Transport", world.resource::<Transport>().as_partial_reflect());
        section(ui, "Wall clock", world.resource::<WallClock>().as_partial_reflect());
    });
}

fn section(ui: &mut egui::Ui, title: &str, value: &dyn PartialReflect) {
    egui::CollapsingHeader::new(title).default_open(true).show(ui, |ui| reflect_ui::show(ui, value));
}
