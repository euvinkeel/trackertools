//! The eframe host: owns the world and runs one app frame.
//!
//! eframe calls `logic` before every UI pass (also while the window is hidden),
//! so the wall clock, key → action translation and the PreUi schedule live
//! there. `ui` draws panels from the world, then runs PostUi so panel gestures
//! (scrubbing) apply before the frame is presented.

use std::time::{Duration, Instant};

use tt_core::input::{Keymap, KeysHeld, PendingActions};
use tt_core::tool::PointerFrame;
use tt_core::time::WallClock;
use tt_core::transport::Transport;
use tt_core::{AppBuilder, Core, CoreModules};

use crate::media::{MediaModule, OpenRequest};
use crate::pointer::PointerService;
use crate::session::{Session, SessionModule};
use crate::panels::timeline::TimelineModule;
use crate::panels::viewport::{ViewportMapping, ViewportModule, WaitingForFrame};
use crate::{keys, layout, panels, style, video};

pub struct Shell {
    core: Core,
    epoch: Instant,
    pointer: PointerService,
    /// Time of the last pointer sample handed to the world.
    pointer_read: f64,
    /// Key events kept from egui (Tab: see keys::take_tab), for the keymap.
    taken_keys: Vec<egui::Event>,
    /// A text field had the keyboard at the end of the last UI pass.
    was_typing: bool,
    /// Dev/benchmark: `TT_AUTOPLAY_SECS=N` plays N seconds once the video is
    /// open, logs the playback probe, and quits (spike S2 measurements).
    autoplay: Option<(f64, Option<f64>)>,
    /// Dev/benchmark: `TT_BENCH_STEPS=1` times seeks and frame steps until the
    /// exact frame is on screen, logs a summary, and quits (M1 acceptance).
    bench: Option<StepBench>,
    /// Dev/benchmark: `TT_SKETCH_DEMO=<sprite_truth.json>` sketches the sprite
    /// fixture with a scripted hand, logs the error, and quits (demo.rs).
    sketch_demo: Option<crate::demo::SketchDemo>,
    /// Dev: `TT_SCENE_DEMO=1` builds a scene with one of everything (scene.rs).
    scene: Option<crate::scene::Scene>,
}

/// Seek to a few far-apart frames, then step backward and forward, timing each
/// action until the viewport shows the exact frame.
struct StepBench {
    ops: Vec<(&'static str, tt_core::input::Action)>,
    next: usize,
    pending: Option<(&'static str, f64)>,
    results: Vec<(&'static str, f64)>,
}

impl StepBench {
    fn new() -> Self {
        use tt_core::input::Action::*;
        let mut ops: Vec<(&'static str, tt_core::input::Action)> =
            [30_000, 5_000, 61_234, 42_424, 12_345, 69_000, 777, 33_333].into_iter().map(|f| ("seek", Seek(f))).collect();
        ops.extend(std::iter::repeat_n(("step back", StepBackward), 60));
        ops.extend(std::iter::repeat_n(("step forward", StepForward), 20));
        Self { ops, next: 0, pending: None, results: Vec::new() }
    }

    fn summary(&self) -> String {
        let mut out = String::new();
        for kind in ["seek", "step back", "step forward"] {
            let mut v: Vec<f64> = self.results.iter().filter(|(k, _)| *k == kind).map(|(_, ms)| *ms).collect();
            if v.is_empty() {
                continue;
            }
            v.sort_by(f64::total_cmp);
            let q = |f: f64| v[((v.len() - 1) as f64 * f).round() as usize];
            out.push_str(&format!(
                "\n  {kind:>12}: n={:<3} median {:>6.1} ms | p95 {:>6.1} ms | max {:>6.1} ms",
                v.len(),
                q(0.5),
                q(0.95),
                v[v.len() - 1]
            ));
        }
        out
    }
}

impl Shell {
    /// `ready`: FFmpeg was found (else the setup shows first; `crate::setup`).
    pub fn new(cc: &eframe::CreationContext<'_>, ready: bool) -> Self {
        style::apply(&cc.egui_ctx);
        if let Some(rs) = cc.wgpu_render_state.as_ref() {
            video::VideoRenderer::install(rs);
            // The card's driver can reset it: start again instead of panicking (recover.rs).
            crate::recover::watch(rs, &cc.egui_ctx);
        }
        let mut app = AppBuilder::new();
        // Modules build as they're added, and the session applies the remembered
        // settings to resources that exist by then: everything with a setting
        // (the trackers', the updater's) comes before it.
        app.add_module(CoreModules)
            .add_module(layout::LayoutModule)
            .add_module(MediaModule)
            .add_module(ViewportModule)
            .add_module(TimelineModule)
            .add_module(tt_track::TrackModule)
            .add_module(crate::update::UpdateModule)
            .add_module(crate::rebuild::RebuildModule)
            .add_module(crate::setup::SetupModule)
            .add_module(SessionModule)
            .add_module(crate::project::ProjectModule);
        let mut core = app.build();
        crate::update::on_start(&core.world);
        {
            let mut doctor = core.world.resource_mut::<crate::setup::Doctor>();
            doctor.graphics = cc.wgpu_render_state.as_ref().map(|rs| {
                let i = rs.adapter.get_info();
                let driver = if i.driver.is_empty() { String::new() } else { format!(", driver {} {}", i.driver, i.driver_info) };
                format!("{} ({:?}{driver})", i.name, i.backend)
            });
            doctor.setup = !ready;
            doctor.last_run_failed = crate::setup::last_run_failed() || crate::recover::recovered().is_some();
            if !ready {
                doctor.recheck();
            }
        }

        // Until a video is open, a one-minute demo clock keeps the transport live.
        core.world.resource_mut::<Transport>().frame_count = 60 * 60;
        // `trackertools <video>` opens it on startup; otherwise the last session resumes.
        if let Some(path) = std::env::args_os().nth(1) {
            core.world.resource_mut::<OpenRequest>().0 = Some(path.into());
        } else {
            core.world.resource_scope(|world, mut session: bevy_ecs::world::Mut<Session>| {
                session.restore_into(&mut world.resource_mut::<OpenRequest>());
            });
        }

        core.world.insert_resource(crate::recover::Notice { why: crate::recover::recovered(), paused: 0 });
        if let Some(probe) = crate::input_probe::InputProbe::start() {
            core.world.insert_resource(probe);
        }
        let autoplay = std::env::var("TT_AUTOPLAY_SECS").ok().and_then(|s| s.parse().ok()).map(|s| (s, None));
        let bench = std::env::var_os("TT_BENCH_STEPS").map(|_| StepBench::new());
        let epoch = Instant::now();
        let pointer = PointerService::start(epoch);
        let sketch_demo = crate::demo::SketchDemo::start();
        // A scripted run (autoplay, bench, demo) on a video that isn't there
        // would wait forever for it: say so and quit instead.
        let scripted = autoplay.is_some() || bench.is_some() || sketch_demo.is_some();
        if let Some(path) = std::env::args_os().nth(1).filter(|p| scripted && !std::path::Path::new(p).exists()) {
            tracing::error!("{} not found: nothing to run on (cargo xtask fixtures makes the test clips)", std::path::Path::new(&path).display());
            std::process::exit(2);
        }
        Self { core, epoch, pointer, pointer_read: 0.0, taken_keys: Vec::new(), was_typing: false, autoplay, bench, sketch_demo, scene: crate::scene::Scene::start() }
    }

    /// Setup is done: the window becomes the app's.
    fn start_app(&mut self, ctx: &egui::Context) {
        self.core.world.resource_mut::<crate::setup::Doctor>().setup = false;
        tracing::info!("setup done: starting the app");
        // The app's size, within the screen.
        let monitor = ctx.input(|i| i.viewport().monitor_size);
        let size = monitor.map_or(egui::vec2(1600.0, 950.0), |m| egui::vec2(1600f32.min(m.x * 0.92), 950f32.min(m.y * 0.88)));
        ctx.send_viewport_cmd(egui::ViewportCommand::Title("trackertools".into()));
        ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(egui::vec2(900f32.min(size.x), 560f32.min(size.y))));
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
        if let Some(m) = monitor {
            let at = ((m - size) / 2.0).max(egui::Vec2::ZERO);
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(at.x, at.y)));
        }
        ctx.request_repaint();
    }

    fn drive_bench(&mut self, ctx: &egui::Context, now: f64) {
        let Some(bench) = &mut self.bench else { return };
        let world = &mut self.core.world;
        ctx.request_repaint();
        // The viewport reports (from the previous UI pass) whether the exact frame is shown.
        if world.get_resource::<crate::media::Media>().is_none() || world.resource::<WaitingForFrame>().0 || now < 1.0 {
            return;
        }
        if let Some((kind, t0)) = bench.pending.take() {
            bench.results.push((kind, (now - t0) * 1e3));
        }
        if let Some((kind, action)) = bench.ops.get(bench.next).copied() {
            world.resource_mut::<PendingActions>().push(action);
            bench.pending = Some((kind, now));
            bench.next += 1;
        } else {
            let stats = active_stats(world);
            tracing::info!("step bench (time until the exact frame is on screen):{}\n  decode: {stats:?}", bench.summary());
            self.bench = None;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn drive_autoplay(&mut self, ctx: &egui::Context, now: f64) {
        let Some((secs, started)) = self.autoplay else { return };
        let world = &mut self.core.world;
        if world.get_resource::<crate::media::Media>().is_none() {
            return;
        }
        match started {
            // Give the decoder a second to warm up at frame 0, then play.
            None if now > 1.0 => {
                world.resource_mut::<PendingActions>().push(tt_core::input::Action::TogglePlay);
                self.autoplay = Some((secs, Some(now)));
            }
            Some(t0) if now - t0 >= secs => {
                if world.resource::<Transport>().playing {
                    world.resource_mut::<PendingActions>().push(tt_core::input::Action::TogglePlay);
                } else if now - t0 >= secs + 0.5 {
                    let stats = active_stats(world);
                    tracing::info!("decode stats: {stats:?}");
                    self.autoplay = None;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    return;
                }
            }
            _ => {}
        }
        ctx.request_repaint();
    }
}

impl eframe::App for Shell {
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // The graphics device is lost (the card's driver reset it): painting this
        // frame would panic. Save, start again, exit (recover.rs).
        crate::recover::simulate(ctx, frame);
        crate::recover::check(frame);
        if crate::recover::gpu_lost() {
            crate::recover::after_gpu_loss(&mut self.core.world);
        }
        // The setup comes first: the app waits (a video it would reopen needs FFmpeg).
        if self.core.world.resource::<crate::setup::Doctor>().setup {
            return;
        }
        let now = self.epoch.elapsed().as_secs_f64();
        self.core.world.resource_mut::<WallClock>().tick(now);

        if let Some(path) = ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf())) {
            self.core.world.resource_mut::<OpenRequest>().0 = Some(path);
        }
        let mut frame = crate::pointer::frame(ctx, &self.pointer, &mut self.pointer_read, now, self.core.world.resource::<ViewportMapping>());
        if frame.pressed.is_some() {
            // A press on the video ends any text editing (an Inspector field keeping
            // focus would otherwise swallow Space, Shift and Esc for the whole stroke).
            ctx.memory_mut(|m| {
                if let Some(id) = m.focused() {
                    m.surrender_focus(id);
                }
            });
            self.was_typing = false;
        }
        // A text field focused at the end of the last pass counts too: egui has
        // already let go of it here when Esc (or Enter) ended the edit, and that
        // key belongs to the field, not the keymap.
        let typing = ctx.egui_wants_keyboard_input() || self.was_typing;
        let taken = std::mem::take(&mut self.taken_keys);
        if !typing {
            let keymap = self.core.world.resource::<Keymap>();
            let mut actions = ctx.input(|i| keys::actions(&i.events, keymap));
            actions.extend(keys::actions(&taken, keymap));
            self.core.world.resource_mut::<PendingActions>().0.extend(actions);
        }
        let held = if typing { KeysHeld::default() } else { ctx.input(keys::held) };
        if let Some(demo) = &mut self.sketch_demo {
            ctx.request_repaint();
            save_screenshots(ctx);
            let done = demo.drive(&mut self.core.world, now, &mut frame);
            if let Some(name) = demo.shot.take() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(name.to_string())));
            }
            if done {
                self.sketch_demo = None;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        *self.core.world.resource_mut::<KeysHeld>() = held;
        *self.core.world.resource_mut::<PointerFrame>() = frame;
        self.drive_autoplay(ctx, now);
        self.drive_bench(ctx, now);
        if let Some(mut probe) = self.core.world.get_resource_mut::<crate::input_probe::InputProbe>() {
            let moves = ctx.input(|i| i.events.iter().filter(|e| matches!(e, egui::Event::PointerMoved(_))).count());
            let ppp = ctx.pixels_per_point();
            let mapping = ctx.input(|i| i.viewport().inner_rect.zip(i.pointer.latest_pos())).and_then(|(inner, egui_pos)| {
                let s = *self.pointer.recent(0.0).last()?;
                Some(egui::pos2(s.x as f32 / ppp - inner.min.x, s.y as f32 / ppp - inner.min.y) - egui_pos)
            });
            probe.frame(now, moves as u32, &self.pointer, mapping);
            ctx.request_repaint();
        }

        self.core.run_pre_ui();
        if let Some(scene) = &mut self.scene
            && self.core.world.get_resource::<tt_track::runner::Footage>().is_some()
        {
            ctx.request_repaint();
            if scene.drive(&mut self.core.world) {
                self.scene = None;
            }
        }

        // Trackers: results arrive from background threads, and a re-plan may
        // be waiting (inputs settling, a drag just ended). Keep frames coming.
        if self.core.world.get_resource::<tt_track::runner::TrackJobs>().is_some_and(|j| j.active()) {
            ctx.request_repaint_after(Duration::from_millis(33));
        }
        // Playing, or sketching while frozen: samples and the clock map keep flowing.
        if self.core.world.resource::<Transport>().playing || self.core.world.resource::<tt_core::capture::LiveCapture>().0.is_some() {
            ctx.request_repaint();
        }
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        keys::take_tab(ctx, raw_input, &mut self.taken_keys);
        // The sketch demo's (and the scene demo's) scripted UI input.
        if let Some(demo) = &mut self.sketch_demo {
            raw_input.events.append(&mut demo.inject);
        }
        if let Some(scene) = &mut self.scene {
            raw_input.events.append(&mut scene.inject);
        }
    }

    fn on_exit(&mut self) {
        crate::project::save_if_dirty(&mut self.core.world);
        self.core.world.resource_mut::<Session>().save();
        // Saved: if an update was just installed, start the new version.
        crate::update::restart_if_updated(&self.core.world);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.core.world.resource::<crate::setup::Doctor>().setup {
            let start = crate::setup::screen(ui, &mut self.core.world.resource_mut::<crate::setup::Doctor>());
            if start {
                self.start_app(ui.ctx());
            }
            return;
        }
        panels::draw(ui, &mut self.core.world);
        if crate::recover::notice(ui.ctx(), &mut self.core.world) {
            let mut doctor = self.core.world.resource_mut::<crate::setup::Doctor>();
            (doctor.open, doctor.last_run_failed) = (true, false);
            doctor.recheck();
        }
        self.was_typing = ui.ctx().egui_wants_keyboard_input();

        let had_actions = !self.core.world.resource::<PendingActions>().0.is_empty();
        self.core.run_post_ui();
        if had_actions {
            // The panels drew the state from before the gesture; show the result.
            ui.ctx().request_repaint();
        }
        if self.core.world.resource::<WaitingForFrame>().0 {
            ui.ctx().request_repaint_after(Duration::from_millis(8));
        }
    }
}

/// Screenshots the window delivered (the sketch demo asks for them), saved as
/// PNGs under `<data dir>/screens/`.
fn save_screenshots(ctx: &egui::Context) {
    let shots: Vec<(String, std::sync::Arc<egui::ColorImage>)> = ctx.input(|i| {
        i.raw
            .events
            .iter()
            .filter_map(|e| match e {
                egui::Event::Screenshot { image, user_data, .. } => {
                    let name = user_data.data.as_ref().and_then(|d| d.downcast_ref::<String>()).cloned().unwrap_or_else(|| "shot".into());
                    Some((name, image.clone()))
                }
                _ => None,
            })
            .collect()
    });
    for (name, image) in shots {
        let dir = tt_media::proxy::data_dir().join("screens");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{name}.png"));
        let result = (|| -> anyhow::Result<()> {
            let file = std::io::BufWriter::new(std::fs::File::create(&path)?);
            let mut enc = png::Encoder::new(file, image.size[0] as u32, image.size[1] as u32);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let bytes: Vec<u8> = image.pixels.iter().flat_map(|c| c.to_array()).collect();
            enc.write_header()?.write_image_data(&bytes)?;
            Ok(())
        })();
        match result {
            Ok(()) => tracing::info!("screenshot saved: {}", path.display()),
            Err(e) => tracing::warn!("screenshot {}: {e:#}", path.display()),
        }
    }
}

/// Decode stats of the rendition the viewport is showing.
fn active_stats(world: &bevy_ecs::world::World) -> tt_media::PlayerStats {
    let media = world.resource::<crate::media::Media>();
    let which = world.resource::<crate::media::ActiveSource>().0;
    media.source(which).unwrap_or(&media.original).player.stats()
}
