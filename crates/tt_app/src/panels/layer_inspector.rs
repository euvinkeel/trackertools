//! A layer in the Inspector (tt_core::layer): its file, what it is
//! attached to and what it follows of it, a clip's timing, and every value
//! with its key on the shown frame.
//!
//! Each value is a number to drag or type, and a key button (drawn): a
//! filled diamond, a key on this frame (click: remove it); a hollow one,
//! keys on other frames (click: key this frame); a dot, no keys (click:
//! start keying, here). The arrows go to the value's previous and next key.
//! Changing a value with keys keys it on this frame; one without keys
//! changes its fixed value, unless Auto-key is on. A drag is one undo step.

use bevy_ecs::prelude::*;
use tt_core::history::History;
use tt_core::input::{Action, PendingActions};
use tt_core::layer::{AutoKey, BlendMode, EndMode, LayerParams, Restack, SizeMode, has_angle, reattach, restack, set_params, target_of};
use tt_core::time::FrameIndex;
use tt_core::transport::Transport;

use crate::style;

/// The Inspector's layer section for `e`.
pub fn section(ui: &mut egui::Ui, world: &mut World, e: Entity) {
    let Some(p) = world.get::<LayerParams>(e).cloned() else { return };
    let here = world.resource::<Transport>().frame();
    let name = crate::panels::outliner::label(world, e);
    let mut next = p.clone();
    let (mut started, mut stopped, mut seek, mut locate, mut attach_to, mut stack) = (false, false, None, false, None, None);

    // The file.
    let path = std::path::Path::new(&p.media);
    let file = path.file_name().map_or_else(|| p.media.clone(), |n| n.to_string_lossy().into_owned());
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new(&file).strong()).on_hover_text(&p.media);
        let what = if p.clip_duration > 0.0 {
            format!("{:.0}\u{d7}{:.0} \u{b7} {:.2} s at {:.0} fps", p.media_size[0], p.media_size[1], p.clip_duration, p.clip_fps)
        } else {
            format!("{:.0}\u{d7}{:.0} picture", p.media_size[0], p.media_size[1])
        };
        ui.label(egui::RichText::new(what).color(style::MUTED).small());
        if !path.exists() {
            ui.label(egui::RichText::new("not found").color(style::LOST));
        }
        if ui.small_button("Locate\u{2026}").on_hover_text("Choose its file again (moved or renamed)").clicked() {
            locate = true;
        }
    });

    // What it follows.
    let target = target_of(world, e);
    let targets: Vec<(Entity, String)> = crate::tree::tree(world)
        .into_iter()
        .filter(|(t, _, k)| *k != crate::tree::Node::Layer && *t != e)
        .map(|(t, d, _)| (t, format!("{}{}", "  ".repeat(d), crate::panels::outliner::label(world, t))))
        .collect();
    let target_name = target.map_or_else(|| "nothing".to_string(), |t| crate::panels::outliner::label(world, t));
    ui.horizontal(|ui| {
        ui.label("Attached to");
        egui::ComboBox::from_id_salt(("layer-target", e)).selected_text(target_name).show_ui(ui, |ui| {
            for (t, label) in &targets {
                if ui.selectable_label(Some(*t) == target, label).clicked() {
                    attach_to = Some(*t);
                }
            }
        });
    });
    let angled = target.is_some_and(|t| has_angle(world, t));
    ui.add_enabled_ui(angled, |ui| {
        ui.checkbox(&mut next.follow_rotation, "Turn with it").on_hover_text(if angled {
            "Turns as it turns (a subject's angle), and its offset turns with it"
        } else {
            "What it's attached to has no angle (only subjects do): make a subject of two or more trackers to turn with them"
        });
    });
    ui.horizontal(|ui| {
        ui.label("Size").on_hover_text("Fixed: its own scale only. With the box: it grows and shrinks with the box of what it's attached to, from the size it has on the reference frame. Its own scale multiplies on top.");
        egui::ComboBox::from_id_salt(("layer-size", e))
            .selected_text(match next.size {
                SizeMode::Fixed => "Fixed",
                SizeMode::Box => "With the box",
                SizeMode::Width => "With the box's width",
                SizeMode::Height => "With the box's height",
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut next.size, SizeMode::Fixed, "Fixed");
                ui.selectable_value(&mut next.size, SizeMode::Box, "With the box");
                ui.selectable_value(&mut next.size, SizeMode::Width, "With the box's width");
                ui.selectable_value(&mut next.size, SizeMode::Height, "With the box's height");
            });
        if next.size != SizeMode::Fixed {
            let r = next.size_frame.map_or_else(|| "its first frame".to_string(), |f| format!("frame {f}"));
            ui.label(egui::RichText::new(format!("as on {r}")).color(style::MUTED));
            if ui.small_button("This frame").on_hover_text("Its own size is the box's size on the shown frame").clicked() {
                next.size_frame = Some(here);
            }
            if next.size_frame.is_some() && ui.small_button("First").on_hover_text("Its own size is the box's size on its first frame").clicked() {
                next.size_frame = None;
            }
        }
    });

    ui.horizontal(|ui| {
        ui.label("Smoothing").on_hover_text("Steadies how it follows (seconds; no lag): the tracking's jitter goes, the tracking itself stays as it is. 0: exactly as tracked.");
        let r = ui.add(egui::DragValue::new(&mut next.smoothing).range(0.0..=2.0).speed(0.005).suffix(" s").max_decimals(3));
        started |= r.drag_started();
        stopped |= r.drag_stopped();
    });
    ui.horizontal(|ui| {
        ui.label("Blend").on_hover_text("How its colours mix with what's under it in exports. The preview here shows Normal.");
        egui::ComboBox::from_id_salt(("layer-blend", e)).selected_text(next.blend.label()).show_ui(ui, |ui| {
            for b in BlendMode::ALL {
                ui.selectable_value(&mut next.blend, b, b.label());
            }
        });
        if next.blend != BlendMode::Normal {
            ui.label(egui::RichText::new("in exports").color(style::MUTED).small());
        }
    });
    ui.horizontal(|ui| {
        ui.label("Stack");
        for (label, to, tip) in [("Back", Restack::Back, "Under every other layer"), ("Down", Restack::Down, "Under the layer below it"), ("Up", Restack::Up, "Over the layer above it"), ("Front", Restack::Front, "Over every other layer")] {
            if ui.small_button(label).on_hover_text(tip).clicked() {
                stack = Some(to);
            }
        }
    });

    // A clip's timing.
    if p.clip_duration > 0.0 {
        ui.horizontal(|ui| {
            ui.label("Starts at");
            let r = ui.add(egui::DragValue::new(&mut next.clip_in).range(0.0..=p.clip_duration.max(0.0)).speed(0.02).suffix(" s").max_decimals(2));
            started |= r.drag_started();
            stopped |= r.drag_stopped();
            ui.label("Speed");
            let r = ui.add(egui::DragValue::new(&mut next.speed).range(0.05..=8.0).speed(0.01).prefix("\u{d7}").max_decimals(2));
            started |= r.drag_started();
            stopped |= r.drag_stopped();
        });
        ui.horizontal(|ui| {
            ui.label("At its end");
            egui::ComboBox::from_id_salt(("layer-end", e))
                .selected_text(match next.end {
                    EndMode::Loop => "Loop",
                    EndMode::Hold => "Hold the last frame",
                    EndMode::Hide => "Disappear",
                    EndMode::PingPong => "Back and forth",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut next.end, EndMode::Loop, "Loop");
                    ui.selectable_value(&mut next.end, EndMode::Hold, "Hold the last frame");
                    ui.selectable_value(&mut next.end, EndMode::Hide, "Disappear");
                    ui.selectable_value(&mut next.end, EndMode::PingPong, "Back and forth");
                });
        });
    }

    // The values, each with its key.
    let mut auto = world.resource::<AutoKey>().0;
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(format!("On frame {here}")).strong());
        ui.checkbox(&mut auto, "Auto-key").on_hover_text("Changing a value that has no keys yet keys it on this frame (else it changes its fixed value). Values with keys always key.");
    });
    if auto != world.resource::<AutoKey>().0 {
        world.resource_mut::<AutoKey>().0 = auto;
    }
    egui::Grid::new(("layer-values", e)).num_columns(4).spacing([6.0, 3.0]).show(ui, |ui| {
        let specs: [(f64, &str, f64, Option<std::ops::RangeInclusive<f64>>); 7] = [
            (0.5, " px", 1.0, None),
            (0.5, " px", 1.0, None),
            (0.005, "\u{d7}", 1.0, Some(0.0..=f64::INFINITY)),
            (0.2, "\u{b0}", 1.0, None),
            (0.005, "", 1.0, Some(0.0..=1.0)),
            (0.002, "", 1.0, None),
            (0.002, "", 1.0, None),
        ];
        let tips = [
            "Right of what it follows (px; in its turned directions when it turns with it)",
            "Below what it follows (px)",
            "Its size: 1 is its own pixels as video pixels",
            "Turned clockwise (degrees), on top of what it turns with",
            "0 is invisible, 1 solid",
            "Which point of the picture sits on what it follows: 0 its left edge, 1 its right",
            "Which point of the picture sits on what it follows: 0 its top, 1 its bottom",
        ];
        let original = p.values();
        for (i, (label, a)) in next.values_mut().into_iter().enumerate() {
            let (speed, suffix, _, range) = &specs[i];
            ui.label(label).on_hover_text(tips[i]);
            let mut v = a.at(here) as f64;
            // (A value outside the range, set on the video, stays as it is until edited here.)
            let mut drag = egui::DragValue::new(&mut v).speed(*speed).suffix(*suffix).max_decimals(3);
            if let Some(r) = range.clone() {
                drag = drag.range(r).clamp_existing_to_range(false);
            }
            let r = ui.add(drag);
            started |= r.drag_started();
            stopped |= r.drag_stopped();
            if r.changed() {
                a.set(here, v as f32, auto);
            }
            // The key button: a diamond drawn (the fonts have none).
            let (state, tip) = if a.has_key(here) {
                (2, "A key on this frame: click to remove it")
            } else if !a.keys.is_empty() {
                (1, "Keyed on other frames: click to key this frame")
            } else {
                (0, "Not keyed: click to key it on this frame (then it changes over time)")
            };
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::click());
            let c = if resp.hovered() { style::TEXT } else if state == 0 { style::MUTED } else { style::LAYER };
            match state {
                2 => crate::icons::diamond(ui.painter(), rect.center(), 5.0, egui::Stroke::new(1.0, c), Some(c)),
                1 => crate::icons::diamond(ui.painter(), rect.center(), 5.0, egui::Stroke::new(1.2, c), None),
                _ => {
                    ui.painter().circle_filled(rect.center(), 2.0, c);
                }
            }
            if resp.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                a.toggle_key(here);
            }
            ui.horizontal(|ui| {
                let prev = original[i].1.keys.iter().rev().find(|k| k.frame < here).map(|k| k.frame);
                let after = original[i].1.keys.iter().find(|k| k.frame > here).map(|k| k.frame);
                if ui.add_enabled(prev.is_some(), egui::Button::new("\u{25c0}").small()).on_hover_text("Its previous key").clicked() {
                    seek = prev;
                }
                if ui.add_enabled(after.is_some(), egui::Button::new("\u{25b6}").small()).on_hover_text("Its next key").clicked() {
                    seek = after;
                }
            });
            ui.end_row();
        }
    });
    ui.label(
        egui::RichText::new("On the video (Select tool): drag it to move it, a corner to scale it, just outside a corner to turn it, Alt+drag to move its anchor. Its lane on the timeline sets when it starts and ends.")
            .weak()
            .small(),
    );

    if started {
        world.resource_mut::<History>().begin(format!("Edit {name}"));
    }
    if next != p {
        set_params(world, e, &format!("Edit {name}"), |q| *q = next);
    }
    if stopped && world.resource::<History>().in_gesture() {
        world.resource_mut::<History>().end();
    }
    if let Some(to) = stack {
        restack(world, e, to);
    }
    if let Some(t) = attach_to.filter(|t| Some(*t) != target) {
        reattach(world, e, t);
    }
    if locate {
        crate::layers::locate(world, e);
    }
    if let Some(f) = seek {
        world.resource_mut::<PendingActions>().push(Action::Seek(f as FrameIndex));
    }
}
