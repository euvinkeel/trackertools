//! Panels are functions of the world (DESIGN §1, §14): they read state and
//! queue actions/intents; they never own state or mutate the document directly.

mod brush;
pub mod export;
mod focus_inspector;
mod inspector;
pub mod look_editor;
mod menu;
pub mod outliner;
mod settings;
mod overlay;
mod tracks;
pub mod timeline;
pub mod viewport;

use bevy_ecs::prelude::*;
use tt_core::input::{Action, PendingActions};
use tt_core::time::timecode;
use tt_core::tool::{ActiveTool, Tool};
use tt_core::transport::Transport;

use crate::layout::{Layout, Pane};
use crate::media::{Media, OpenRequest, StatusLine, proxy_status};
use crate::session::Session;
use crate::style;

pub fn draw(ui: &mut egui::Ui, world: &mut World) {
    crate::update::drive(ui.ctx(), world);
    crate::rebuild::drive(ui.ctx(), world);
    egui::Panel::top("top_bar").show(ui, |ui| top_bar(ui, world));
    egui::CentralPanel::no_frame().show(ui, |ui| {
        world.resource_scope(|world, mut layout: Mut<Layout>| {
            let mut behavior = Behavior { world };
            layout.tree.ui(&mut behavior, ui);
        });
    });
    export::ui(ui.ctx(), world);
    crate::update::prompt(ui.ctx(), world);
    crate::setup::window(ui.ctx(), &mut world.resource_mut::<crate::setup::Doctor>());
    // A rename the outliner didn't take (its tab isn't showing) is dropped, not kept for later.
    world.resource_mut::<tt_core::commands::RenameRequest>().0 = None;
}

/// A new version, in one click from anywhere (Settings \u{2192} Updates has the details).
fn update_button(ui: &mut egui::Ui, world: &World) {
    use crate::update::{State, Updater, installable};
    let up = world.resource::<Updater>();
    match up.state() {
        State::Available(release) if installable() && release.download.is_some() => {
            ui.separator();
            let button = egui::Button::new(egui::RichText::new(format!("\u{2B06} Update to {}", release.version)).color(style::ACCENT));
            let tip = "A new version of trackertools is out. Click to download it, save your work, and restart with it (Settings, Updates says what's new).";
            if ui.add(button).on_hover_text(tip).clicked() {
                up.update(release);
            }
        }
        State::Downloading { got, total, .. } => {
            ui.separator();
            let pct = (100 * got).checked_div(total).unwrap_or(0);
            ui.label(egui::RichText::new(format!("Updating\u{2026} {pct}%")).color(style::ACCENT));
        }
        State::Ready { .. } | State::Restarting => {
            ui.separator();
            ui.label(egui::RichText::new("Restarting with the new version\u{2026}").color(style::ACCENT));
        }
        _ => {}
    }
}

/// What the Project menu asks for (done after the top bar is drawn: file dialogs block).
enum ProjectAction {
    Switch(std::path::PathBuf),
    New,
    SaveAs,
    Open,
    Reveal,
    Forget(std::path::PathBuf),
}

/// The Project menu beside the video's name: its projects (the open one
/// selected), and New, Save as, Open and Show the file.
fn project_menu(ui: &mut egui::Ui, world: &World, video: &std::path::Path) -> Option<ProjectAction> {
    use crate::project::{own_project, project_name};
    let current = world.resource::<crate::project::ProjectFile>().path.clone();
    let own = own_project(video);
    let mut list: Vec<std::path::PathBuf> = world.resource::<Session>().projects_of(video).to_vec();
    if let Some(o) = own.clone().filter(|o| !list.contains(o)) {
        list.push(o);
    }
    let name = current.as_deref().map_or_else(|| "none".to_string(), |p| project_name(video, p));
    let mut action = None;
    ui.menu_button(format!("Project: {name} \u{23f7}"), |ui| {
        ui.label(egui::RichText::new("This video's projects").color(style::MUTED).small());
        for p in &list {
            let open = current.as_deref() == Some(p.as_path());
            let gone = !p.exists() && own.as_deref() != Some(p.as_path());
            if gone {
                let tip = format!("{} is not there now. Click to remove it from this list.", p.display());
                if ui.button(egui::RichText::new(format!("{} (file not found)", project_name(video, p))).color(style::MUTED)).on_hover_text(tip).clicked() {
                    action = Some(ProjectAction::Forget(p.clone()));
                }
                continue;
            }
            if ui.add(egui::Button::new(project_name(video, p)).selected(open)).on_hover_text(p.display().to_string()).clicked() && !open {
                action = Some(ProjectAction::Switch(p.clone()));
                ui.close();
            }
        }
        ui.separator();
        if ui.button("New project\u{2026}").on_hover_text("An empty project on this video, in a file you choose. The open project is saved first.").clicked() {
            action = Some(ProjectAction::New);
            ui.close();
        }
        if ui.button("Save project as\u{2026}").on_hover_text("A copy of this project in a file you choose. Changes then save to that file. The old file keeps what it had.").clicked() {
            action = Some(ProjectAction::SaveAs);
            ui.close();
        }
        if ui.button("Open project\u{2026}").on_hover_text("Open a .ttproj file, and its video with it").clicked() {
            action = Some(ProjectAction::Open);
            ui.close();
        }
        if ui.add_enabled(current.as_ref().is_some_and(|p| p.exists()), egui::Button::new("Show the project file")).clicked() {
            action = Some(ProjectAction::Reveal);
            ui.close();
        }
    });
    action
}

fn run_project_action(world: &mut World, action: ProjectAction) {
    use crate::project::EXTENSION;
    let video = world.get_resource::<Media>().map(|m| m.index().path.clone());
    let dialog = || {
        let mut d = rfd::FileDialog::new().add_filter("trackertools project", &[EXTENSION]);
        if let Some(dir) = video.as_ref().and_then(|v| v.parent()) {
            d = d.set_directory(dir);
        }
        d
    };
    let suggested = || {
        let stem = video.as_ref().and_then(|v| v.file_stem()).map_or_else(|| "project".to_string(), |s| s.to_string_lossy().into_owned());
        format!("{stem} project.{EXTENSION}")
    };
    match action {
        // The video's own project before anything was saved in it: an empty one.
        ProjectAction::Switch(p) if !p.exists() => {
            crate::project::new_project(world, p);
        }
        ProjectAction::Switch(p) => {
            crate::project::open_project(world, p);
        }
        ProjectAction::New => {
            if let Some(p) = dialog().set_title("New project on this video").set_file_name(suggested()).save_file() {
                crate::project::new_project(world, p);
            }
        }
        ProjectAction::SaveAs => {
            if let Some(p) = dialog().set_title("Save project as").set_file_name(suggested()).save_file() {
                crate::project::save_as(world, p);
            }
        }
        ProjectAction::Open => {
            if let Some(p) = dialog().set_title("Open project").pick_file() {
                crate::project::open_project(world, p);
            }
        }
        ProjectAction::Forget(p) => crate::project::forget(world, &p),
        ProjectAction::Reveal => {
            if let Some(p) = world.resource::<crate::project::ProjectFile>().path.clone() {
                crate::files::reveal(&p);
            }
        }
    }
}

fn top_bar(ui: &mut egui::Ui, world: &mut World) {
    let mut open = false;
    let mut reopen = None;
    let mut history_action = None;
    let mut new_sketch = false;
    let mut track_kind = None;
    let mut report_problem = false;
    let mut open_doctor = false;
    let mut project_action = None;
    let tracking = tracks::summary(world);
    let kind = world.resource::<tt_track::NewTrackers>().method;
    let cotracker = tt_track::job::cotracker_availability();
    let t = world.resource::<Transport>();
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("trackertools").strong().color(style::ACCENT));
        ui.separator();
        open = ui.button("Open…").on_hover_text("Open a video (Ctrl+O), or drop a file on the window").clicked();
        let recent = world.resource::<Session>().recent();
        ui.add_enabled_ui(!recent.is_empty(), |ui| {
            ui.menu_button("Recent ⏷", |ui| {
                for path in recent {
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    if ui.button(name).on_hover_text(path.display().to_string()).clicked() {
                        reopen = Some(path.clone());
                    }
                }
            });
        });
        match world.get_resource::<Media>() {
            Some(m) => {
                let i = m.index();
                ui.label(egui::RichText::new(&m.name).strong());
                project_action = project_menu(ui, world, &i.path);
                // (Short: the top bar holds the tools too. The codec and the exact rate on hover.)
                ui.label(egui::RichText::new(format!("{}×{} \u{b7} {:.2} fps", i.width, i.height, i.fps.as_f64())).color(style::MUTED))
                    .on_hover_text(format!("{} \u{b7} {} frames @ {:.3} fps", i.codec, i.frame_count(), i.fps.as_f64()));
                if let Some(status) = proxy_status(m) {
                    ui.separator();
                    ui.label(egui::RichText::new(status).color(style::MUTED));
                }
                if let Some((text, tip)) = &tracking {
                    ui.separator();
                    ui.label(egui::RichText::new(text).color(tracks::TRACK)).on_hover_text(tip);
                }
            }
            None => {
                ui.label(egui::RichText::new("no media (demo clock)").color(style::MUTED));
            }
        }
        update_button(ui, world);
        if world.resource::<crate::setup::Doctor>().last_run_failed {
            ui.separator();
            report_problem = ui
                .button(egui::RichText::new("\u{26a0} Report a problem").color(egui::Color32::from_rgb(0xfb, 0xbf, 0x24)))
                .on_hover_text("trackertools stopped because of an error the last time. Click Report a problem. Then click Copy report and send the report to the person who gave you trackertools.")
                .clicked();
        }
        {
            use crate::cotracker::Step;
            let co = world.resource::<crate::setup::Doctor>().cotracker();
            let (step, finished) = (co.step(), co.finished());
            let shown = match &step {
                s if s.busy() => Some((format!("\u{23f3} CoTracker setup: {}", s.short()), style::ACCENT)),
                Step::Failed(_) => Some(("\u{26a0} CoTracker setup stopped".to_string(), egui::Color32::from_rgb(0xfb, 0xbf, 0x24))),
                Step::Done(_) if finished.is_some_and(|s| s < 60) => Some(("\u{2714} CoTracker is ready".to_string(), style::ACCENT)),
                _ => None,
            };
            if let Some((text, color)) = shown {
                ui.separator();
                open_doctor |= ui.button(egui::RichText::new(text).color(color)).on_hover_text(format!("{} Click to open the doctor.", step.text())).clicked();
                ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
            }
        }
        let history = world.resource::<tt_core::history::History>();
        ui.separator();
        let undo_tip = history.undo_label().map_or("Nothing to undo".to_string(), |l| format!("Undo {l} (Ctrl+Z)"));
        let redo_tip = history.redo_label().map_or("Nothing to redo".to_string(), |l| format!("Redo {l} (Ctrl+Shift+Z)"));
        if ui.add_enabled(history.can_undo(), egui::Button::new("⟲")).on_hover_text(undo_tip).on_disabled_hover_text("Nothing to undo").clicked() {
            history_action = Some(Action::Undo);
        }
        if ui.add_enabled(history.can_redo(), egui::Button::new("⟳")).on_hover_text(redo_tip).on_disabled_hover_text("Nothing to redo").clicked() {
            history_action = Some(Action::Redo);
        }
        ui.separator();
        let sketching = world.resource::<ActiveTool>().0 == Tool::Sketch;
        let chord = world.resource::<tt_core::input::Keymap>().chord_for(Action::Tool(Tool::Sketch)).unwrap_or_default();
        if ui
            .selectable_label(sketching, "✏ Sketch")
            .on_hover_text(format!(
                "Sketch tool ({chord})\n\
                 • hold on the video: record into the selected sketch at the shown frame (paused: edit that instant)\n\
                 • Space while holding: play and record across frames (at the playback speed: Q slower, E faster)\n\
                 • the view holds still while you hold (the wheel too; the Brush tab can give it zoom, size or falloff)\n\
                 • Ctrl+hold: move only (keep the box size) · Shift+hold: new sketch\n\
                 • click: select the sketch under the cursor · Alt+A: deselect · Esc: cancel\n\
                 • Tab: enter the selected sketch's view (sketch inside it for detail) · Shift+Tab: back up"
            ))
            .clicked()
        {
            history_action = Some(Action::Tool(Tool::Sketch));
        }
        let tracking_tool = world.resource::<ActiveTool>().0 == Tool::Track;
        let track_chord = world.resource::<tt_core::input::Keymap>().chord_for(Action::Tool(Tool::Track)).unwrap_or_default();
        for (method, text, what) in [
            (tt_track::Method::Template, "⌖ Template tracker", "matches the pattern you show it on every frame: fast, sub-pixel, built in"),
            (tt_track::Method::CoTracker, "⌖ CoTracker", "Meta's CoTracker3, a learned point tracker, run in Python (PyTorch and its weights)"),
        ] {
            let usable = method == tt_track::Method::Template || cotracker.is_ok();
            let r = ui.selectable_label(tracking_tool && kind == method, text);
            if !usable {
                open_doctor |= r.on_hover_text("CoTracker is not set up on this computer. Click CoTracker. The doctor shows what CoTracker needs and sets it up.").clicked();
                continue;
            }
            let r = r
                .on_hover_text(format!(
                    "Track tool ({track_chord}) making a {}: {what}\n\
                     \u{2022} template tracker: drag a rectangle around what to follow (its pattern, a look), or click a point (the dashed box's size; Ctrl+wheel sizes it)\n\
                     \u{2022} CoTracker: click the pixel to follow (a reset point)\n\
                     \u{2022} inside a sketch's box it searches there; elsewhere, the whole frame\n\
                     \u{2022} a new tracker waits: Back, Both or Forward in the Inspector (or its right-click menu) tracks it; Pause stops it\n\
                     \u{2022} with a tracker selected: where it missed, another look (a CoTracker: a reset point); Shift: a new tracker\n\
                     \u{2022} select a look (Outliner, Inspector) to paint which of its pixels are the subject",
                    tracks::kind_name(method)
                ));
            if r.clicked() {
                track_kind = Some(method);
                // The other kind while the tool is on: switch kinds, keep the tool.
                if !tracking_tool || kind == method {
                    history_action = Some(Action::Tool(Tool::Track));
                }
            }
        }
        let drawing = world.resource::<ActiveTool>().0 == Tool::Draw;
        let draw_chord = world.resource::<tt_core::input::Keymap>().chord_for(Action::Tool(Tool::Draw)).unwrap_or_default();
        if ui
            .selectable_label(drawing, egui::RichText::new("Draw by hand").color(if drawing { style::HAND } else { style::TEXT }))
            .on_hover_text(format!(
                "Draw tool ({draw_chord}): a tracker's point by hand, frame by frame\n\
                 \u{2022} with a tracker selected: hold on the video, and on every frame shown its point is where you hold (paused: this frame; Space plays). \
                 What you draw is its output there, over its automatic results (orange on the video and the timeline)\n\
                 \u{2022} with nothing selected (or Shift+hold): a manual dot, a tracker that is only what you draw\n\
                 \u{2022} Alt+hold: erase what was drawn (its own results show again) \u{b7} Esc: cancel the hold"
            ))
            .clicked()
        {
            history_action = Some(Action::Tool(Tool::Draw));
        }
        let sketch_selected = world.resource::<tt_core::selection::Selection>().primary().is_some_and(|e| tt_core::sketch::is_sketch(world, e));
        if ui
            .add_enabled(sketch_selected || !sketching, egui::Button::new("✚ New sketch"))
            .on_hover_text("The next stroke starts a new sketch instead of editing the selected one (deselects; Shift+hold does the same for one stroke)")
            .on_disabled_hover_text("Nothing is selected: the next stroke already starts a new sketch")
            .clicked()
        {
            new_sketch = true;
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // egui repaints on demand, so frame time only means something while playing.
            if t.playing {
                let dt = ui.ctx().input(|i| i.stable_dt).max(1e-4);
                ui.monospace(format!("{:>5.1} ms/frame", dt * 1000.0));
            } else {
                ui.label(egui::RichText::new("idle").weak());
            }
            ui.separator();
            ui.monospace(timecode(t.frame(), t.fps));
            // The last message, in the room left (cut short; all of it on hover).
            if let Some((msg, error)) = &world.resource::<StatusLine>().0 {
                ui.separator();
                let color = if *error { style::LOST } else { style::MUTED };
                ui.add(egui::Label::new(egui::RichText::new(msg).color(color)).truncate()).on_hover_text(msg);
            }
        });
    });
    if open {
        world.resource_mut::<PendingActions>().push(Action::OpenFile);
    }
    if let Some(a) = project_action {
        run_project_action(world, a);
    }
    if report_problem {
        let mut doctor = world.resource_mut::<crate::setup::Doctor>();
        (doctor.open, doctor.last_run_failed) = (true, false);
        doctor.recheck();
    }
    if open_doctor {
        world.resource_mut::<crate::setup::Doctor>().show();
    }
    if let Some(method) = track_kind {
        world.resource_mut::<tt_track::NewTrackers>().method = method;
    }
    if let Some(a) = history_action {
        world.resource_mut::<PendingActions>().push(a);
    }
    if new_sketch {
        world.resource_mut::<tt_core::selection::Selection>().clear();
        world.resource_mut::<ActiveTool>().0 = Tool::Sketch;
    }
    if let Some(path) = reopen {
        world.resource_mut::<OpenRequest>().0 = Some(path);
    }
}

struct Behavior<'w> {
    world: &'w mut World,
}

impl egui_tiles::Behavior<Pane> for Behavior<'_> {
    fn pane_ui(&mut self, ui: &mut egui::Ui, _tile: egui_tiles::TileId, pane: &mut Pane) -> egui_tiles::UiResponse {
        match pane {
            Pane::Viewport => viewport::ui(ui, self.world),
            Pane::Timeline => timeline::ui(ui, self.world),
            Pane::Inspector => inspector::ui(ui, self.world),
            Pane::Outliner => outliner::ui(ui, self.world),
            Pane::Brush => brush::ui(ui, self.world),
            Pane::Settings => settings::ui(ui, self.world),
        }
        egui_tiles::UiResponse::None
    }

    fn tab_title_for_pane(&mut self, pane: &Pane) -> egui::WidgetText {
        // A new version out: a red dot on the Settings tab (its Updates section has it).
        if *pane == Pane::Settings && self.world.resource::<crate::update::Updater>().news().is_some() {
            let mut job = egui::text::LayoutJob::default();
            // (The tab's own colour for its name, PLACEHOLDER; the dot red, centred on the line.)
            let font = egui::TextFormat { font_id: egui::FontId::proportional(14.0), color: egui::Color32::PLACEHOLDER, valign: egui::Align::Center, ..Default::default() };
            job.append(pane.title(), 0.0, font.clone());
            job.append("\u{2022}", 3.0, egui::TextFormat { color: style::LOST, font_id: egui::FontId::proportional(22.0), valign: egui::Align::Max, ..font });
            return job.into();
        }
        pane.title().into()
    }

    fn simplification_options(&self) -> egui_tiles::SimplificationOptions {
        egui_tiles::SimplificationOptions { all_panes_must_have_tabs: true, ..Default::default() }
    }
}
