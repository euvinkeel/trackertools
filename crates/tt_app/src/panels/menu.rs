//! The right-click menu for entities, shared by the outliner, the timeline and
//! the viewport. It acts on the selection (a right-click on something
//! unselected selects it first: [`right_clicked`]). Commands go through
//! actions, so they are the same as their keys and undo the same way.

use bevy_ecs::prelude::*;
use tt_core::capture::LiveCapture;
use tt_core::commands::strokes_of;
use tt_core::input::{Action, Keymap, PendingActions};
use tt_core::selection::Selection;
use tt_core::sketch::{Capture, is_sketch, sketch_of};

/// Call on a secondary click of `e`: unless it is already selected, it becomes the selection.
pub fn right_clicked(world: &mut World, e: Entity) {
    if !world.resource::<Selection>().is_selected(e) {
        world.resource_mut::<Selection>().select_only(e);
    }
}

/// The menu's contents (inside `Response::context_menu`).
pub fn entity_menu(ui: &mut egui::Ui, world: &mut World) {
    let selection = world.resource::<Selection>().entities.clone();
    let primary = selection.last().copied();
    let keymap = world.resource::<Keymap>().clone();
    let chord = |a: Action| keymap.chord_for(a).map(|c| format!("  ({c})")).unwrap_or_default();
    let busy = world.resource::<LiveCapture>().0.is_some();
    let sketch = primary.and_then(|e| sketch_of(world, e));
    let any_sketch = selection.iter().any(|e| sketch_of(world, *e).is_some());
    let mut push: Option<Action> = None;

    if selection.is_empty() {
        ui.label(egui::RichText::new("Nothing selected").weak());
    } else {
        let what = match selection.len() {
            1 => crate::panels::outliner::label(world, selection[0]),
            n => format!("{n} selected"),
        };
        ui.label(egui::RichText::new(what).strong());
        ui.separator();
        if let Some(s) = sketch
            && ui.button(format!("Enter view{}", chord(Action::EnterView))).on_hover_text("See this sketch's view; sketch inside it for detail").clicked()
        {
            world.resource_mut::<Selection>().select_only(s);
            push = Some(Action::EnterView);
            ui.close();
        }
        if ui.add_enabled(primary.is_some(), egui::Button::new(format!("Rename…{}", chord(Action::Rename)))).clicked() {
            push = Some(Action::Rename);
            ui.close();
        }
        if ui.add_enabled(any_sketch && !busy, egui::Button::new(format!("Duplicate{}", chord(Action::Duplicate)))).on_hover_text("Copy the sketch with all its strokes").clicked() {
            push = Some(Action::Duplicate);
            ui.close();
        }
        if ui.add_enabled(!busy, egui::Button::new(format!("Delete{}", chord(Action::Delete)))).on_hover_text("A sketch goes with its strokes and view; a stroke leaves its sketch").clicked() {
            push = Some(Action::Delete);
            ui.close();
        }
        ui.separator();
        if let Some(p) = primary {
            if is_sketch(world, p) {
                let strokes = strokes_of(world, p);
                if ui.add_enabled(!strokes.is_empty(), egui::Button::new(format!("Select its {} strokes", strokes.len()))).clicked() {
                    world.resource_mut::<Selection>().entities = strokes;
                    ui.close();
                }
            } else if world.get::<Capture>(p).is_some()
                && let Some(s) = sketch
                && ui.button("Select its sketch").clicked()
            {
                world.resource_mut::<Selection>().select_only(s);
                ui.close();
            }
        }
    }
    // Where the selected thing is on the timeline, and its lifetime.
    if let Some((a, b)) = primary.and_then(|p| tt_core::commands::time_span(world, p)) {
        ui.separator();
        if ui.button(format!("Go to its start (frame {a})")).clicked() {
            push = Some(Action::Seek(a));
            ui.close();
        }
        if ui.button(format!("Go to its end (frame {b})")).clicked() {
            push = Some(Action::Seek(b));
            ui.close();
        }
    }
    if let Some(p) = primary.filter(|p| tt_core::span::extent_of(world, *p).is_some() && world.get::<Capture>(*p).is_none()) {
        use tt_core::span::{Edge, move_edge, set_span, span_of};
        let here = world.resource::<tt_core::transport::Transport>().frame();
        let tip = "Its lifetime: nothing outside it is shown, used or tracked, and nothing is deleted (drag its ends on the timeline)";
        if ui.button(format!("Starts here (frame {here})")).on_hover_text(tip).clicked() {
            move_edge(world, p, Edge::First, here);
            ui.close();
        }
        if ui.button(format!("Ends here (frame {here})")).on_hover_text(tip).clicked() {
            move_edge(world, p, Edge::Last, here);
            ui.close();
        }
        if span_of(world, p).is_trimmed() && ui.button("Untrim (its whole length)").clicked() {
            set_span(world, p, tt_core::span::Span::default());
            ui.close();
        }
        ui.separator();
    } else if primary.is_some() {
        ui.separator();
    }
    // Trackers: which way they track (nothing runs until asked), or pause.
    let trackers: Vec<Entity> = selection.iter().copied().filter(|e| tt_track::is_tracker(world, *e)).collect();
    if let Some(first) = trackers.first() {
        let now = tt_track::run_of(world, *first);
        for (run, _, item, tip) in super::tracks::RUNS {
            if ui.add_enabled(now != run || trackers.len() > 1, egui::Button::new(item)).on_hover_text(tip).clicked() {
                super::tracks::ask(world, &trackers, run);
                ui.close();
            }
        }
        ui.separator();
    }
    // Subjects (tt_core::subject): one position and angle that several points carry together.
    let subjects: Vec<Entity> = selection.iter().copied().filter(|e| tt_core::subject::is_subject(world, *e)).collect();
    // The points: trackers, and sketches (a sketch's point is the hand's path).
    let mut points: Vec<Entity> = Vec::new();
    for e in selection.iter().copied().filter(|e| !subjects.contains(e)) {
        let p = if tt_track::is_tracker(world, e) { Some(e) } else { sketch_of(world, e) };
        if let Some(p) = p.filter(|p| !points.contains(p)) {
            points.push(p);
        }
    }
    let here = world.resource::<tt_core::transport::Transport>().frame();
    let name_of = |world: &World, e: Entity| world.get::<Name>(e).map_or_else(|| "it".to_string(), |n| n.to_string());
    if !points.is_empty() && subjects.is_empty() {
        let tip = "A subject: one position (and angle) that these trackers and sketches carry together. \
                   It starts in the middle of them on this frame, then moves with their motion, \
                   so one of them coming, going or getting lost never makes it jump. \
                   Drag it on the video (Select tool) to put it where you want it on any frame: that keys its own offset.";
        let them = if points.len() == 1 { "it".to_string() } else { format!("these {}", points.len()) };
        if ui.button(format!("Make a subject of {them}")).on_hover_text(tip).clicked() {
            tt_core::subject::make_subject(world, &points, here);
            ui.close();
        }
        ui.separator();
    }
    if let [subject] = subjects[..] {
        let name = name_of(world, subject);
        let members = tt_core::subject::members_of(world, subject);
        let (inside, outside): (Vec<Entity>, Vec<Entity>) = points.iter().partition(|p| members.contains(p));
        if !outside.is_empty() && ui.button(format!("Add the {} selected to {name}", outside.len())).clicked() {
            tt_core::subject::add_members(world, subject, &outside);
            ui.close();
        }
        if !inside.is_empty() && ui.button(format!("Take the {} selected out of {name}", inside.len())).clicked() {
            tt_core::subject::remove_members(world, subject, &inside);
            ui.close();
        }
        if points.is_empty() {
            if ui.add_enabled(!members.is_empty(), egui::Button::new(format!("Select its {} member{}", members.len(), if members.len() == 1 { "" } else { "s" }))).clicked() {
                world.resource_mut::<Selection>().entities = members.clone();
                ui.close();
            }
            let offsets = world.get::<tt_core::subject::Subject>(subject).map(|s| s.offsets.clone()).unwrap_or_default();
            if offsets.iter().any(|k| k.frame == here) {
                if ui.button(format!("Remove its key on frame {here}")).clicked() {
                    tt_core::subject::remove_offset_key(world, subject, here);
                    ui.close();
                }
            } else if ui
                .button(format!("Key it on frame {here}, as it is"))
                .on_hover_text("Pins where it is on this frame, so keys made elsewhere don't move it here (a drag keys it too)")
                .clicked()
            {
                let [x, y, a] = tt_core::subject::offset_at(&offsets, here);
                tt_core::subject::set_offset_key(world, subject, tt_core::subject::OffsetKey { frame: here, x: x as f32, y: y as f32, angle: a.to_degrees() as f32 });
                ui.close();
            }
        }
        ui.separator();
    }
    // The exports: a selected subject's final data, else the points.
    let source = subjects.first().copied();
    if source.is_some() || !points.is_empty() {
        let (label, follow_label, tip) = match source {
            Some(s) => {
                let name = name_of(world, s);
                (
                    format!("Copy Resolve stabilizer (Fusion): {name}"),
                    format!("Copy Resolve follower (Fusion): text on {name}"),
                    format!(
                        "A Fusion Transform that holds {name} still as it is on this frame: its final position and angle \
                         (its members' motion plus its own keys). \
                         In Resolve's Fusion page: select MediaIn1, press Ctrl+V (Cmd+V on a Mac), and make sure the Transform sits between MediaIn1 and MediaOut1. \
                         It lines itself up with a trimmed clip and with a timeline of another frame rate. \
                         Inside a Fusion Clip, set its \"Clip Starts At Source Frame\" to the frame (as numbered here) the Fusion Clip starts on."
                    ),
                )
            }
            None => (
                match points.len() {
                    1 => "Copy Resolve stabilizer (Fusion), position only".to_string(),
                    n => format!("Copy Resolve stabilizer (Fusion), {n} points"),
                },
                "Copy Resolve follower (Fusion): text that follows".to_string(),
                "The selected trackers and sketches (a sketch's point is your hand's path) as a Fusion Transform that holds their points still \
                 as they are on this frame: in position, and with two or more in rotation too \
                 (the best fit to all of them: more, farther apart, turn it more precisely; a subject keeps them steadier still). \
                 In Resolve's Fusion page: select MediaIn1, press Ctrl+V (Cmd+V on a Mac), and make sure the Transform sits between MediaIn1 and MediaOut1. \
                 It lines itself up with a trimmed clip and with a timeline of another frame rate. \
                 Inside a Fusion Clip, set its \"Clip Starts At Source Frame\" to the frame (as numbered here) the Fusion Clip starts on."
                    .to_string(),
            ),
        };
        if ui.button(label).on_hover_text(tip).clicked() {
            let status = copy_stabilizer(ui.ctx(), world, &points, source, false);
            world.resource_mut::<crate::media::StatusLine>().0 = Some(status);
            ui.close();
        }
        let follow_tip = "The other way round: the footage stays as it is, and something moves with it (and turns with it): \
                          a Fusion Merge whose foreground, a Text+ to start with, goes where it goes. \
                          Paste it the same way (Fusion page, select MediaIn1, Ctrl+V); edit the Text+, or plug anything into the Merge's foreground (its green input) instead. \
                          To put the text beside the point rather than on it, move the Text+'s own Center.";
        if ui.button(follow_label).on_hover_text(follow_tip).clicked() {
            let status = copy_stabilizer(ui.ctx(), world, &points, source, true);
            world.resource_mut::<crate::media::StatusLine>().0 = Some(status);
            ui.close();
        }
        // Or rendered here, for any editor: nothing to keep working there.
        let from = || match source {
            Some(s) => crate::panels::export::Source::Subject(s),
            None => crate::panels::export::Source::Points(points.clone()),
        };
        if ui
            .button("Export stabilized video\u{2026}")
            .on_hover_text("Renders a stabilized copy of the video here, at its size and frame rate, with its sound: a normal clip for any editor (no Fusion node to keep working).")
            .clicked()
        {
            crate::panels::export::open(world, crate::panels::export::Kind::Stabilized, from());
            ui.close();
        }
        if ui
            .button("Export tracking target video\u{2026}")
            .on_hover_text("Renders a video of a marker that moves and turns with it, for your editor's own tracker to follow (then attach text to that track).")
            .clicked()
        {
            crate::panels::export::open(world, crate::panels::export::Kind::Target, from());
            ui.close();
        }
        // The spring on what the trackers measured: it lets faster motion through, jitter included.
        let mut d = *world.resource::<tt_track::export::StabilizerDefaults>();
        let spring = "Seconds of spring smoothing on the motion the trackers measured (no lag): the stabilizer lets motion faster than this through. \
                      It takes out the trackers' jitter, which shows most in rotation when they are close together, \
                      and with it the correction of shake that fast. 0 = hold them exactly.";
        ui.horizontal(|ui| {
            ui.label("Smooth rotation");
            ui.add(egui::DragValue::new(&mut d.smooth_rotation).range(0.0..=2.0).speed(0.005).max_decimals(2).suffix(" s")).on_hover_text(spring);
        });
        ui.horizontal(|ui| {
            ui.label("Smooth position");
            ui.add(egui::DragValue::new(&mut d.smooth_position).range(0.0..=2.0).speed(0.005).max_decimals(2).suffix(" s")).on_hover_text(spring);
        });
        if d != *world.resource::<tt_track::export::StabilizerDefaults>() {
            *world.resource_mut::<tt_track::export::StabilizerDefaults>() = d;
        }
        ui.separator();
    }
    if ui.button(format!("Select all{}", chord(Action::SelectAll))).clicked() {
        push = Some(Action::SelectAll);
        ui.close();
    }
    if ui.add_enabled(!selection.is_empty(), egui::Button::new(format!("Deselect all{}", chord(Action::DeselectAll)))).clicked() {
        push = Some(Action::DeselectAll);
        ui.close();
    }
    if let Some(a) = push {
        world.resource_mut::<PendingActions>().push(a);
    }
}

/// Copies the Fusion stabilizer for `points` (trackers and sketches;
/// tt_track::export), held as they are on the current frame, or with
/// `follower` the follower (a Merge whose Text+ moves with them), and saves it
/// as a `.setting` in the data folder too. Returns the status line's message
/// and whether it's an error.
fn copy_stabilizer(ctx: &egui::Context, world: &World, points: &[Entity], subject: Option<Entity>, follower: bool) -> (String, bool) {
    use tt_track::export::{StabilizerDefaults, follow, follow_path, fusion_follow_setting, fusion_setting, good_points, stabilize, stabilize_path, subject_path};
    let size = world.resource::<tt_core::view::SourceSize>();
    let transport = world.resource::<tt_core::transport::Transport>();
    let (here, fps) = (transport.frame(), transport.fps.as_f64());
    let smoothing = (*world.resource::<StabilizerDefaults>()).into();
    let wh = [size.width, size.height];
    let made = match subject {
        Some(s) => {
            let path = subject_path(world, s);
            if follower { follow_path(&path, wh, here, smoothing, fps) } else { stabilize_path(&path, wh, here, smoothing, fps) }
        }
        None => {
            let tracks: Vec<_> = points.iter().map(|e| good_points(world, *e)).collect();
            if follower { follow(&tracks, wh, here, smoothing, fps) } else { stabilize(&tracks, wh, here, smoothing, fps) }
        }
    };
    let Some(st) = made else {
        let why = match (subject, points.len()) {
            (Some(_), _) => "The subject has no frames yet: its members have no points",
            (None, 1) => "It has no point on any frame",
            _ => "They have no frame where two of them have a point",
        };
        return (why.into(), true);
    };
    let points: Vec<Entity> = subject.map_or_else(|| points.to_vec(), |s| vec![s]);
    let points = &points[..];
    let text = if follower { fusion_follow_setting("Follow", &st.keys, fps) } else { fusion_setting("Stabilize", &st.keys, fps) };
    ctx.copy_text(text.clone());
    let names: Vec<String> = points.iter().map(|e| world.get::<Name>(*e).map_or_else(|| "point".to_string(), |n| n.to_string())).collect();
    let what = match names.len() {
        1..=3 => names.join(" + "),
        n => format!("{} + {} more", names[..2].join(" + "), n - 2),
    };
    let dir = tt_media::proxy::data_dir().join("exports");
    let file = dir.join(format!("{} {what}.setting", if follower { "follow" } else { "stabilize" }).replace(['/', '\\', ':'], "_"));
    let saved = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&file, &text)).map_or_else(|e| format!(" (not saved: {e})"), |()| format!("; saved {}", file.display()));
    let (lo, hi) = (st.keys.first().map_or(0, |k| k.frame), st.keys.last().map_or(0, |k| k.frame));
    let rotation = if subject.is_some() {
        "; its final position and angle".to_string()
    } else if st.used < 2 {
        "; position only (a second point adds rotation)".to_string()
    } else if follower {
        format!("; it turns with them too ({:.0} px spread)", st.spread)
    } else {
        // What the rotation's jitter does at the picture's corners, before the spring.
        let corner = st.jitter.to_radians() * size.width.hypot(size.height) / 2.0;
        let advice = if corner > 1.0 { format!(" — it turns the corners by about {corner:.0} px a frame: spread the points farther apart, or smooth rotation more") } else { String::new() };
        format!("; {:.0} px spread, rotation jitter ±{:.2}° a frame before smoothing{advice}", st.spread, st.jitter)
    };
    (
        if follower {
            format!(
                "Copied a Fusion follower from {} point{}: {} keys, frames {lo}–{hi}; pasted on the clip, its Text+ goes where they go{rotation}{saved}",
                st.used,
                if st.used == 1 { "" } else { "s" },
                st.keys.len()
            )
        } else {
            format!(
                "Copied a Fusion stabilizer from {} point{}: {} keys, frames {lo}–{hi}, held as on frame {}{rotation}{saved}",
                st.used,
                if st.used == 1 { "" } else { "s" },
                st.keys.len(),
                st.reference
            )
        },
        false,
    )
}
