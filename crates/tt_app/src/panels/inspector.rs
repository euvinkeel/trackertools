//! Inspector: every reflected component of the selected entity, editable with
//! generic widgets. Edits go through transactions (undoable); a drag is one
//! undo step. Below, read-only views of core resources.

use bevy_ecs::prelude::*;
use bevy_ecs::reflect::{AppTypeRegistry, ReflectComponent};
use bevy_reflect::PartialReflect;
use tt_core::capture::SketchDefaults;
use tt_core::history::edit;
use tt_core::meta::{Class, ComponentMetas};
use tt_core::selection::Selection;
use tt_core::sketch::SketchParams;
use tt_core::time::WallClock;
use tt_core::transport::Transport;

use crate::reflect_ui;

pub fn ui(ui: &mut egui::Ui, world: &mut World) {
    egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
        match world.resource::<Selection>().primary() {
            Some(e) => entity_section(ui, world, e),
            None => {
                ui.label(egui::RichText::new("Nothing selected. Select something in the Outliner.").weak());
            }
        }
        ui.separator();
        egui::CollapsingHeader::new("Transport").default_open(false).show(ui, |ui| {
            reflect_ui::show(ui, world.resource::<Transport>().as_partial_reflect())
        });
        egui::CollapsingHeader::new("Wall clock").default_open(false).show(ui, |ui| {
            reflect_ui::show(ui, world.resource::<WallClock>().as_partial_reflect())
        });
    });
}

fn entity_section(ui: &mut egui::Ui, world: &mut World, e: Entity) {
    ui.heading(crate::panels::outliner::label(world, e));
    super::tracks::inspector(ui, world, e);
    if tt_core::subject::is_subject(world, e) {
        subject_section(ui, world, e);
    }
    if tt_core::focus::is_focus(world, e) {
        super::focus_inspector::section(ui, world, e);
    }
    if tt_core::layer::is_layer(world, e) {
        super::layer_inspector::section(ui, world, e);
    }
    if world.get::<SketchParams>(e).is_some() {
        sketch_color(ui, world, e);
        sketch_presets(ui, world, e);
    }
    components(ui, world, e);
    // A sketch's view (its framing) is edited right here too.
    if let Some(view) = tt_core::view::view_of(world, e) {
        ui.separator();
        egui::CollapsingHeader::new("View (Tab to enter)").id_salt(("view", view)).default_open(true).show(ui, |ui| components(ui, world, view));
    }
}

/// A sketch's colour: a swatch per choice (one undo step a click), and Auto
/// (its place in the palette). Its trackers take it too.
fn sketch_color(ui: &mut egui::Ui, world: &mut World, e: Entity) {
    use tt_core::sketch::Tint;
    let now = crate::colors::Colors::new(world).sketch(e);
    let own = world.get::<Tint>(e).copied();
    let mut pick: Option<Option<Tint>> = None;
    ui.horizontal_wrapped(|ui| {
        ui.label("Colour").on_hover_text("Its box, path and timeline lane, and the trackers that follow it. Pick one that shows on this video.");
        let choices = crate::colors::PALETTE.iter().copied().chain([egui::Color32::WHITE, egui::Color32::from_rgb(0x11, 0x11, 0x11), egui::Color32::from_rgb(0x38, 0xbd, 0xf8)]);
        for c in choices {
            let (rect, response) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::click());
            let chosen = own.is_some() && c == now;
            ui.painter().rect_filled(rect.shrink(1.0), 3.0, c);
            ui.painter().rect_stroke(rect.shrink(1.0), 3.0, egui::Stroke::new(if chosen { 2.0 } else { 1.0 }, if chosen { egui::Color32::WHITE } else { egui::Color32::from_gray(70) }), egui::StrokeKind::Inside);
            if response.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                pick = Some(Some(crate::colors::tint_of(c)));
            }
        }
        if ui.add_enabled(own.is_some(), egui::Button::new("Auto").small()).on_hover_text("Its own place in the palette").clicked() {
            pick = Some(None);
        }
    });
    if let Some(choice) = pick.filter(|c| *c != own) {
        let name = world.get::<bevy_ecs::name::Name>(e).map_or("the sketch".to_string(), |n| n.to_string());
        edit(world, &format!("Colour of {name}"), |tx| match choice {
            Some(t) => tx.insert(e, t),
            None => tx.remove::<Tint>(e),
        });
    }
}

/// Every reflected document/session component of `e`, editable.
fn components(ui: &mut egui::Ui, world: &mut World, e: Entity) {
    // Editable copies of each reflected document/session component on the entity.
    let manual = tt_track::human::is_manual(world, e);
    let mut components: Vec<(String, String, Box<dyn PartialReflect>, Class)> = {
        let registry = world.resource::<AppTypeRegistry>().read();
        let metas = world.resource::<ComponentMetas>();
        let Ok(entity) = world.get_entity(e) else { return };
        registry
            .iter()
            .filter_map(|r| {
                let class = metas.get(r.type_id())?.class;
                // Derived state isn't edited; the name is the heading; creation order, a tracker's stamp
                // and its layers' signals are bookkeeping (a manual dot has no tracking settings).
                let hidden = [
                    std::any::TypeId::of::<bevy_ecs::name::Name>(),
                    std::any::TypeId::of::<tt_core::meta::Created>(),
                    std::any::TypeId::of::<tt_track::runner::TrackBook>(),
                    std::any::TypeId::of::<tt_track::human::AutoOutput>(),
                    std::any::TypeId::of::<tt_track::human::HumanLayer>(),
                    // (A sketch's colour has its own row: `sketch_color`.)
                    std::any::TypeId::of::<tt_core::sketch::Tint>(),
                    // (A SpringFocus's keys and spring have their own section: `focus_inspector`.)
                    std::any::TypeId::of::<tt_core::focus::FocusParams>(),
                    // (A layer's values have their own section: `layer_inspector`.)
                    std::any::TypeId::of::<tt_core::layer::LayerParams>(),
                    // (A tracker's cursor icons have their own section: `cursor_icons`.)
                    std::any::TypeId::of::<tt_track::icons::CursorIcons>(),
                ];
                if class == Class::Derived || hidden.contains(&r.type_id()) || (manual && r.type_id() == std::any::TypeId::of::<tt_track::Tracker>()) {
                    return None;
                }
                let rc = r.data::<ReflectComponent>()?;
                let value = rc.reflect(entity)?;
                let info = r.type_info().type_path_table();
                Some((info.path().to_string(), info.short_path().to_string(), value.to_dynamic(), class))
            })
            .collect()
    };
    components.sort_by(|a, b| a.1.cmp(&b.1));

    // (Text fields as wide as half the panel, not egui's 280 px: a name beside an entity ran off a narrow panel.)
    ui.spacing_mut().text_edit_width = (ui.available_width() * 0.5).clamp(60.0, 280.0);
    let mut pending: Option<PendingEdit> = None;
    for (path, short, mut value, class) in components {
        egui::CollapsingHeader::new(&short).id_salt((&path, e)).default_open(true).show(ui, |ui| {
            if class == Class::Session {
                ui.label(egui::RichText::new("session (not undoable)").weak().small());
            }
            let r = reflect_ui::edit(ui, value.as_mut());
            if r.changed || r.drag_started || r.dragging || r.drag_stopped {
                pending = Some((path.clone(), short.clone(), value, r, class));
            }
        });
    }

    let Some((path, short, value, r, class)) = pending else { return };
    if class == Class::Session {
        // Session state applies directly: not part of the undo history.
        let rc = world.resource::<AppTypeRegistry>().read().get_with_type_path(&path).and_then(|t| t.data::<ReflectComponent>().cloned());
        if let (Some(rc), true) = (rc, r.changed) {
            rc.apply(world.entity_mut(e), value.as_ref());
        }
        return;
    }
    // A drag is one undo step.
    let drag = tt_core::history::WidgetDrag { started: r.drag_started, dragging: r.dragging, changed: r.changed, stopped: r.drag_stopped };
    tt_core::history::widget_edit(world, &format!("Edit {short}"), drag, |w| {
        edit(w, &format!("Edit {short}"), |tx| {
            tx.set_reflected(e, &path, value.as_ref());
        });
    });
}

/// A subject (tt_core::subject): its members, its own offset on the shown
/// frame (editing keys it there; a drag is one undo step), its anchor, and
/// its keys (click one to go to its frame).
fn subject_section(ui: &mut egui::Ui, world: &mut World, e: Entity) {
    use tt_core::input::{Action, PendingActions};
    use tt_core::subject::{OffsetKey, Subject, members_of, offset_at, remove_offset_key, set_offset_key};
    let Some(subject) = world.get::<Subject>(e).cloned() else { return };
    let here = world.resource::<Transport>().frame();
    let members = members_of(world, e);
    let name = crate::panels::outliner::label(world, e);
    ui.label(
        egui::RichText::new(
            "It moves with its members' motion (one coming, going or getting lost never makes it jump), plus its own offset. \
             Drag it on the video (Select tool) to put it where you want it on this frame: that keys the offset.",
        )
        .weak()
        .small(),
    );
    let mut select: Option<Entity> = None;
    let (mut add, mut take_out): (Option<Entity>, Option<Entity>) = (None, None);
    // What could join it: trackers, sketches, SpringFocuses and other subjects, in the outliner's order.
    let joinable: Vec<Entity> = crate::tree::tree(world)
        .into_iter()
        .filter(|(m, _, kind)| matches!(kind, crate::tree::Node::Tracker | crate::tree::Node::Sketch | crate::tree::Node::Subject | crate::tree::Node::Focus) && tt_core::subject::can_join(world, e, *m))
        .map(|(m, _, _)| m)
        .collect();
    let colors = crate::colors::Colors::new(world);
    ui.label(format!("{} member{}", members.len(), if members.len() == 1 { "" } else { "s" }));
    for m in &members {
        ui.horizontal(|ui| {
            let glyph = crate::icons::Glyph::of(world, *m);
            crate::icons::icon_in(ui, glyph, false, colors.of(world, *m, glyph));
            if ui.link(crate::panels::outliner::label(world, *m)).on_hover_text("Select it").clicked() {
                select = Some(*m);
            }
            // Whether it carries the subject on this frame (a good point here).
            if tt_core::subject::points_of(world, *m).binary_search_by_key(&here, |(f, _)| *f).is_err() {
                ui.label(egui::RichText::new("no point here").color(crate::style::MUTED).small())
                    .on_hover_text("Lost, off, outside its lifetime or not tracked on this frame: the others carry the subject here");
            }
            if crate::icons::cross_button(ui, &format!("Take it out of {name}")).clicked() {
                take_out = Some(*m);
            }
        });
    }
    ui.add_enabled_ui(!joinable.is_empty(), |ui| {
        ui.menu_button("Add member \u{23f7}", |ui| {
            for m in &joinable {
                if ui.button(crate::panels::outliner::label(world, *m)).clicked() {
                    add = Some(*m);
                    ui.close();
                }
            }
        })
        .response
        .on_hover_text("Another tracker, sketch, SpringFocus or subject whose motion carries it too")
        .on_disabled_hover_text("Nothing else can join it (a tracker in a view that follows it can't: it would follow itself)");
    });
    let keyed = subject.offsets.iter().any(|k| k.frame == here);
    let [x, y, a] = offset_at(&subject.offsets, here, subject.ease);
    let (mut x, mut y, mut a) = (x as f32, y as f32, a.to_degrees() as f32);
    let mut key_drag = tt_core::history::WidgetDrag::default();
    ui.horizontal(|ui| {
        ui.label(if keyed { format!("Its key on frame {here}:") } else { format!("Its offset on frame {here}:") }).on_hover_text(
            "Where on the moving thing it sits (px, in its starting frame's directions, so it turns with the thing) and how much more it is turned. \
             From key to key as 'Between keys' says, held beyond them. Changing it here keys it on this frame.",
        );
        for (v, suffix, speed) in [(&mut x, " x", 0.5), (&mut y, " y", 0.5), (&mut a, "\u{b0}", 0.2)] {
            let r = ui.add(egui::DragValue::new(v).speed(speed).max_decimals(2).suffix(suffix));
            key_drag.changed |= r.changed();
            key_drag.started |= r.drag_started();
            key_drag.dragging |= r.dragged();
            key_drag.stopped |= r.drag_stopped();
        }
    });
    let (mut unkey, mut anchor) = (false, false);
    ui.horizontal(|ui| {
        if keyed && ui.button("Remove this key").clicked() {
            unkey = true;
        }
        if subject.anchor != here
            && ui
                .button(format!("Start it here (frame {here})"))
                .on_hover_text(format!("Its anchor is frame {}: there it starts in the middle of its members, and their motion carries it both ways from there.", subject.anchor))
                .clicked()
        {
            anchor = true;
        }
    });
    let mut go: Option<tt_core::time::FrameIndex> = None;
    if !subject.offsets.is_empty() {
        ui.horizontal_wrapped(|ui| {
            ui.label("Keys:");
            for k in &subject.offsets {
                let tip = format!("Go to frame {}: {:+.1}, {:+.1} px, {:+.1}\u{b0}", k.frame, k.x, k.y, k.angle);
                if ui.small_button(k.frame.to_string()).on_hover_text(tip).clicked() {
                    go = Some(k.frame);
                }
            }
        });
    }
    // The whole path smoothed, and how it goes from key to key.
    let (mut smooth, mut ease) = (subject.smooth, subject.ease);
    let mut smooth_drag = tt_core::history::WidgetDrag::default();
    ui.horizontal(|ui| {
        ui.label("Smooth its path").on_hover_text(
            "Seconds of smoothing on its members' motion, over the whole path (0: none). Its keys stay exactly where they are. \
             An export's own smoothing comes on top of this.",
        );
        let r = ui.add(egui::Slider::new(&mut smooth, 0.0..=2.0).step_by(0.01).suffix(" s"));
        smooth_drag = tt_core::history::WidgetDrag { started: r.drag_started(), dragging: r.dragged(), changed: false, stopped: r.drag_stopped() };
    });
    ui.horizontal(|ui| {
        ui.label("Between keys");
        ui.selectable_value(&mut ease, tt_core::subject::Ease::Smooth, "Smooth").on_hover_text("Eases out of each key and into the next: no corner at a key");
        ui.selectable_value(&mut ease, tt_core::subject::Ease::Linear, "Linear").on_hover_text("A straight line from key to key");
    });
    ui.label(egui::RichText::new("Drag its path on the video to bend it there (the wheel while dragging: how far). Alt+click a key on the path to remove it.").weak().small());
    ui.separator();
    smooth_drag.changed = smooth != subject.smooth || ease != subject.ease;
    tt_core::history::widget_edit(world, &format!("Smooth {name}"), smooth_drag, |w| {
        edit(w, &format!("Smooth {name}"), |tx| tx.modify::<Subject>(e, |s| (s.smooth, s.ease) = (smooth.max(0.0), ease)));
    });
    tt_core::history::widget_edit(world, &format!("Key {name}"), key_drag, |w| set_offset_key(w, e, OffsetKey { frame: here, x, y, angle: a }));
    if unkey {
        remove_offset_key(world, e, here);
    }
    if anchor {
        edit(world, &format!("Start {name} on frame {here}"), |tx| tx.modify::<Subject>(e, |s| s.anchor = here));
    }
    if let Some(f) = go {
        world.resource_mut::<PendingActions>().push(Action::Seek(f));
    }
    if let Some(m) = select {
        world.resource_mut::<Selection>().select_only(m);
    }
    if let Some(m) = add {
        tt_core::subject::add_members(world, e, &[m]);
    }
    if let Some(m) = take_out {
        tt_core::subject::remove_members(world, e, &[m]);
    }
}

/// Preset buttons over a sketch's numbers, and "use for new sketches".
fn sketch_presets(ui: &mut egui::Ui, world: &mut World, e: Entity) {
    let current = world.get::<SketchParams>(e).cloned().unwrap_or_default();
    let mut chosen = None;
    let mut make_default = false;
    ui.horizontal(|ui| {
        ui.label("Preset");
        for name in SketchParams::PRESETS {
            let preset = SketchParams::preset(name).expect("listed preset");
            if ui.selectable_label(preset == current, name).clicked() && preset != current {
                chosen = Some((name, preset));
            }
        }
        let is_default = world.resource::<SketchDefaults>().params == current;
        make_default = ui
            .add_enabled(!is_default, egui::Button::new("Use for new sketches").small())
            .on_hover_text("New captures start with these numbers")
            .on_disabled_hover_text("New captures already start with these numbers")
            .clicked();
    });
    if let Some((name, preset)) = chosen {
        edit(world, &format!("Preset {name}"), |tx| tx.insert(e, preset));
    }
    if make_default {
        world.resource_mut::<SketchDefaults>().params = current;
    }
}

/// An edit made this frame: (type path, short name, new value, what happened, class).
type PendingEdit = (String, String, Box<dyn PartialReflect>, reflect_ui::Edited, Class);
