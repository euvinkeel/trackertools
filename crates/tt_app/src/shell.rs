//! The eframe host: owns the world and runs one app frame.
//!
//! eframe calls `logic` before every UI pass (also while the window is hidden),
//! so the wall clock, key → action translation and the PreUi schedule live
//! there. `ui` draws panels from the world, then runs PostUi so panel gestures
//! (scrubbing) apply before the frame is presented.

use std::time::{Duration, Instant};

use tt_core::input::{Keymap, PendingActions};
use tt_core::time::WallClock;
use tt_core::transport::Transport;
use tt_core::{AppBuilder, Core, CoreModules};

use crate::media::{MediaModule, OpenRequest};
use crate::session::{Session, SessionModule};
use crate::panels::timeline::TimelineModule;
use crate::panels::viewport::{ViewportModule, WaitingForFrame};
use crate::{keys, layout, panels, style, video};

pub struct Shell {
    core: Core,
    epoch: Instant,
    /// Dev/benchmark: `TT_AUTOPLAY_SECS=N` plays N seconds once the video is
    /// open, logs the playback probe, and quits (spike S2 measurements).
    autoplay: Option<(f64, Option<f64>)>,
    /// Dev/benchmark: `TT_BENCH_STEPS=1` times seeks and frame steps until the
    /// exact frame is on screen, logs a summary, and quits (M1 acceptance).
    bench: Option<StepBench>,
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
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        style::apply(&cc.egui_ctx);
        if let Some(rs) = cc.wgpu_render_state.as_ref() {
            video::VideoRenderer::install(rs);
        }
        let mut app = AppBuilder::new();
        app.add_module(CoreModules)
            .add_module(layout::LayoutModule)
            .add_module(MediaModule)
            .add_module(ViewportModule)
            .add_module(TimelineModule)
            .add_module(SessionModule)
            .add_module(crate::project::ProjectModule);
        let mut core = app.build();

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

        if let Some(probe) = crate::input_probe::InputProbe::start() {
            core.world.insert_resource(probe);
        }
        let autoplay = std::env::var("TT_AUTOPLAY_SECS").ok().and_then(|s| s.parse().ok()).map(|s| (s, None));
        let bench = std::env::var_os("TT_BENCH_STEPS").map(|_| StepBench::new());
        Self { core, epoch: Instant::now(), autoplay, bench }
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
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let now = self.epoch.elapsed().as_secs_f64();
        self.core.world.resource_mut::<WallClock>().tick(now);

        if let Some(path) = ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf())) {
            self.core.world.resource_mut::<OpenRequest>().0 = Some(path);
        }
        if !ctx.egui_wants_keyboard_input() {
            let actions = ctx.input(|i| keys::actions(i, self.core.world.resource::<Keymap>()));
            self.core.world.resource_mut::<PendingActions>().0.extend(actions);
        }
        self.drive_autoplay(ctx, now);
        self.drive_bench(ctx, now);
        if let Some(mut probe) = self.core.world.get_resource_mut::<crate::input_probe::InputProbe>() {
            let moves = ctx.input(|i| i.events.iter().filter(|e| matches!(e, egui::Event::PointerMoved(_))).count());
            probe.frame(now, moves as u32);
            ctx.request_repaint();
        }

        self.core.run_pre_ui();

        if self.core.world.resource::<Transport>().playing {
            ctx.request_repaint();
        }
    }

    fn on_exit(&mut self) {
        crate::project::save_if_dirty(&mut self.core.world);
        self.core.world.resource_mut::<Session>().save();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        panels::draw(ui, &mut self.core.world);

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

/// Decode stats of the rendition the viewport is showing.
fn active_stats(world: &bevy_ecs::world::World) -> tt_media::PlayerStats {
    let media = world.resource::<crate::media::Media>();
    let which = world.resource::<crate::media::ActiveSource>().0;
    media.source(which).unwrap_or(&media.original).player.stats()
}
