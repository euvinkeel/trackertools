//! Outliner: the document as one tree (`crate::tree`): everything under
//! what it was made relative to (a sketch drawn inside a tracker's view under
//! that tracker, a tracker under its sketch, and so on), with a sketch's
//! strokes (folded under "n strokes"), a tracker's looks and a subject's
//! members; other document entities follow. Type in the box to filter.
//!
//! - Click selects, Ctrl+click toggles, Shift+click selects the range from
//!   the last click.
//! - Drag on the list draws a box that selects every row it touches (Shift or
//!   Ctrl add to the selection); past the top or bottom it scrolls the list,
//!   Esc drops it. A click beside a label is on its row; a click on empty
//!   space deselects.
//! - Double-click or F2 renames; right-click opens the entity menu.
//! - The wheel or a middle-drag scrolls; a selection made elsewhere (the
//!   viewport, the timeline) unfolds and scrolls into view.

use std::collections::HashSet;

use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::name::Name;
use bevy_ecs::prelude::*;
use egui::{Pos2, Rect, Sense};
use tt_core::ComponentMetas;
use tt_core::app::ModuleList;
use tt_core::commands::{RenameRequest, rename, strokes_of};
use tt_core::meta::{Created, creation_order};
use tt_core::op::{OpError, Operator};
use tt_core::selection::Selection;
use tt_core::sketch::{Capture, sketch_of};
use crate::tree::Node;

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
    /// A box being dragged: its start, in the list's content coordinates
    /// (so it stays on its row while the list scrolls).
    marquee: Option<Pos2>,
    scroll: f32,
    /// The primary selection as the outliner last left it; a different one
    /// was made elsewhere.
    last_primary: Option<Entity>,
    /// A row to scroll into view once it is drawn.
    scroll_to: Option<Entity>,
    /// Where the list and its first row were drawn (the demo aims at them).
    pub list_area: Option<Rect>,
    pub first_row: Option<Rect>,
}

/// Sketches in tree order: `(sketch, depth)` (the demo counts them).
pub fn sketch_tree(world: &mut World) -> Vec<(Entity, usize)> {
    crate::tree::tree(world).into_iter().filter(|(_, _, k)| *k == Node::Sketch).map(|(e, d, _)| (e, d)).collect()
}

/// Unfold what hides `e`'s row: a stroke's list of strokes, a look's
/// tracker, and everything it is nested under (`crate::tree`).
fn reveal(world: &mut World, st: &mut OutlinerState, e: Entity) {
    let mut e = e;
    // A look: its tracker.
    if world.get::<tt_track::look::Look>(e).is_some() {
        let mut q = world.query::<(Entity, &tt_core::op::Inputs)>();
        if let Some(t) = q.iter(world).find(|(_, i)| i.0.iter().any(|(s, p)| s == "look" && *p == e)).map(|(t, _)| t) {
            st.folded.remove(&t);
            e = t;
        }
    }
    // A stroke: its sketch, with its strokes shown.
    if world.get::<Capture>(e).is_some()
        && let Some(s) = sketch_of(world, e)
    {
        st.strokes_open.insert(s);
        st.folded.remove(&s);
        e = s;
    }
    for p in crate::tree::ancestors(world, e) {
        st.folded.remove(&p);
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Row {
    /// An entity at a depth; `fold`: Some(open) if it can fold.
    Entity { e: Entity, depth: usize, fold: Option<bool> },
    /// "n strokes" under a sketch.
    Strokes { sketch: Entity, n: usize, depth: usize, open: bool },
}

fn rows(world: &mut World, st: &OutlinerState) -> Vec<Row> {
    let tree = crate::tree::tree(world);
    let filter = st.filter.trim().to_lowercase();
    let matches = |w: &World, e: Entity| filter.is_empty() || label(w, e).to_lowercase().contains(&filter);
    // What hangs off a node besides its children in the tree: a sketch's
    // strokes, a tracker's looks, a subject's members (references: they are
    // in the tree too).
    let extras = |w: &mut World, e: Entity, kind: Node| -> Vec<Entity> {
        match kind {
            Node::Sketch => strokes_of(w, e),
            Node::Tracker => tt_track::look::looks_of(w, e),
            Node::Subject => tt_core::subject::members_of(w, e),
            // A SpringFocus: what it focuses on (references), in the order of its keys.
            Node::Focus => {
                let mut v: Vec<Entity> = Vec::new();
                for k in w.get::<tt_core::focus::FocusParams>(e).map(|p| p.keys.clone()).unwrap_or_default() {
                    if !v.contains(&k.target) && w.get_entity(k.target).is_ok() {
                        v.push(k.target);
                    }
                }
                v
            }
        }
    };
    // With a filter: a row shows if it matches, or anything under it does (all unfolded).
    let subtree_end = |i: usize| {
        let d = tree[i].1;
        tree[i + 1..].iter().position(|(_, dd, _)| *dd <= d).map_or(tree.len(), |k| i + 1 + k)
    };
    let mut out = Vec::new();
    let mut hidden_below: Option<usize> = None;
    for (i, &(e, depth, kind)) in tree.iter().enumerate() {
        if let Some(d) = hidden_below {
            if depth > d {
                continue;
            }
            hidden_below = None;
        }
        let own = extras(world, e, kind);
        if !filter.is_empty() {
            let mut any = false;
            for &(x, _, k) in &tree[i..subtree_end(i)] {
                if matches(world, x) || extras(world, x, k).iter().any(|c| matches(world, *c)) {
                    any = true;
                    break;
                }
            }
            if !any {
                hidden_below = Some(depth);
                continue;
            }
        }
        let open = !filter.is_empty() || !st.folded.contains(&e);
        let has_children = !own.is_empty() || tree.get(i + 1).is_some_and(|(_, d, _)| *d > depth);
        out.push(Row::Entity { e, depth, fold: has_children.then_some(open) });
        if !open {
            hidden_below = Some(depth);
            continue;
        }
        let shown: Vec<Entity> = own.iter().copied().filter(|c| filter.is_empty() || matches(world, *c) || matches(world, e)).collect();
        match kind {
            // Strokes fold under "n strokes".
            Node::Sketch if !shown.is_empty() => {
                let strokes_open = !filter.is_empty() || st.strokes_open.contains(&e);
                out.push(Row::Strokes { sketch: e, n: own.len(), depth: depth + 1, open: strokes_open });
                if strokes_open {
                    out.extend(shown.into_iter().map(|c| Row::Entity { e: c, depth: depth + 2, fold: None }));
                }
            }
            _ => out.extend(shown.into_iter().map(|c| Row::Entity { e: c, depth: depth + 1, fold: None })),
        }
        // (Its children in the tree follow at depth + 1.)
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

    // Rename requested by F2 or the menu (dropped below if its row still isn't drawn).
    if let Some(e) = world.resource_mut::<RenameRequest>().0.take() {
        reveal(world, &mut st, e);
        st.renaming = Some((e, label(world, e), false));
    }
    // A selection made elsewhere: unfold to it and scroll it into view.
    let primary = world.resource::<Selection>().primary();
    if primary != st.last_primary {
        st.scroll_to = primary;
        if let Some(p) = primary {
            reveal(world, &mut st, p);
        }
    }

    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut st.filter).hint_text("Filter by name").desired_width(ui.available_width() - 28.0));
        if !st.filter.is_empty() && crate::icons::cross_button(ui, "Clear the filter").clicked() {
            st.filter.clear();
        }
    });

    let rows = rows(world, &st);
    let area = ui.available_rect_before_wrap();
    // Under the rows: box selection, clicks beside a label, and middle-drag scrolling.
    let bg = ui.interact(area, ui.id().with("outliner-bg"), Sense::click_and_drag());
    let mut set_scroll = bg.dragged_by(egui::PointerButton::Middle);
    if set_scroll {
        st.scroll = (st.scroll - bg.drag_delta().y).max(0.0);
    }
    let selection = world.resource::<Selection>().clone();
    let colors = crate::colors::Colors::new(world);
    let mods = ui.input(|i| i.modifiers);
    let adding = mods.shift || mods.ctrl || mods.command;
    let pointer = ui.input(|i| i.pointer.interact_pos());
    // A box dragged past the top or bottom of the list scrolls it.
    if st.marquee.is_some()
        && let Some(p) = pointer
    {
        let over = (p.y - area.min.y).min(0.0) + (p.y - area.max.y).max(0.0);
        if over != 0.0 {
            st.scroll = (st.scroll + over.clamp(-60.0, 60.0) * 10.0 * ui.input(|i| i.stable_dt).min(0.1)).max(0.0);
            set_scroll = true;
            ui.ctx().request_repaint();
        }
    }
    let anchor = st.marquee;
    // The box this frame (screen): from its anchor to the pointer, kept inside the list.
    let mut marquee: Option<Rect> = None;
    let mut content_top = area.min.y;
    let mut row_rects: Vec<(Entity, Rect)> = Vec::new();
    let mut click: Option<Entity> = None;
    let mut toggle_fold: Option<Entity> = None;
    let mut toggle_strokes: Option<Entity> = None;
    let mut menu_for: Option<(Entity, egui::Response)> = None;
    let mut commit_rename: Option<(Entity, String)> = None;
    let mut scrolled_to = false;
    let requested_rename = st.renaming.as_ref().filter(|(_, _, focused)| !focused).map(|(e, _, _)| *e);

    let mut scroll = egui::ScrollArea::vertical()
        .id_salt("outliner-scroll")
        .auto_shrink([false; 2])
        .scroll_source(egui::containers::scroll_area::ScrollSource::SCROLL_BAR | egui::containers::scroll_area::ScrollSource::MOUSE_WHEEL);
    if set_scroll {
        scroll = scroll.vertical_scroll_offset(st.scroll);
    }
    let out = scroll.show(ui, |ui| {
        content_top = ui.max_rect().min.y;
        marquee = anchor.zip(pointer).map(|(a, b)| Rect::from_two_pos(Pos2::new(a.x, a.y + content_top), b.clamp(area.min, area.max)));
        if rows.is_empty() {
            let text = if st.filter.is_empty() { "Nothing yet · press D, then hold on the video to sketch." } else { "Nothing matches the filter." };
            ui.label(egui::RichText::new(text).weak());
        }
        for row in &rows {
            match *row {
                Row::Entity { e, depth, fold } => {
                    // Behind the row: the box's preview (known once the row is laid out).
                    let preview = ui.painter().add(egui::Shape::Noop);
                    let r = ui.horizontal(|ui| {
                        ui.add_space(depth as f32 * 14.0);
                        let h = ui.text_style_height(&egui::TextStyle::Body).max(12.0);
                        match fold {
                            Some(open) => {
                                if crate::icons::fold_button(ui, open).clicked() {
                                    toggle_fold = Some(e);
                                }
                            }
                            None => ui.add_space(h),
                        }
                        // What it is, in the visual language's colours.
                        let glyph = crate::icons::Glyph::of(world, e);
                        crate::icons::icon_in(ui, glyph, selection.is_selected(e), colors.of(world, e, glyph));
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
                        // While a plain box is dragged, it alone shows what will be selected.
                        let r = ui.selectable_label(selection.is_selected(e) && (anchor.is_none() || adding), rt);
                        let r = match error {
                            Some(err) => r.on_hover_text(err),
                            None => r,
                        };
                        Some(r)
                    });
                    // The row across the list, gaps included: what clicks and boxes hit.
                    let full = Rect::from_x_y_ranges(area.x_range(), r.response.rect.y_range()).expand2(egui::vec2(0.0, ui.spacing().item_spacing.y / 2.0));
                    if marquee.is_some_and(|m| full.intersects(m)) {
                        ui.painter().set(preview, egui::Shape::rect_filled(full.shrink2(egui::vec2(0.0, 1.0)), 2.0, ui.visuals().selection.bg_fill));
                    }
                    row_rects.push((e, full));
                    if let Some(r) = r.inner {
                        if st.scroll_to == Some(e) {
                            r.scroll_to_me(None);
                            scrolled_to = true;
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
                        let fold = crate::icons::fold_button(ui, open);
                        let text = format!("{n} stroke{}", if n == 1 { "" } else { "s" });
                        let words = ui.add(egui::Label::new(egui::RichText::new(text).color(style::MUTED)).sense(Sense::click()));
                        if fold.clicked() || words.clicked() {
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
    if scrolled_to {
        st.scroll_to = None;
    }
    st.list_area = Some(area);
    st.first_row = row_rects.first().map(|(_, r)| *r);
    // A requested rename whose row isn't drawn (filtered out) is dropped rather than grabbing the keyboard later.
    if st.renaming.as_ref().is_some_and(|(e, _, focused)| !focused && requested_rename == Some(*e)) {
        st.renaming = None;
    }

    // Box selection.
    if bg.drag_started_by(egui::PointerButton::Primary) {
        st.marquee = ui.input(|i| i.pointer.press_origin()).map(|p| Pos2::new(p.x, p.y - content_top));
    }
    if let Some(m) = marquee {
        ui.painter_at(area).rect(m, 2.0, style::ACCENT.gamma_multiply(0.12), egui::Stroke::new(1.0, style::ACCENT), egui::StrokeKind::Inside);
        let down = ui.input(|i| i.pointer.primary_down());
        if bg.drag_stopped() || !down {
            // Stopped with the button still down (Esc): no selection.
            if !down {
                let hit: Vec<Entity> = row_rects.iter().filter(|(_, r)| r.intersects(m)).map(|(e, _)| *e).collect();
                let mut sel = world.resource_mut::<Selection>();
                if !adding {
                    sel.clear();
                }
                for e in hit {
                    if !sel.is_selected(e) {
                        sel.entities.push(e);
                    }
                }
            }
            st.marquee = None;
        }
    } else if bg.clicked() {
        // Beside a row's label is still that row; empty space deselects.
        match bg.interact_pointer_pos().and_then(|p| row_rects.iter().find(|(_, r)| r.contains(p))) {
            Some((e, _)) => click = Some(*e),
            None if !adding => world.resource_mut::<Selection>().clear(),
            None => {}
        }
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
    // What the outliner selected itself doesn't scroll it.
    st.last_primary = world.resource::<Selection>().primary();
    *world.resource_mut::<OutlinerState>() = st;
}

/// Enabled document entities (every one carries [`Created`]) that aren't
/// sketches, strokes or views.
fn other_entities(world: &mut World) -> Vec<Entity> {
    let mut q = world.query_filtered::<(Entity, Option<&Operator>), (With<Created>, Without<Capture>, Without<tt_track::look::Look>, Without<Disabled>)>();
    let mut out: Vec<Entity> = q.iter(world).filter(|(_, o)| !o.is_some_and(|o| o.kind == "sketch" || o.kind == "frame" || o.kind == "track" || o.kind == "subject" || o.kind == "focus")).map(|(e, _)| e).collect();
    creation_order(world, &mut out);
    out
}

/// `tt_core::transport::TransportModule` → `TransportModule`.
fn short(path: &str) -> &str {
    path.rsplit("::").next().unwrap_or(path)
}
