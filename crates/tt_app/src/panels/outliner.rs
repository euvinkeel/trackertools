//! Outliner: the document as a tree. Each sketch lists the sketches drawn
//! inside its view (nested under it) and its strokes (folded under "n
//! strokes"); other document entities follow. Type in the box to filter.
//!
//! - Click selects, Ctrl+click toggles, Shift+click selects the range from
//!   the last click.
//! - Drag on the list draws a box that selects every row it touches (Shift or
//!   Ctrl add to the selection); a click on empty space deselects.
//! - Double-click or F2 renames; right-click opens the entity menu.
//! - The wheel or a middle-drag scrolls; a selection made elsewhere (the
//!   viewport, the timeline) unfolds and scrolls into view.

use std::collections::HashSet;

use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::name::Name;
use bevy_ecs::prelude::*;
use bevy_ecs::reflect::{AppTypeRegistry, ReflectComponent};
use bevy_ecs::resource::IsResource;
use egui::{Pos2, Rect, Sense};
use tt_core::ComponentMetas;
use tt_core::app::ModuleList;
use tt_core::commands::{RenameRequest, rename, strokes_of};
use tt_core::meta::Class;
use tt_core::op::{OpError, Operator};
use tt_core::selection::Selection;
use tt_core::sketch::{Capture, sketch_of};
use tt_core::view::{home_of, sketch_framed};

use super::menu;
use crate::style;

/// A human label: the Name component, else the operator kind, else the id.
pub fn label(world: &World, e: Entity) -> String {
    if let Some(n) = world.get::<Name>(e) {
        return n.as_str().to_string();
    }
    if let Some(op) = world.get::<Operator>(e) {
        return format!("{} ({e})", op.kind);
    }
    format!("Entity {e}")
}

/// The outliner's own state (session; not part of the document).
#[derive(Resource, Default)]
pub struct OutlinerState {
    filter: String,
    /// Sketches folded shut.
    folded: HashSet<Entity>,
    /// Sketches whose strokes are shown.
    strokes_open: HashSet<Entity>,
    /// The row being renamed, its text, and whether it has had focus yet.
    renaming: Option<(Entity, String, bool)>,
    /// Where the last plain or Ctrl click was (Shift+click selects from here).
    anchor: Option<Entity>,
    /// A box being dragged: its start (screen).
    marquee: Option<Pos2>,
    scroll: f32,
    last_primary: Option<Entity>,
    /// Where the list and its first row were drawn (the demo aims at them).
    pub list_area: Option<Rect>,
    pub first_row: Option<Rect>,
}

/// Sketches in tree order: `(sketch, depth)`, nested sketches under the one
/// whose view they were drawn in. (The timeline's lanes use the same order.)
pub fn sketch_tree(world: &mut World) -> Vec<(Entity, usize)> {
    let mut q = world.query_filtered::<(Entity, &Operator), Without<Disabled>>();
    let mut sketches: Vec<Entity> = q.iter(world).filter(|(_, o)| o.kind == "sketch").map(|(e, _)| e).collect();
    sketches.sort_by_key(|e| e.index_u32()); // creation order (ids are never reused within a document)
    let parent = |w: &World, s: Entity| home_of(w, s).and_then(|v| sketch_framed(w, v)).filter(|p| sketches.contains(p));
    let parents: Vec<Option<Entity>> = sketches.iter().map(|s| parent(world, *s)).collect();
    let mut out = Vec::new();
    fn walk(s: Entity, depth: usize, sketches: &[Entity], parents: &[Option<Entity>], out: &mut Vec<(Entity, usize)>) {
        if out.iter().any(|(e, _)| *e == s) || depth > 64 {
            return;
        }
        out.push((s, depth));
        for (i, c) in sketches.iter().enumerate() {
            if parents[i] == Some(s) {
                walk(*c, depth + 1, sketches, parents, out);
            }
        }
    }
    for (i, s) in sketches.iter().enumerate() {
        if parents[i].is_none() {
            walk(*s, 0, &sketches, &parents, &mut out);
        }
    }
    out
}

#[derive(Clone, Copy, PartialEq)]
enum Row {
    /// An entity at a depth; `fold`: Some(open) if it can fold.
    Entity { e: Entity, depth: usize, fold: Option<bool> },
    /// "n strokes" under a sketch.
    Strokes { sketch: Entity, n: usize, depth: usize, open: bool },
}

fn rows(world: &mut World, st: &OutlinerState) -> Vec<Row> {
    let tree = sketch_tree(world);
    let filter = st.filter.trim().to_lowercase();
    let matches = |w: &World, e: Entity| filter.is_empty() || label(w, e).to_lowercase().contains(&filter);
    // With a filter: a row shows if it matches or anything under it does (all unfolded).
    let subtree = |i: usize| {
        let (_, d) = tree[i];
        let end = tree[i + 1..].iter().position(|(_, dd)| *dd <= d).map_or(tree.len(), |k| i + 1 + k);
        &tree[i..end]
    };
    let mut out = Vec::new();
    let mut hidden_below: Option<usize> = None;
    for (i, &(s, depth)) in tree.iter().enumerate() {
        if let Some(d) = hidden_below {
            if depth > d {
                continue;
            }
            hidden_below = None;
        }
        let strokes = strokes_of(world, s);
        if !filter.is_empty() {
            let any = subtree(i).iter().any(|(e, _)| matches(world, *e) || strokes_of(world, *e).iter().any(|c| matches(world, *c)));
            if !any {
                hidden_below = Some(depth);
                continue;
            }
        }
        let open = !filter.is_empty() || !st.folded.contains(&s);
        let has_children = !strokes.is_empty() || tree.get(i + 1).is_some_and(|(_, d)| *d > depth);
        out.push(Row::Entity { e: s, depth, fold: has_children.then_some(open) });
        if !open {
            hidden_below = Some(depth);
            continue;
        }
        if !strokes.is_empty() {
            let shown: Vec<Entity> = strokes.iter().copied().filter(|c| filter.is_empty() || matches(world, *c) || matches(world, s)).collect();
            if !shown.is_empty() {
                let strokes_open = !filter.is_empty() || st.strokes_open.contains(&s);
                out.push(Row::Strokes { sketch: s, n: strokes.len(), depth: depth + 1, open: strokes_open });
                if strokes_open {
                    out.extend(shown.into_iter().map(|c| Row::Entity { e: c, depth: depth + 2, fold: None }));
                }
            }
        }
        // (Nested sketches follow in tree order at depth + 1.)
    }
    // Everything else in the document.
    for e in other_entities(world) {
        if matches(world, e) {
            out.push(Row::Entity { e, depth: 0, fold: None });
        }
    }
    out
}

pub fn ui(ui: &mut egui::Ui, world: &mut World) {
    let mut st = std::mem::take(&mut *world.resource_mut::<OutlinerState>());

    // Rename requested by F2 or the menu.
    if let Some(e) = world.resource_mut::<RenameRequest>().0.take() {
        st.renaming = Some((e, label(world, e), false));
    }
    // A selection made elsewhere: unfold to it and scroll it into view.
    let primary = world.resource::<Selection>().primary();
    let mut scroll_to = None;
    if primary != st.last_primary {
        if let Some(p) = primary {
            if world.get::<Capture>(p).is_some()
                && let Some(s) = sketch_of(world, p)
            {
                st.strokes_open.insert(s);
                st.folded.remove(&s);
            }
            scroll_to = Some(p);
        }
        st.last_primary = primary;
    }

    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut st.filter).hint_text("🔍 Filter by name").desired_width(ui.available_width() - 28.0));
        if ui.add_enabled(!st.filter.is_empty(), egui::Button::new("✖").small()).on_hover_text("Clear the filter").clicked() {
            st.filter.clear();
        }
    });

    let rows = rows(world, &st);
    let area = ui.available_rect_before_wrap();
    // Under the rows: box selection and middle-drag scrolling (rows only take clicks).
    let bg = ui.interact(area, ui.id().with("outliner-bg"), Sense::click_and_drag());
    if bg.dragged_by(egui::PointerButton::Middle) {
        st.scroll = (st.scroll - bg.drag_delta().y).max(0.0);
    }
    let selection = world.resource::<Selection>().clone();
    let mods = ui.input(|i| i.modifiers);
    let marquee_rect = st.marquee.zip(ui.input(|i| i.pointer.interact_pos())).map(|(a, b)| Rect::from_two_pos(a, b));
    let mut row_rects: Vec<(Entity, Rect)> = Vec::new();
    let mut click: Option<Entity> = None;
    let mut toggle_fold: Option<Entity> = None;
    let mut toggle_strokes: Option<Entity> = None;
    let mut menu_for: Option<(Entity, egui::Response)> = None;
    let mut commit_rename: Option<(Entity, String)> = None;

    let mut scroll = egui::ScrollArea::vertical()
        .id_salt("outliner-scroll")
        .auto_shrink([false; 2])
        .scroll_source(egui::containers::scroll_area::ScrollSource::SCROLL_BAR | egui::containers::scroll_area::ScrollSource::MOUSE_WHEEL);
    if bg.dragged_by(egui::PointerButton::Middle) {
        scroll = scroll.vertical_scroll_offset(st.scroll);
    }
    let out = scroll.show(ui, |ui| {
        if rows.is_empty() {
            let text = if st.filter.is_empty() { "Nothing yet · press D, then hold on the video to sketch." } else { "Nothing matches the filter." };
            ui.label(egui::RichText::new(text).weak());
        }
        for row in &rows {
            match *row {
                Row::Entity { e, depth, fold } => {
                    let r = ui.horizontal(|ui| {
                        ui.add_space(depth as f32 * 14.0);
                        match fold {
                            Some(open) => {
                                if ui.add(egui::Button::new(if open { "⏷" } else { "⏵" }).frame(false).small()).clicked() {
                                    toggle_fold = Some(e);
                                }
                            }
                            None => ui.add_space(18.0),
                        }
                        if let Some((re, text, focused)) = st.renaming.as_mut().filter(|(re, _, _)| *re == e) {
                            let te = ui.add(egui::TextEdit::singleline(text).desired_width(150.0));
                            if !*focused {
                                te.request_focus();
                                *focused = true;
                            } else if te.lost_focus() {
                                if !ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                                    commit_rename = Some((*re, text.clone()));
                                }
                                commit_rename.get_or_insert((Entity::PLACEHOLDER, String::new()));
                            }
                            return None;
                        }
                        let error = world.get::<OpError>(e).map(|err| err.0.clone());
                        let is_stroke = world.get::<Capture>(e).is_some();
                        let mut rt = egui::RichText::new(label(world, e));
                        if is_stroke {
                            rt = rt.color(style::MUTED);
                        }
                        if error.is_some() {
                            rt = rt.color(egui::Color32::from_rgb(0xf4, 0x3f, 0x5e));
                        }
                        let previewed = marquee_rect.is_some_and(|m| row_rect_hit(ui, m));
                        let r = ui.selectable_label(selection.is_selected(e) || previewed, rt);
                        let r = match error {
                            Some(err) => r.on_hover_text(err),
                            None => r,
                        };
                        Some(r)
                    });
                    let full = Rect::from_x_y_ranges(area.x_range(), r.response.rect.y_range());
                    row_rects.push((e, full));
                    if let Some(r) = r.inner {
                        if scroll_to == Some(e) {
                            r.scroll_to_me(Some(egui::Align::Center));
                        }
                        if r.double_clicked() {
                            st.renaming = Some((e, label(world, e), false));
                        } else if r.clicked() {
                            click = Some(e);
                        }
                        if r.secondary_clicked() || r.context_menu_opened() {
                            menu_for = Some((e, r));
                        }
                    }
                }
                Row::Strokes { sketch, n, depth, open } => {
                    ui.horizontal(|ui| {
                        ui.add_space(depth as f32 * 14.0);
                        let text = format!("{} {n} stroke{}", if open { "⏷" } else { "⏵" }, if n == 1 { "" } else { "s" });
                        if ui.add(egui::Button::new(egui::RichText::new(text).color(style::MUTED)).frame(false).small()).clicked() {
                            toggle_strokes = Some(sketch);
                        }
                    });
                }
            }
        }

        ui.add_space(12.0);
        egui::CollapsingHeader::new("Dev: modules & types").default_open(false).show(ui, |ui| {
            for name in &world.resource::<ModuleList>().0 {
                ui.monospace(short(name));
            }
            ui.separator();
            let mut metas: Vec<_> = world.resource::<ComponentMetas>().iter().cloned().collect();
            metas.sort_by_key(|m| m.name);
            for m in metas {
                ui.horizontal(|ui| {
                    ui.monospace(short(m.name));
                    ui.label(egui::RichText::new(format!("{:?}", m.class)).weak());
                });
            }
        });
    });
    st.scroll = out.state.offset.y;
    st.list_area = Some(area);
    st.first_row = row_rects.first().map(|(_, r)| *r);

    // Box selection.
    if bg.drag_started_by(egui::PointerButton::Primary) {
        st.marquee = ui.input(|i| i.pointer.press_origin());
    }
    if let Some(m) = marquee_rect {
        ui.painter().rect(m, 2.0, style::ACCENT.gamma_multiply(0.12), egui::Stroke::new(1.0, style::ACCENT), egui::StrokeKind::Inside);
        if bg.drag_stopped() || !ui.input(|i| i.pointer.primary_down()) {
            let hit: Vec<Entity> = row_rects.iter().filter(|(_, r)| r.intersects(m)).map(|(e, _)| *e).collect();
            let mut sel = world.resource_mut::<Selection>();
            if !(mods.shift || mods.ctrl || mods.command) {
                sel.clear();
            }
            for e in hit {
                if !sel.is_selected(e) {
                    sel.entities.push(e);
                }
            }
            st.marquee = None;
        }
    } else if bg.clicked() && !(mods.shift || mods.ctrl || mods.command) {
        world.resource_mut::<Selection>().clear();
    }

    // Clicks on rows.
    if let Some(e) = click {
        let visible: Vec<Entity> = row_rects.iter().map(|(e, _)| *e).collect();
        let mut sel = world.resource_mut::<Selection>();
        if mods.shift
            && let Some(a) = st.anchor
            && let (Some(i), Some(j)) = (visible.iter().position(|x| *x == a), visible.iter().position(|x| *x == e))
        {
            let range = &visible[i.min(j)..=i.max(j)];
            if !(mods.ctrl || mods.command) {
                sel.clear();
            }
            for x in range {
                if !sel.is_selected(*x) {
                    sel.entities.push(*x);
                }
            }
            // Keep the clicked row primary.
            sel.entities.retain(|x| *x != e);
            sel.entities.push(e);
        } else if mods.ctrl || mods.command {
            sel.toggle(e);
            st.anchor = Some(e);
        } else {
            sel.select_only(e);
            st.anchor = Some(e);
        }
        st.last_primary = world.resource::<Selection>().primary();
    }
    if let Some(s) = toggle_fold
        && !st.folded.remove(&s)
    {
        st.folded.insert(s);
    }
    if let Some(s) = toggle_strokes
        && !st.strokes_open.remove(&s)
    {
        st.strokes_open.insert(s);
    }
    if let Some((e, text)) = commit_rename {
        if e != Entity::PLACEHOLDER {
            rename(world, e, &text);
        }
        st.renaming = None;
    }
    if let Some((e, r)) = menu_for {
        if r.secondary_clicked() {
            menu::right_clicked(world, e);
        }
        r.context_menu(|ui| menu::entity_menu(ui, world));
    }
    *world.resource_mut::<OutlinerState>() = st;
}

/// Whether the row being laid out in `ui` lies under the box `m`.
fn row_rect_hit(ui: &egui::Ui, m: Rect) -> bool {
    let y = ui.min_rect().y_range();
    m.y_range().intersects(y)
}

/// Enabled document entities that aren't sketches, strokes or views.
fn other_entities(world: &mut World) -> Vec<Entity> {
    let doc_types: Vec<ReflectComponent> = {
        let registry = world.resource::<AppTypeRegistry>().read();
        let metas = world.resource::<ComponentMetas>();
        registry
            .iter()
            .filter(|r| metas.get(r.type_id()).is_some_and(|m| m.class == Class::Document))
            .filter_map(|r| r.data::<ReflectComponent>().cloned())
            .collect()
    };
    let mut q = world.query_filtered::<EntityRef, (Without<IsResource>, Without<Disabled>)>();
    let mut out: Vec<Entity> = q
        .iter(world)
        .filter(|e| doc_types.iter().any(|rc| rc.contains(*e)))
        .filter(|e| !e.contains::<Capture>() && !e.get::<Operator>().is_some_and(|o| o.kind == "sketch" || o.kind == "frame"))
        .map(|e| e.id())
        .collect();
    out.sort_by_key(|e| e.index_u32());
    out
}

/// `tt_core::transport::TransportModule` → `TransportModule`.
fn short(path: &str) -> &str {
    path.rsplit("::").next().unwrap_or(path)
}
