//! Generic views and editors for any reflected value (DESIGN §11): a new
//! component gets an inspector with no UI code. Numbers are drag fields,
//! booleans checkboxes, strings text fields, unit-variant enums dropdowns;
//! structs and tuples nest. Entity references and exact rationals are shown
//! read-only.

use bevy_ecs::entity::Entity;
use bevy_reflect::enums::{DynamicEnum, DynamicVariant};
use bevy_reflect::{PartialReflect, ReflectMut, ReflectRef, TypeInfo};

/// What an edit pass did: the value changed, and whether a drag gesture
/// started or ended (so a whole drag becomes one undo step).
#[derive(Default, Clone, Copy)]
pub struct Edited {
    pub changed: bool,
    pub drag_started: bool,
    pub drag_stopped: bool,
}

impl Edited {
    fn merge(&mut self, r: &egui::Response) {
        self.changed |= r.changed();
        self.drag_started |= r.drag_started();
        self.drag_stopped |= r.drag_stopped();
    }

    fn absorb(&mut self, other: Edited) {
        self.changed |= other.changed;
        self.drag_started |= other.drag_started;
        self.drag_stopped |= other.drag_stopped;
    }
}

/// Editable view of `value` (modified in place).
/// Lists longer than this show their length instead of every element.
const MAX_LIST: usize = 16;

pub fn edit(ui: &mut egui::Ui, value: &mut dyn PartialReflect) -> Edited {
    let mut out = Edited::default();
    if edit_leaf(ui, value, &mut out) {
        return out;
    }
    let enum_variants = value.get_represented_type_info().and_then(|info| match info {
        TypeInfo::Enum(e) => Some(e.variant_names().iter().map(|n| n.to_string()).collect::<Vec<_>>()),
        _ => None,
    });
    match value.reflect_mut() {
        ReflectMut::Struct(s) => {
            egui::Grid::new(ui.next_auto_id()).num_columns(2).show(ui, |ui| {
                for i in 0..s.field_len() {
                    let name = s.name_at(i).unwrap_or("?").to_string();
                    ui.label(egui::RichText::new(name).weak());
                    if let Some(f) = s.field_at_mut(i) {
                        out.absorb(ui.vertical(|ui| edit(ui, f)).inner);
                    }
                    ui.end_row();
                }
            });
        }
        ReflectMut::TupleStruct(s) => {
            ui.horizontal(|ui| {
                for i in 0..s.field_len() {
                    if let Some(f) = s.field_mut(i) {
                        out.absorb(edit(ui, f));
                    }
                }
            });
        }
        ReflectMut::Tuple(t) => {
            ui.horizontal(|ui| {
                for i in 0..t.field_len() {
                    if let Some(f) = t.field_mut(i) {
                        out.absorb(edit(ui, f));
                    }
                }
            });
        }
        // Long lists (a capture's clock map) are data, not settings: summarised.
        ReflectMut::List(l) if l.len() > MAX_LIST => {
            ui.label(egui::RichText::new(format!("{} items", l.len())).weak());
        }
        ReflectMut::List(l) => {
            ui.vertical(|ui| {
                for i in 0..l.len() {
                    if let Some(f) = l.get_mut(i) {
                        out.absorb(edit(ui, f));
                    }
                }
                if l.is_empty() {
                    ui.label(egui::RichText::new("(empty)").weak());
                }
            });
        }
        ReflectMut::Enum(e) => {
            let current = e.variant_name().to_string();
            let unit = e.field_len() == 0;
            match enum_variants {
                Some(names) if unit => {
                    let mut chosen = None;
                    egui::ComboBox::from_id_salt(ui.next_auto_id()).selected_text(&current).show_ui(ui, |ui| {
                        for n in &names {
                            if ui.selectable_label(*n == current, n).clicked() && *n != current {
                                chosen = Some(n.clone());
                            }
                        }
                    });
                    if let Some(n) = chosen {
                        e.apply(&DynamicEnum::new(n, DynamicVariant::Unit));
                        out.changed = true;
                    }
                }
                _ => {
                    ui.horizontal(|ui| {
                        ui.label(current);
                        for i in 0..e.field_len() {
                            if let Some(f) = e.field_at_mut(i) {
                                out.absorb(edit(ui, f));
                            }
                        }
                    });
                }
            }
        }
        _ => {
            ui.monospace(format!("{value:?}"));
        }
    }
    out
}

/// Leaf types with a direct widget. Returns false if `value` isn't a leaf.
fn edit_leaf(ui: &mut egui::Ui, value: &mut dyn PartialReflect, out: &mut Edited) -> bool {
    macro_rules! drag {
        ($($t:ty),*) => {$(
            if let Some(v) = value.try_downcast_mut::<$t>() {
                let speed = drag_speed(*v as f64);
                out.merge(&ui.add(egui::DragValue::new(v).speed(speed)));
                return true;
            }
        )*};
    }
    drag!(f32, f64, i32, i64, u32, u64, usize, u8, u16, i16);
    if let Some(v) = value.try_downcast_mut::<bool>() {
        out.merge(&ui.checkbox(v, ""));
        return true;
    }
    if let Some(v) = value.try_downcast_mut::<String>() {
        out.merge(&ui.text_edit_singleline(v));
        return true;
    }
    if let Some(e) = value.try_downcast_ref::<Entity>() {
        ui.monospace(format!("entity {e}"));
        return true;
    }
    if let Some(r) = value.try_downcast_ref::<tt_core::time::Rational>() {
        ui.monospace(format!("{}/{}", r.num, r.den));
        return true;
    }
    false
}

fn drag_speed(v: f64) -> f64 {
    (v.abs() * 0.01).clamp(0.01, 10.0)
}

/// Read-only view (for values that aren't components, e.g. resources).
pub fn show(ui: &mut egui::Ui, value: &dyn PartialReflect) {
    match value.reflect_ref() {
        ReflectRef::Struct(s) => {
            egui::Grid::new(ui.next_auto_id()).num_columns(2).striped(true).show(ui, |ui| {
                for i in 0..s.field_len() {
                    let (Some(name), Some(field)) = (s.name_at(i), s.field_at(i)) else { continue };
                    ui.label(egui::RichText::new(name).weak());
                    ui.monospace(leaf(field));
                    ui.end_row();
                }
            });
        }
        _ => {
            ui.monospace(leaf(value));
        }
    }
}

fn leaf(value: &dyn PartialReflect) -> String {
    if let Some(v) = value.try_downcast_ref::<f64>() {
        return format!("{v:.3}");
    }
    if let Some(v) = value.try_downcast_ref::<f32>() {
        return format!("{v:.3}");
    }
    if let Some(r) = value.try_downcast_ref::<tt_core::time::Rational>() {
        return format!("{}/{}", r.num, r.den);
    }
    if let ReflectRef::Struct(s) = value.reflect_ref() {
        let parts: Vec<String> =
            (0..s.field_len()).filter_map(|i| Some(format!("{}: {}", s.name_at(i)?, leaf(s.field_at(i)?)))).collect();
        return format!("{{ {} }}", parts.join(", "));
    }
    format!("{value:?}")
}
