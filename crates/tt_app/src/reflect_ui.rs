//! Generic, read-only view of any reflected value. The M2 inspector grows this
//! into editing (through transactions) so new components need no UI code.

use bevy_reflect::{PartialReflect, ReflectRef};

pub fn show(ui: &mut egui::Ui, value: &dyn PartialReflect) {
    match value.reflect_ref() {
        ReflectRef::Struct(s) => {
            egui::Grid::new(ui.next_auto_id()).num_columns(2).striped(true).show(ui, |ui| {
                for i in 0..s.field_len() {
                    let (Some(name), Some(field)) = (s.name_at(i), s.field_at(i)) else { continue };
                    ui.label(egui::RichText::new(name).weak());
                    if is_leaf(field) {
                        ui.monospace(leaf(field));
                    } else {
                        ui.vertical(|ui| show(ui, field));
                    }
                    ui.end_row();
                }
            });
        }
        _ => {
            ui.monospace(leaf(value));
        }
    }
}

fn is_leaf(value: &dyn PartialReflect) -> bool {
    match value.reflect_ref() {
        ReflectRef::Struct(s) => s.field_len() == 0 || is_small_struct(value),
        _ => true,
    }
}

/// Two-field numeric structs (rationals, vectors) read better inline.
fn is_small_struct(value: &dyn PartialReflect) -> bool {
    matches!(value.reflect_ref(), ReflectRef::Struct(s) if s.field_len() <= 2)
}

fn leaf(value: &dyn PartialReflect) -> String {
    if let Some(v) = value.try_downcast_ref::<f64>() {
        return format!("{v:.3}");
    }
    if let Some(v) = value.try_downcast_ref::<f32>() {
        return format!("{v:.3}");
    }
    if let ReflectRef::Struct(s) = value.reflect_ref() {
        let parts: Vec<String> =
            (0..s.field_len()).filter_map(|i| Some(format!("{}: {}", s.name_at(i)?, leaf(s.field_at(i)?)))).collect();
        return format!("{{ {} }}", parts.join(", "));
    }
    format!("{value:?}")
}
