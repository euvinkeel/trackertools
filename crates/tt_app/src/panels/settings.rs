//! Settings: how the tools behave (remembered between launches, in the
//! session file) and every key, so nothing has to be memorized.

use bevy_ecs::prelude::*;
use tt_core::autospeed::{AutoSpeed, AutoSpeedState, Foresight};
use tt_core::input::{Action, Keymap};
use tt_core::view::ViewDefaults;

use super::viewport::PointerView;
use crate::style;

pub fn ui(ui: &mut egui::Ui, world: &mut World) {
    egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
        updates(ui, world);
        build_from_code(ui, world);

        ui.separator();
        ui.heading("Doctor");
        ui.label("The doctor checks FFmpeg, the graphics and the folders. It also makes a report for the person who helps you.");
        if ui.button("Open the doctor").clicked() {
            let mut doctor = world.resource_mut::<crate::setup::Doctor>();
            doctor.open = true;
            doctor.recheck();
        }

        ui.separator();
        ui.heading("CoTracker");
        {
            let mut e = world.resource_mut::<crate::cotracker::EarlyStart>();
            ui.checkbox(&mut e.enabled, "Start CoTracker when a video opens").on_hover_text(
                "CoTracker loads its model in the background and keeps it loaded, so CoTracker trackers start at once. \
                 It uses some memory on the graphics card while trackertools is open. \
                 Off: CoTracker starts when a CoTracker tracker tracks, and stops 30 seconds after the last one.",
            );
        }
        let state = match tt_track::job::cotracker_engine() {
            tt_track::job::CoTrackerEngine::Off => "CoTracker is not running.".to_string(),
            tt_track::job::CoTrackerEngine::Starting => "CoTracker is starting.".to_string(),
            tt_track::job::CoTrackerEngine::Ready(device) => format!("CoTracker is ready ({}).", match device.as_str() {
                "cuda" => "on the NVIDIA graphics card",
                "mps" => "on the Apple graphics",
                "cpu" => "on the processor: slow",
                _ => "loaded",
            }),
            tt_track::job::CoTrackerEngine::Failed(why) => format!("CoTracker could not start: {why}"),
        };
        ui.label(egui::RichText::new(state).color(style::MUTED).small());

        ui.separator();
        ui.heading("Sketching");
        ui.label(egui::RichText::new("The next stroke's size, falloff, the wheel and the box are in the Brush tab.").color(style::MUTED).small());
        ui.add_space(6.0);
        ui.label("While holding a stroke");
        let mut pv = world.resource::<PointerView>().clone();
        ui.checkbox(&mut pv.hide_pointer, "Hide the pointer while holding a stroke");
        ui.horizontal(|ui| {
            ui.label("Clear window around the pointer");
            ui.add(egui::DragValue::new(&mut pv.clear_radius).range(0.0..=200.0).speed(0.5).suffix(" pt"))
                .on_hover_text("The video inside this radius is shown raw: no boxes, trails or text over it. 0 = off.");
        });
        ui.add_space(6.0);
        ui.label("On the video");
        ui.checkbox(&mut pv.paint_paths, "Paths of paint trackers' points")
            .on_hover_text("Each point of the selected paint tracker gets its path over the nearby frames: in its colour while it is in the cohort, faded red up to where it leaves. The points' dots on the shown frame always show.");
        ui.checkbox(&mut pv.ants, "Moving outline on the selected box")
            .on_hover_text("A black and white outline that moves around the selected sketch's box and the box being recorded, so you can see it on light and dark pictures.");
        ui.checkbox(&mut pv.dim_outside, "In a view, dim what is outside its box")
            .on_hover_text("Inside a sketch's view, the picture outside the box it follows is darker and has moving lines, so the box is clear.");
        if pv != *world.resource::<PointerView>() {
            *world.resource_mut::<PointerView>() = pv;
        }

        ui.separator();
        auto_speed(ui, world);

        ui.separator();
        ui.heading("Trackers");
        let mut auto_mask = world.resource::<tt_track::look::LookDefaults>().auto_mask;
        if ui
            .checkbox(&mut auto_mask, "Paint a new look's mask automatically")
            .on_hover_text("When you drag a look, the pixels that stand out from its rectangle's border (a cursor over the game) become its mask, so only they count. You can still repaint it in the Look editor.")
            .changed()
        {
            world.resource_mut::<tt_track::look::LookDefaults>().auto_mask = auto_mask;
        }

        ui.separator();
        views(ui, world);
        ui.label(egui::RichText::new("Each view has all of these in the Inspector (select the sketch: its View).").color(style::MUTED).small());

        ui.separator();
        ui.heading("Keys");
        let keymap = world.resource::<Keymap>();
        egui::Grid::new("keys").num_columns(2).striped(true).show(ui, |ui| {
            for (binding, action) in &keymap.bindings {
                ui.monospace(binding.chord());
                ui.label(describe(*action));
                ui.end_row();
            }
        });
        ui.label(
            egui::RichText::new("With the mouse: hold on the video to sketch (Sketch tool) · click a box to select · right-click for commands · Ctrl+hold: move only · Shift+hold: new sketch · wheel/middle-drag to zoom and pan")
                .color(style::MUTED)
                .small(),
        );

        ui.separator();
        ui.heading("Files");
        let dir = tt_media::proxy::data_dir();
        ui.label(egui::RichText::new(dir.display().to_string()).monospace().small());
        ui.label(egui::RichText::new("Projects (autosaved per video), proxies and the session live here.").weak().small());
        if ui.button("Open the folder").clicked() {
            let _ = std::fs::create_dir_all(&dir);
            crate::files::open_folder(&dir);
        }
    });
}

/// Updates (crate::update): this copy's version, a check, and the new
/// version in one click. Worded for people who don't build it themselves.
fn updates(ui: &mut egui::Ui, world: &mut World) {
    use crate::update::{State, Updater, installable, version};
    let up = world.resource::<Updater>().clone();
    ui.heading("Updates");
    ui.label(format!("You have trackertools {}.", version()));
    match up.state() {
        State::Idle => {}
        State::Checking => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Looking for a new version\u{2026}");
            });
        }
        State::UpToDate => {
            ui.label(egui::RichText::new("\u{2714} This is the latest version.").color(style::ACCENT));
        }
        State::Available(release) => {
            ui.label(egui::RichText::new(format!("A new version is out: {}", release.version)).strong().color(style::ACCENT));
            if !installable() {
                ui.label(egui::RichText::new("This copy was built from the source code: update it with git pull and cargo build.").color(style::MUTED).small());
            } else if release.download.is_some() {
                if ui
                    .button(format!("Update to {} and restart", release.version))
                    .on_hover_text("Downloads the new version, saves your work, and restarts trackertools with it. It takes about a minute.")
                    .clicked()
                {
                    up.update(release.clone());
                }
            } else {
                ui.label(egui::RichText::new("There's no download for this kind of computer yet.").color(style::MUTED));
            }
            if !release.notes.is_empty() {
                ui.collapsing("What's new", |ui| {
                    ui.label(crate::update::plain_notes(&release.notes));
                });
            }
            if !release.page.is_empty() {
                ui.hyperlink_to("See it on GitHub", &release.page);
            }
        }
        State::Downloading { release, got, total } => {
            ui.label(format!("Downloading version {}\u{2026}", release.version));
            let part = if total > 0 { got as f32 / total as f32 } else { 0.0 };
            ui.add(egui::ProgressBar::new(part).show_percentage().desired_width(240.0));
        }
        State::Ready { .. } | State::Restarting => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Installing, then restarting\u{2026}");
            });
        }
        State::Failed(problem) => {
            ui.label(egui::RichText::new(&problem.what).color(egui::Color32::from_rgb(0xf4, 0x3f, 0x5e))).on_hover_text(&problem.details);
        }
    }
    ui.horizontal(|ui| {
        if ui.add_enabled(!up.busy(), egui::Button::new("Check for updates")).clicked() {
            up.check(false);
        }
        let mut on_start = up.check_on_start;
        if ui.checkbox(&mut on_start, "Check when trackertools starts").changed() {
            world.resource_mut::<Updater>().check_on_start = on_start;
        }
    });
}

/// Build from your code and restart (`crate::rebuild`): only on a computer
/// with this program's source checkout. Its branch (switch to another, a
/// pull request's after Fetch), then one button.
fn build_from_code(ui: &mut egui::Ui, world: &mut World) {
    use crate::rebuild::{Rebuild, State};
    let Some(dir) = world.resource::<Rebuild>().dir.clone() else { return };
    if world.resource::<Rebuild>().checkout.is_none() {
        world.resource_mut::<Rebuild>().refresh();
    }
    let rb = world.resource::<Rebuild>().clone();
    let state = rb.state();
    // Re-read the checkout once a git step ends.
    let id = egui::Id::new("rebuild-was-busy");
    let was_busy = ui.data(|d| d.get_temp::<bool>(id)).unwrap_or(false);
    if was_busy && !rb.busy() {
        world.resource_mut::<Rebuild>().refresh();
    }
    ui.data_mut(|d| d.insert_temp(id, rb.busy()));
    let Some(co) = world.resource::<Rebuild>().checkout.clone() else { return };

    ui.add_space(6.0);
    ui.label(egui::RichText::new("Build from your code").strong());
    ui.label(egui::RichText::new(dir.display().to_string()).monospace().small().color(style::MUTED));
    ui.label(format!("Branch {} \u{b7} {}", co.branch, co.commit)).on_hover_text("What the next build is made from");
    let mut switch_to = None;
    ui.horizontal(|ui| {
        ui.add_enabled_ui(!rb.busy() && co.dirty == 0, |ui| {
            egui::ComboBox::from_id_salt("rebuild-branch").selected_text("Switch to\u{2026}").show_ui(ui, |ui| {
                for b in co.local.iter().filter(|b| **b != co.branch) {
                    if ui.selectable_label(false, b).clicked() {
                        switch_to = Some(b.clone());
                    }
                }
                if !co.remote.is_empty() {
                    ui.separator();
                    ui.label(egui::RichText::new("On GitHub").small().color(style::MUTED));
                    for b in &co.remote {
                        if ui.selectable_label(false, b).on_hover_text("A branch on GitHub, such as a pull request's: it is copied here to build it").clicked() {
                            switch_to = Some(b.clone());
                        }
                    }
                }
            });
        });
        if ui.add_enabled(!rb.busy(), egui::Button::new("Fetch")).on_hover_text("Get GitHub's branches (pull requests too) so you can switch to them").clicked() {
            rb.fetch();
        }
    });
    if co.dirty > 0 {
        ui.label(egui::RichText::new(format!("{} files have changes that aren't committed, so switching branches is off. The build uses them as they are.", co.dirty)).small().color(style::MUTED));
    }
    if let Some(b) = switch_to {
        rb.switch(&b);
    }
    let exe = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.join(crate::rebuild::program_file())));
    let tip = format!(
        "Builds branch {} and puts it at {}. Then trackertools saves your work, closes and starts the new build. The first build takes several minutes; later ones are faster.",
        co.branch,
        exe.map_or_else(|| "this program's folder".to_string(), |e| e.display().to_string())
    );
    if ui.add_enabled(!rb.busy(), egui::Button::new("Build and restart")).on_hover_text(tip).clicked() {
        rb.build_and_restart();
    }
    match &state {
        State::Running { what } => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(what);
            });
        }
        State::Done => {
            ui.label(egui::RichText::new("Built. Restarting\u{2026}").color(style::ACCENT));
        }
        State::Failed(why) => {
            ui.label(egui::RichText::new(why).color(egui::Color32::from_rgb(0xf4, 0x3f, 0x5e)));
        }
        State::Idle => {}
    }
    let log = rb.log();
    if !log.is_empty() {
        let running = matches!(state, State::Running { .. });
        if let Some(last) = log.last().filter(|_| running) {
            ui.label(egui::RichText::new(last).monospace().small().color(style::MUTED));
        }
        egui::CollapsingHeader::new("Build output").id_salt("rebuild-log").default_open(matches!(state, State::Failed(_))).show(ui, |ui| {
            egui::ScrollArea::vertical().max_height(180.0).stick_to_bottom(true).show(ui, |ui| {
                for line in &log {
                    ui.label(egui::RichText::new(line).monospace().small());
                }
            });
        });
    }
}

/// How a view follows its sketch (Tab into a sketch; tt_core::view's
/// FrameParams), for new views: the zoom, and how steadily it pans: a dead
/// zone (small moves of the box don't move the view) and smoothing. The
/// sketch's box wobbles a little with the hand, and a view that follows it
/// exactly makes a still picture shake inside it; a preview shows both
/// sides of that, and a button gives the project's views the same.
fn views(ui: &mut egui::Ui, world: &mut World) {
    ui.heading("Views");
    ui.label(
        "A view follows its sketch's box (Tab into a sketch). The box wobbles a little with your hand, and a view that follows it exactly makes the picture inside shake, \
         even where the video is still. A dead zone lets small moves of the box go, and smoothing calms the rest.",
    );
    let mut p = world.resource::<ViewDefaults>().params.clone();
    let before = p.clone();
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        ui.label("Follow:");
        for (name, damping, dead) in [("closely", 0.1, 0.0), ("steadily", 0.3, 0.08), ("very steadily", 0.6, 0.18)] {
            let on = (p.pan_damping - damping).abs() < 1e-3 && (p.dead_zone - dead).abs() < 1e-3;
            if ui.selectable_label(on, name).clicked() {
                (p.pan_damping, p.dead_zone) = (damping, dead);
            }
        }
    });
    egui::Grid::new("view-follow").num_columns(2).show(ui, |ui| {
        ui.label("dead zone").on_hover_text("How far (a fraction of the view, from its middle) the box may wander before the view moves at all. 0: it always follows.");
        ui.add(egui::Slider::new(&mut p.dead_zone, 0.0..=0.4).max_decimals(2).custom_formatter(|v, _| format!("{:.0}% of the view", v * 100.0)));
        ui.end_row();
        ui.label("smoothing").on_hover_text("How calmly the view pans (seconds of video; no lag: it looks both ways).");
        ui.add(egui::Slider::new(&mut p.pan_damping, 0.0..=2.0).max_decimals(2).suffix(" s"));
        ui.end_row();
        ui.label("zoom");
        let mut mode = if p.pan_only { 0 } else if p.lock_zoom { 1 } else { 2 };
        ui.vertical(|ui| {
            ui.radio_value(&mut mode, 0, "only pan: the zoom is yours (the wheel)");
            ui.radio_value(&mut mode, 1, "steady zoom: the widest the sketch needs");
            ui.radio_value(&mut mode, 2, "zoom with the sketch's size (smoothed)");
        });
        (p.pan_only, p.lock_zoom) = (mode == 0, mode != 2);
        ui.end_row();
    });
    egui::CollapsingHeader::new("Preview").id_salt("view-follow-preview").default_open(true).show(ui, |ui| {
        view_preview(ui, &p);
    });
    if p != before {
        world.resource_mut::<ViewDefaults>().params = p.clone();
    }
    let views: Vec<Entity> = {
        let mut q = world.query_filtered::<(Entity, &tt_core::view::FrameParams), bevy_ecs::query::Without<bevy_ecs::entity_disabling::Disabled>>();
        q.iter(world).filter(|(_, v)| (v.pan_damping, v.dead_zone, v.pan_only, v.lock_zoom) != (p.pan_damping, p.dead_zone, p.pan_only, p.lock_zoom)).map(|(e, _)| e).collect()
    };
    if ui
        .add_enabled(!views.is_empty(), egui::Button::new(format!("Use these for the {} other view(s) in this project", views.len())))
        .on_hover_text("These settings are for new views; this gives them to the views you already have (one undo step).")
        .clicked()
    {
        tt_core::history::edit(world, "Views follow the same way", |tx| {
            for v in views {
                tx.modify::<tt_core::view::FrameParams>(v, |q| {
                    (q.pan_damping, q.dead_zone, q.pan_only, q.lock_zoom) = (p.pan_damping, p.dead_zone, p.pan_only, p.lock_zoom);
                });
            }
        });
    }
}

/// A box drawn by hand around a subject that stands still, moves across,
/// and stands still again (source px, a 320 × 180 source): it wobbles a few
/// pixels all along, as a hand does.
fn wobbly_box(f: i64) -> [f32; 6] {
    let t = f as f64 / 60.0;
    let s = ((t - 2.5) / 1.2).clamp(0.0, 1.0);
    let x = 110.0 + 100.0 * s * s * (3.0 - 2.0 * s) + 2.6 * (11.0 * t).sin() + 1.8 * (17.3 * t + 1.0).cos();
    let y = 92.0 + 2.2 * (13.1 * t).sin() + 1.2 * (7.7 * t).cos();
    let h = 17.0 + 2.0 * (6.3 * t).sin();
    [x, y, x - h, y - h, x + h, y + h].map(|v| v as f32)
}

/// The preview: on the left the source, with the hand-drawn box (orange) and
/// the view following it (blue); on the right what the view shows, a still
/// scene (the dots) and the subject: where the dots shake, the view moved.
fn view_preview(ui: &mut egui::Ui, p: &tt_core::view::FrameParams) {
    use egui::{Color32, Pos2, Rect, Stroke, Vec2};
    const N: i64 = 6 * 60;
    let size = tt_core::view::SourceSize { width: 320.0, height: 180.0 };
    let mut sig = tt_core::signal::Signal::new(tt_core::sketch::BOX_CHANNELS);
    for f in 0..N {
        sig.set(f, &wobbly_box(f));
    }
    let Some((first, frames)) = tt_core::view::frame_views(&sig, None, p, 60.0, &size) else { return };
    let f = ((ui.input(|i| i.time) * 60.0) as i64).rem_euclid(N);
    let Some(mut v) = frames.get((f - first) as usize).copied() else { return };
    // Only panning, the zoom is the wheel's: shown as you would look, 3× closer.
    if p.pan_only {
        (v[2], v[3]) = (v[2] / 3.0, v[3] / 3.0);
    }
    let w = ((ui.available_width() - 12.0) / 2.0).clamp(120.0, 240.0);
    let h = w * 9.0 / 16.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(2.0 * w + 12.0, h + 16.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    let left = Rect::from_min_size(rect.min, Vec2::new(w, h));
    let right = Rect::from_min_size(rect.min + Vec2::new(w + 12.0, 0.0), Vec2::new(w, h));
    for r in [left, right] {
        painter.rect_filled(r, 3.0, style::BG);
        painter.rect_stroke(r, 3.0, Stroke::new(1.0, style::RULER), egui::StrokeKind::Inside);
    }
    // The still scene: a grid of dots, fixed in the source.
    let dots: Vec<[f64; 2]> = (0..9).flat_map(|i| (0..5).map(move |j| [20.0 + 35.0 * i as f64, 18.0 + 36.0 * j as f64])).collect();
    let in_left = |q: [f64; 2]| left.min + Vec2::new((q[0] / size.width) as f32 * w, (q[1] / size.height) as f32 * h);
    for d in &dots {
        painter.circle_filled(in_left(*d), 1.5, style::MUTED);
    }
    let b = wobbly_box(f);
    let boxed = |to: &dyn Fn([f64; 2]) -> Pos2| Rect::from_two_pos(to([b[2] as f64, b[3] as f64]), to([b[4] as f64, b[5] as f64]));
    painter.rect_stroke(boxed(&in_left), 0.0, Stroke::new(1.5, style::HAND), egui::StrokeKind::Middle);
    // The view: [cx, cy, crop_w, crop_h, ..] in source px.
    let crop = Rect::from_center_size(in_left([v[0], v[1]]), Vec2::new((v[2] / size.width) as f32 * w, (v[3] / size.height) as f32 * h));
    painter.rect_stroke(crop, 0.0, Stroke::new(1.5, style::VIEW), egui::StrokeKind::Middle);
    // What the view shows: the source through it.
    let in_right = |q: [f64; 2]| right.center() + Vec2::new(((q[0] - v[0]) / v[2]) as f32 * w, ((q[1] - v[1]) / v[3]) as f32 * h);
    let clip = painter.with_clip_rect(right.shrink(1.0));
    for d in &dots {
        clip.circle_filled(in_right(*d), 2.0, Color32::from_gray(150));
    }
    clip.rect_stroke(boxed(&in_right), 0.0, Stroke::new(1.5, style::HAND), egui::StrokeKind::Middle);
    let text = |at: Pos2, s: &str| {
        painter.text(at, egui::Align2::LEFT_TOP, s, egui::FontId::proportional(10.0), style::MUTED);
    };
    text(left.left_bottom() + Vec2::new(0.0, 2.0), "the source: your box, the view");
    text(right.left_bottom() + Vec2::new(0.0, 2.0), "in the view: a still scene should hold still");
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
}

/// Anticipatory speed (tt_core::autospeed): the switch, the speed range and
/// how far ahead it reads; the rest under Advanced.
fn auto_speed(ui: &mut egui::Ui, world: &mut World) {
    let mut a = world.resource::<AutoSpeed>().clone();
    let (bias, comfort) = {
        let s = world.resource::<AutoSpeedState>();
        (s.bias, s.calibrated_comfort())
    };
    ui.heading("Anticipatory speed");
    ui.checkbox(&mut a.enabled, "Set the playback speed for me while I hold a stroke").on_hover_text(
        "While you hold a stroke with the video playing, it reads ahead in the parent sketch (the one whose view you're drawing in). \
         It slows down before what is ahead is busier than usual: its box bigger than usual for the whole sketch (the subject was hard to follow there), \
         or its motion or box busier than they were over the last few seconds (so after a still stretch, even a few pixels' move ahead slows it). \
         Where it is as calm as usual, it plays fast. Q/E during a stroke multiply its speed.",
    );
    ui.add_enabled_ui(a.enabled, |ui| {
        egui::Grid::new("auto-speed").num_columns(2).show(ui, |ui| {
            ui.label("busy stretches at").on_hover_text("The speed where the sketch ahead is at its busiest.");
            ui.add(egui::Slider::new(&mut a.slowest, 0.02..=1.0).logarithmic(true).max_decimals(2).prefix("\u{d7}"));
            ui.end_row();
            ui.label("calm stretches at").on_hover_text("The speed where the sketch ahead is as calm as it gets. In between, the speed goes smoothly from one to the other.");
            ui.add(egui::Slider::new(&mut a.fastest, 0.25..=4.0).logarithmic(true).max_decimals(2).prefix("\u{d7}"));
            ui.end_row();
            ui.label("look ahead").on_hover_text("How far ahead it reads (seconds of video): the busiest moment in this window sets the speed, so it slows this long before a busy stretch.");
            ui.add(egui::Slider::new(&mut a.look_ahead, 0.0..=5.0).max_decimals(2).suffix(" s"));
            ui.end_row();
            ui.label("sensitivity").on_hover_text(
                "How much busier than the last few seconds counts as fully busy. Up to 1.5\u{d7} is ordinary. Lower: a smaller change slows it (a slight move after standing still).",
            );
            ui.add(egui::Slider::new(&mut a.sensitivity, 1.6..=10.0).logarithmic(true).max_decimals(1).suffix("\u{d7} busier"));
            ui.end_row();
            ui.label("remembers").on_hover_text("How far back it looks (seconds of video) to know how calm the subject has been lately.");
            ui.add(egui::Slider::new(&mut a.memory, 0.5..=10.0).max_decimals(1).suffix(" s"));
            ui.end_row();
        });
        // What it reads right now, while it drives.
        let (acting, busy, reason) = {
            let s = world.resource::<AutoSpeedState>();
            (s.acting(), s.busy, s.reason)
        };
        if acting {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("now: {reason}")).small());
                if let Some(u) = busy {
                    ui.add(egui::ProgressBar::new(u as f32).desired_width(140.0).text(format!("{:.0}% busy", 100.0 * u)));
                }
            });
            ui.ctx().request_repaint();
        }
        a.fastest = a.fastest.max(a.slowest);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(format!("Q/E while it drives: your multiplier ×{bias:.2}")).color(style::MUTED).small());
            if ui.add_enabled((bias - 1.0).abs() > 1e-9, egui::Button::new("Reset").small()).clicked() {
                world.resource_mut::<AutoSpeedState>().bias = 1.0;
            }
        });

        egui::CollapsingHeader::new("Advanced").id_salt("auto-speed-advanced").show(ui, |ui| {
            ui.label("Read ahead in");
            ui.horizontal_wrapped(|ui| {
                ui.radio_value(&mut a.foresight, Foresight::Parent, "the parent sketch").on_hover_text("The sketch whose view you're drawing in: its box shows where the subject was hard to follow. On the source, the sketch you're editing.");
                ui.radio_value(&mut a.foresight, Foresight::Editing, "the sketch I'm editing");
                ui.radio_value(&mut a.foresight, Foresight::Both, "both (the busier)");
            });
            ui.add_space(4.0);
            ui.checkbox(&mut a.react_to_hand, "Also slow down when my hand races or starts to jiggle").on_hover_text(
                "Reacts to your hand right now (it can't see ahead): slower while the subject moves faster on screen than your comfortable hand speed,                  or while your jiggle grows. Also drives the speed where there's nothing to read ahead.",
            );
            ui.add_enabled_ui(a.react_to_hand, |ui| {
                egui::Grid::new("auto-speed-hand").num_columns(2).show(ui, |ui| {
                    ui.label("comfortable hand speed").on_hover_text("How fast your hand follows comfortably, on screen. Calibrate sets it from your recent strokes.");
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut a.comfort).range(20.0..=5000.0).speed(2.0).suffix(" pt/s"));
                        let tip = match comfort {
                            Some(c) => format!("Your hand moved at up to about {c:.0} pt/s (on screen, at 1×) in most of your recent strokes: use that."),
                            None => "Hold a few strokes with the video playing first: it measures how fast your hand moves.".to_string(),
                        };
                        if ui.add_enabled(comfort.is_some(), egui::Button::new("Calibrate from my recent strokes")).on_hover_text(tip.clone()).on_disabled_hover_text(tip).clicked()
                            && let Some(c) = comfort
                        {
                            a.comfort = c.round() as f32;
                        }
                    });
                    ui.end_row();
                    ui.label("jiggle sensitivity").on_hover_text("How strongly your jiggle growing slows it. 0 ignores it; 1 halves the speed at three times its calm size.");
                    ui.add(egui::Slider::new(&mut a.jiggle, 0.0..=4.0).max_decimals(2));
                    ui.end_row();
                });
            });
            ui.add_space(4.0);
            egui::Grid::new("auto-speed-smooth").num_columns(2).show(ui, |ui| {
                ui.label("standing still below").on_hover_text("Motion slower than this (source pixels per second) counts as none: the sketch's own wobble isn't a move.");
                ui.add(egui::DragValue::new(&mut a.still).range(0.1..=100.0).speed(0.2).suffix(" px/s"));
                ui.end_row();
                ui.label("slow down within").on_hover_text("How quickly it slows down (real seconds): short, so it brakes in time.");
                ui.add(egui::DragValue::new(&mut a.slow_down).range(0.01..=2.0).speed(0.005).suffix(" s"));
                ui.end_row();
                ui.label("speed up within").on_hover_text("How quickly it speeds back up (real seconds): long, so it doesn't lurch.");
                ui.add(egui::DragValue::new(&mut a.speed_up).range(0.05..=10.0).speed(0.01).suffix(" s"));
                ui.end_row();
            });
            if ui.button("Defaults").on_hover_text("Put everything back (keeps it on or off)").clicked() {
                a = AutoSpeed { enabled: a.enabled, ..AutoSpeed::default() };
            }
        });
    });
    if a != *world.resource::<AutoSpeed>() {
        *world.resource_mut::<AutoSpeed>() = a;
    }
}

/// What an action does, for the key list.
fn describe(action: Action) -> &'static str {
    use Action::*;
    match action {
        Seek(_) => "Go to a frame",
        SetRate(_) => "Set the playback speed",
        ToggleLoop => "Loop on/off",
        TogglePlay => "Play / pause (also while holding a stroke)",
        StepForward => "Next frame",
        StepBackward => "Previous frame",
        JumpForward => "Jump forward 10 frames",
        JumpBackward => "Jump back 10 frames",
        GoToStart => "Go to the start",
        GoToEnd => "Go to the end",
        FasterPlayback => "Faster playback (= capture speed)",
        SlowerPlayback => "Slower playback (= capture speed)",
        ShuttleForward => "Play forward; again: twice as fast (up to 8×)",
        ShuttleBackward => "Play backward; again: twice as fast (up to 8×)",
        FrameAll => "Fit the video in the viewport",
        OpenFile => "Open a video",
        Undo => "Undo",
        Redo => "Redo",
        Tool(tt_core::tool::Tool::Track) => "Track tool on/off: drag a pattern or click a point on the video",
        Tool(tt_core::tool::Tool::Draw) => "Draw tool on/off: a tracker's point by hand, or a manual dot",
        Tool(_) => "Sketch tool on/off",
        Cancel => "Cancel the stroke, or leave the tool",
        DeselectAll => "Deselect all",
        EnterView => "Enter a view that follows the selected sketch, tracker or subject",
        ExitView => "Back out to the parent view",
        Delete => "Delete the selection",
        Duplicate => "Duplicate the selected sketches",
        SelectAll => "Select all sketches",
        Rename => "Rename the selection",
        Track => "Track the selected sketch from here (on a tracker: re-seed it here)",
        ToggleSnap => "Snap the playhead to the start and end of things on the timeline while scrubbing (Ctrl inverts)",
        MarkIn => "Mark the in point here: the first frame an export renders",
        MarkOut => "Mark the out point here: the last frame an export renders",
        ClearMarks => "Clear the in and out points (exports render the whole video)",
        GoToIn => "Go to the in point",
        GoToOut => "Go to the out point",
    }
}
