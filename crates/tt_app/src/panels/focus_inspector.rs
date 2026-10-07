//! A SpringFocus in the Inspector (tt_core::focus): what it focuses on,
//! key by key, and its spring, with a picture of a move.
//!
//! - **Its keys:** each frame it moves over, to what (change it in its
//!   list), Go to and Remove.
//! - **On the shown frame:** *Move over to …* picks what it focuses on from
//!   here (a key here, or the one here changed).
//! - **The spring:** move time, bounce and lead, and a small graph of one
//!   move (from what it followed, 0, to the new target, 1) over time.

use bevy_ecs::prelude::*;
use tt_core::focus::{FocusParams, focus_on, set_focus, spring};
use tt_core::history::History;
use tt_core::input::{Action, PendingActions};
use tt_core::time::FrameIndex;
use tt_core::transport::Transport;

use crate::style;

/// Everything it can focus on: sketches, trackers, subjects (not itself or another SpringFocus).
fn choices(world: &mut World, me: Entity) -> Vec<(Entity, String)> {
    crate::tree::tree(world)
        .into_iter()
        .filter(|(e, _, k)| *e != me && *k != crate::tree::Node::Focus)
        .map(|(e, d, _)| (e, format!("{}{}", "  ".repeat(d), crate::panels::outliner::label(world, e))))
        .collect()
}

/// The Inspector's section for SpringFocus `e`.
pub fn section(ui: &mut egui::Ui, world: &mut World, e: Entity) {
    let Some(p) = world.get::<FocusParams>(e).cloned() else { return };
    let here = world.resource::<Transport>().frame();
    let fps = world.resource::<Transport>().fps.as_f64();
    let name = crate::panels::outliner::label(world, e);
    let all = choices(world, e);
    let label_of = |t: Entity| all.iter().find(|(x, _)| *x == t).map_or_else(|| "(gone)".to_string(), |(_, l)| l.trim().to_string());
    ui.label(
        egui::RichText::new(
            "One point that follows what it focuses on, and at each key moves smoothly over to the next thing: \
             once there it sits exactly on it. Tab follows it like a camera; right-click it to export it as a stabilized video.",
        )
        .weak()
        .small(),
    );
    let mut next = p.clone();
    let (mut started, mut stopped, mut seek, mut set_here) = (false, false, None, None);

    // On the shown frame.
    let now = p.target_at(here);
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(format!("Frame {here}:")).strong());
        let text = match p.key_at(here) {
            Some(k) => format!("moves over to {}", label_of(k.target)),
            None => format!("on {}", now.map_or_else(|| "nothing".to_string(), label_of)),
        };
        ui.label(text);
    });
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt(("focus-here", e)).selected_text("Move over to\u{2026}").show_ui(ui, |ui| {
            for (t, label) in &all {
                if ui.selectable_label(false, label).clicked() {
                    set_here = Some(*t);
                }
            }
        });
        ui.label(egui::RichText::new("from this frame (or right-click it on the video)").color(style::MUTED).small());
    });

    // Its keys.
    ui.add_space(4.0);
    ui.label(egui::RichText::new("Its keys").strong());
    let mut remove = None;
    egui::Grid::new(("focus-keys", e)).num_columns(3).spacing([6.0, 3.0]).show(ui, |ui| {
        for (i, k) in p.keys.iter().enumerate() {
            if ui.small_button(format!("frame {}", k.frame)).on_hover_text("Go to it").clicked() {
                seek = Some(k.frame);
            }
            egui::ComboBox::from_id_salt(("focus-key", e, i)).selected_text(label_of(k.target)).show_ui(ui, |ui| {
                for (t, label) in &all {
                    if ui.selectable_label(*t == k.target, label).clicked() {
                        next.keys[i].target = *t;
                    }
                }
            });
            if ui.add_enabled(p.keys.len() > 1, egui::Button::new("Remove").small()).on_hover_text("It stays on what it focused on before").clicked() {
                remove = Some(i);
            }
            ui.end_row();
        }
    });
    if let Some(i) = remove {
        next.keys.remove(i);
    }

    // The spring.
    ui.add_space(4.0);
    ui.label(egui::RichText::new("How it moves over").strong());
    egui::Grid::new(("focus-spring", e)).num_columns(2).spacing([6.0, 3.0]).show(ui, |ui| {
        // (label, value, drag speed, range, unit, tip)
        type Row<'a> = (&'a str, &'a mut f32, f64, std::ops::RangeInclusive<f64>, &'a str, &'a str);
        let rows: [Row; 3] = [
            ("Move time", &mut next.move_time, 0.01, 0.0..=10.0, " s", "Seconds a move takes to settle (0: a cut)"),
            ("Bounce", &mut next.bounce, 0.005, 0.0..=0.9, "", "0: it glides in and stops; more: it goes past and settles back"),
            ("Lead", &mut next.lead, 0.01, 0.0..=5.0, " s", "Seconds it starts moving before each key, so it arrives sooner"),
        ];
        for (label, v, speed, range, suffix, tip) in rows {
            ui.label(label).on_hover_text(tip);
            let r = ui.add(egui::DragValue::new(v).speed(speed).range(range).suffix(suffix).max_decimals(2));
            started |= r.drag_started();
            stopped |= r.drag_stopped();
            ui.end_row();
        }
    });
    move_graph(ui, &next, fps);

    if started {
        world.resource_mut::<History>().begin(format!("Edit {name}"));
    }
    if next != p {
        set_focus(world, e, &format!("Edit {name}"), |q| *q = next);
    }
    if stopped && world.resource::<History>().in_gesture() {
        world.resource_mut::<History>().end();
    }
    if let Some(t) = set_here {
        focus_on(world, e, here, t);
    }
    if let Some(f) = seek {
        world.resource_mut::<PendingActions>().push(Action::Seek(f as FrameIndex));
    }
}

/// One move over time: from what it followed (bottom) to the new target
/// (top), the key at the dashed line, the lead before it.
fn move_graph(ui: &mut egui::Ui, p: &FocusParams, fps: f64) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width().min(260.0), 64.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, egui::Color32::from_black_alpha(90));
    let lead = p.lead.max(0.0) as f64;
    let span = (lead + p.move_time.max(0.0) as f64 * 1.25).max(0.2);
    let pad = 6.0;
    let (x0, x1, y0, y1) = (rect.min.x + pad, rect.max.x - pad, rect.max.y - pad - 6.0, rect.min.y + pad + 6.0);
    let x = |t: f64| x0 + (x1 - x0) * (t / span) as f32;
    let y = |v: f64| y0 + (y1 - y0) * v as f32;
    for v in [0.0, 1.0] {
        painter.line_segment([egui::pos2(x0, y(v)), egui::pos2(x1, y(v))], egui::Stroke::new(1.0, egui::Color32::from_white_alpha(25)));
    }
    let key = x(lead);
    painter.add(egui::Shape::dashed_line(&[egui::pos2(key, rect.min.y + 2.0), egui::pos2(key, rect.max.y - 2.0)], egui::Stroke::new(1.0, style::MUTED), 3.0, 3.0));
    let pts: Vec<egui::Pos2> = (0..=80).map(|i| {
        let t = span * i as f64 / 80.0;
        egui::pos2(x(t), y(spring(t, p.move_time as f64, p.bounce as f64)))
    }).collect();
    painter.add(egui::Shape::line(pts, egui::Stroke::new(1.5, style::FOCUS)));
    let frames = (p.move_time as f64 * fps).round();
    painter.text(rect.right_bottom() - egui::vec2(4.0, 2.0), egui::Align2::RIGHT_BOTTOM, format!("{:.2} s \u{b7} {frames:.0} frames", p.move_time), egui::FontId::proportional(10.0), style::MUTED);
}
