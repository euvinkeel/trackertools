//! Starting again after an error that ends the app (DESIGN §12.1).
//!
//! Two errors end a running trackertools: the graphics card's driver
//! resetting the card (Windows does that when the card stops answering for
//! 2 s, for example under heavy CoTracker work), and a panic on the main
//! thread. After a reset the app's wgpu device is lost: eframe cannot make a
//! new one, and painting with it panics inside egui-wgpu.
//!
//! - **The loss is found between frames** (a submit or a present finds it,
//!   or [`check`]'s poll at the start of every frame): wgpu calls the
//!   device-lost callback, and `Shell::logic`, before anything is painted,
//!   saves the project and the session (at the current frame), starts
//!   trackertools again and exits ([`after_gpu_loss`]).
//! - **The loss is found while painting** (egui-wgpu's buffers for the frame,
//!   which is where a reset usually shows), or **a panic on the main
//!   thread**: the callback and the panic come in the same call, with no
//!   `logic` between. The panic hook can't save (the world may be half
//!   changed), so the last autosave counts: edits of the last seconds, and
//!   tracker results of the last few seconds, can be missing. It starts
//!   trackertools again and exits ([`after_panic`]).
//!
//! The new process waits until the old one has exited ([`wait_for_old`]),
//! opens the last video at its frame with its project (as any start does;
//! CoTracker trackers are paused when a project opens), and says what
//! happened ([`notice`]). Not twice in 5 minutes, and not in the first
//! seconds of a run (an error at every start would loop): then the message
//! box of before. Release builds only (or `TT_RECOVER_TEST=1`); never in
//! scripted runs.
//!
//! Dev: `TT_SIMULATE_GPU_LOSS=<seconds>[,panic]` destroys the app's wgpu
//! device that many seconds after the start: the same lost device a reset
//! makes, without touching the card. Plain, it notes the loss itself (the
//! saving path); `,panic` doesn't, so egui-wgpu's panic is what recovers.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use bevy_ecs::prelude::*;
use eframe::egui_wgpu::RenderState;
use eframe::wgpu;

use crate::style;

/// The variable that tells the new process why it was started.
pub const RECOVERED: &str = "TT_RECOVERED";
/// The old process's id, for the new one to wait for.
const RECOVERED_FROM: &str = "TT_RECOVERED_FROM";
/// Starts the log line of a lost device (the next start offers the report).
pub const GPU_LOST_MARK: &str = "GPU LOST: ";
/// What the last recovery did, in the logs folder (and in the report).
pub const BREADCRUMB: &str = "recovery.json";
/// No new start if the last one was this recent (seconds).
const AGAIN_AFTER: u64 = 5 * 60;
/// Nor in a run younger than this: an error at every start would loop.
const MIN_UPTIME: Duration = Duration::from_secs(10);
/// How long the new process waits for the old one to exit.
const WAIT_FOR_OLD: Duration = Duration::from_secs(10);

static STARTED: OnceLock<Instant> = OnceLock::new();
static GPU_LOST: AtomicBool = AtomicBool::new(false);
static GPU_WHY: Mutex<String> = Mutex::new(String::new());
static RESTARTED: AtomicBool = AtomicBool::new(false);

/// Why trackertools started again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// The graphics card's driver reset the card; everything was saved.
    Gpu,
    /// The card was reset while painting: the last autosave counts.
    GpuUnsaved,
    /// An error (a panic) on the main thread: the last autosave counts.
    Panic,
}

impl Why {
    pub fn word(self) -> &'static str {
        match self {
            Why::Gpu => "gpu",
            Why::GpuUnsaved => "gpu-unsaved",
            Why::Panic => "panic",
        }
    }

    fn from_word(w: &str) -> Option<Self> {
        [Why::Gpu, Why::GpuUnsaved, Why::Panic].into_iter().find(|why| why.word() == w)
    }
}

/// This process was started by a recovery (`TT_RECOVERED`), and why.
pub fn recovered() -> Option<Why> {
    std::env::var(RECOVERED).ok().and_then(|w| Why::from_word(&w))
}

/// At the very start of `main` (before the log is opened): note the time,
/// and, in a process a recovery started, wait (up to 10 s) for the old one
/// to exit.
pub fn wait_for_old() {
    STARTED.get_or_init(Instant::now);
    if let Some(pid) = std::env::var(RECOVERED_FROM).ok().and_then(|p| p.parse().ok()) {
        wait_for_exit(pid, WAIT_FOR_OLD);
    }
}

#[cfg(windows)]
fn wait_for_exit(pid: u32, timeout: Duration) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
    // SAFETY: a handle we open, wait on and close; a null one (the process is gone) is skipped.
    unsafe {
        let process = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if !process.is_null() {
            WaitForSingleObject(process, timeout.as_millis() as u32);
            CloseHandle(process);
        }
    }
}

#[cfg(not(windows))]
fn wait_for_exit(_pid: u32, timeout: Duration) {
    std::thread::sleep(timeout.min(Duration::from_secs(1)));
}

/// Watch the app's wgpu device (from `Shell::new`): its loss is noted for the
/// next frame. A wgpu error is logged instead of panicking (wgpu's default):
/// a validation error leaves out what failed and the app goes on; running out
/// of memory, or an internal error, counts as a loss.
pub fn watch(rs: &RenderState, ctx: &egui::Context) {
    let lost = ctx.clone();
    rs.device.set_device_lost_callback(move |reason, message| {
        // (Called inside the wgpu call that found the loss: note it, nothing more.)
        note_gpu_lost(format!("{reason:?}: {message}"));
        lost.request_repaint();
    });
    let errors = ctx.clone();
    rs.device.on_uncaptured_error(std::sync::Arc::new(move |e| match e {
        wgpu::Error::OutOfMemory { .. } | wgpu::Error::Internal { .. } => {
            note_gpu_lost(format!("{e}"));
            errors.request_repaint();
        }
        _ => {
            // (One a frame would flood the log: the first few, then one in a thousand.)
            static SEEN: AtomicU32 = AtomicU32::new(0);
            let n = SEEN.fetch_add(1, Ordering::Relaxed) + 1;
            if n <= 5 || n.is_multiple_of(1000) {
                tracing::error!("graphics error {n} (trackertools goes on): {e}");
            }
        }
    }));
}

/// Every `Shell::logic`, first: ask the device whether it still works (a
/// non-blocking poll reads its fence; a reset device fails it and calls the
/// callback). A reset while the app waited between frames is then found
/// here, where the world can still be saved, not in the next paint.
pub fn check(frame: &eframe::Frame) {
    if let Some(rs) = frame.wgpu_render_state() {
        let _ = rs.device.poll(wgpu::PollType::Poll);
    }
}

/// The app's wgpu device is lost (`why`: wgpu's words).
pub fn note_gpu_lost(why: String) {
    tracing::error!("{GPU_LOST_MARK}the graphics device is lost ({why})");
    *GPU_WHY.lock().unwrap_or_else(|e| e.into_inner()) = why;
    GPU_LOST.store(true, Ordering::SeqCst);
}

/// The device is lost: the next painted frame would panic.
pub fn gpu_lost() -> bool {
    GPU_LOST.load(Ordering::SeqCst)
}

fn gpu_why() -> String {
    GPU_WHY.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Runs where a recovery may start trackertools again: a release build (or
/// `TT_RECOVER_TEST=1`), not a scripted run, not `TT_NO_RECOVER`.
fn allowed() -> bool {
    let var = |v: &str| std::env::var_os(v).is_some();
    let scripted = ["TT_SKETCH_DEMO", "TT_BENCH_STEPS", "TT_SCENE_DEMO", "TT_AUTOPLAY_SECS", "TT_SETUP_AUTO", "TT_INPUT_PROBE"].iter().any(|v| var(v));
    (!cfg!(debug_assertions) || var("TT_RECOVER_TEST")) && !scripted && !var("TT_NO_RECOVER")
}

fn breadcrumb() -> PathBuf {
    crate::setup::logs_dir().join(BREADCRUMB)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// When trackertools last started itself again (the breadcrumb keeps it
/// through refusals).
fn last_restart_at() -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(breadcrumb()).ok()?).ok()?;
    v["last_restart_at"].as_u64()
}

/// Whether a new start is allowed now: [`allowed`], once per process, after
/// [`MIN_UPTIME`], and no new start in the last [`AGAIN_AFTER`] seconds (by
/// the breadcrumb, and, should it be unwritable, by this process's own age
/// if a recovery started it).
fn may_restart(at: u64) -> bool {
    let age = STARTED.get().map_or(Duration::ZERO, Instant::elapsed);
    let recent = last_restart_at().is_some_and(|last| at.saturating_sub(last) < AGAIN_AFTER) || recovered().is_some() && age.as_secs() < AGAIN_AFTER;
    allowed() && age >= MIN_UPTIME && !recent && !RESTARTED.load(Ordering::SeqCst)
}

/// Start trackertools again (no file argument: the session resumes) and say
/// why in `TT_RECOVERED`; write the breadcrumb either way. True if the new
/// process started: this one must exit now.
fn restart(why: Why, message: &str) -> bool {
    let at = unix_now();
    let go = may_restart(at);
    let started = go && !RESTARTED.swap(true, Ordering::SeqCst) && {
        let spawned = std::env::current_exe().and_then(|exe| {
            Command::new(exe)
                .env(RECOVERED, why.word())
                .env(RECOVERED_FROM, std::process::id().to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
        });
        match spawned {
            Ok(child) => {
                tracing::info!("started trackertools again (pid {}) after: {message}", child.id());
                true
            }
            Err(e) => {
                tracing::error!("could not start trackertools again: {e}");
                false
            }
        }
    };
    let last = if started { Some(at) } else { last_restart_at() };
    let crumb = serde_json::json!({ "reason": why.word(), "at": at, "message": message, "restarted": started, "last_restart_at": last, "version": crate::update::version() });
    let _ = std::fs::write(breadcrumb(), serde_json::to_vec_pretty(&crumb).unwrap_or_default());
    if !started {
        tracing::warn!("not starting trackertools again ({})", if go { "it could not start" } else { "not allowed now: a recent restart, a young run, or a dev or scripted run" });
    }
    started
}

/// The device is lost and the world is intact (`Shell::logic`, before
/// painting): save the project and the session, start again, exit. Without a
/// new start, a message box says to start it again.
pub fn after_gpu_loss(world: &mut World) -> ! {
    let why = gpu_why();
    // Results the trackers drained aren't an edit: count them as a change so they are saved too.
    world.resource_mut::<tt_core::history::History>().touch();
    crate::project::save_if_dirty(world);
    // (The shown frame only once a video is open: before, the remembered one stays.)
    let frame = world.get_resource::<crate::media::Media>().map(|_| world.resource::<tt_core::transport::Transport>().frame());
    let mut session = world.resource_mut::<crate::session::Session>();
    match frame {
        Some(f) => session.save_at(f),
        None => session.save(),
    }
    if !restart(Why::Gpu, &format!("the graphics device was lost ({why})")) && !cfg!(debug_assertions) {
        crate::setup::message(GPU_STOPPED);
    }
    std::process::exit(0);
}

/// From the panic hook, for a panic on the main thread: start again (the
/// last autosave counts). True if the new process started: exit now.
pub fn after_panic(info: &std::panic::PanicHookInfo<'_>) -> bool {
    let why = if gpu_panic(info) { Why::GpuUnsaved } else { Why::Panic };
    restart(why, &format!("{info}"))
}

/// A panic from the graphics (egui-wgpu or wgpu), or after the device was lost.
pub fn gpu_panic(info: &std::panic::PanicHookInfo<'_>) -> bool {
    gpu_lost() || info.location().is_some_and(|l| l.file().contains("wgpu"))
}

/// The message box when the card reset and trackertools could not start again (ASD-STE100).
pub const GPU_STOPPED: &str = "The graphics card stopped for a short time. trackertools must close.\n\nStart trackertools again. It opens your video and your project. Changes from the last seconds can be missing.\n\nIf this occurs again, click Report a problem at the top of the window.";

// ------------------------------------------------------------ dev simulation

/// `TT_SIMULATE_GPU_LOSS=<seconds>[,panic]`: (seconds, through the panic path).
fn simulation() -> Option<(f64, bool)> {
    static SIM: OnceLock<Option<(f64, bool)>> = OnceLock::new();
    *SIM.get_or_init(|| {
        let v = std::env::var("TT_SIMULATE_GPU_LOSS").ok()?;
        let mut parts = v.split(',');
        let secs = parts.next()?.trim().parse().ok()?;
        Some((secs, parts.next().is_some_and(|p| p.trim() == "panic")))
    })
}

/// Dev: destroy the device once the time has come (every `logic`; an idle
/// window is woken for it).
pub fn simulate(ctx: &egui::Context, frame: &eframe::Frame) {
    static DONE: AtomicBool = AtomicBool::new(false);
    let Some((secs, through_panic)) = simulation() else { return };
    let Some(age) = STARTED.get().map(|t| t.elapsed().as_secs_f64()) else { return };
    if DONE.load(Ordering::SeqCst) {
        return;
    }
    if age < secs {
        ctx.request_repaint_after(Duration::from_secs_f64(secs - age));
        return;
    }
    if let Some(rs) = frame.wgpu_render_state() {
        DONE.store(true, Ordering::SeqCst);
        tracing::warn!("TT_SIMULATE_GPU_LOSS: destroying the graphics device{}", if through_panic { " (the panic path recovers)" } else { "" });
        rs.device.destroy();
        // (destroy() doesn't call the callback until the queue is polled.)
        if !through_panic {
            note_gpu_lost("simulated (TT_SIMULATE_GPU_LOSS)".into());
        }
    }
}

// --------------------------------------------------------- the new process

/// The new process says what happened, until the person closes it.
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct Notice {
    pub why: Option<Why>,
    /// CoTracker trackers paused when the project opened (`crate::project`).
    pub paused: usize,
}

/// What the notice says (ASD-STE100).
pub fn notice_text(why: Why) -> &'static str {
    match why {
        Why::Gpu => "The graphics card stopped for a short time, and trackertools started again. Your video and your project are open at the frame where you stopped.",
        Why::GpuUnsaved => {
            "The graphics card stopped for a short time, and trackertools started again. Your video and your project are open. Changes from the last seconds before the stop can be missing. Trackers can track some frames again."
        }
        Why::Panic => {
            "trackertools stopped because of an error, and started again. Your video and your project are open. Changes from the last seconds before the error can be missing.\n\nIf this occurs again, click Report a problem. Then send the report to the person who gave you trackertools."
        }
    }
}

/// The notice's words about CoTracker trackers paused on open (ASD-STE100).
fn paused_text(paused: usize) -> String {
    let which = if paused == 1 { "1 CoTracker tracker is paused".to_string() } else { format!("{paused} CoTracker trackers are paused") };
    let turns = match tt_track::runner::cotracker_jobs() {
        1 => " One CoTracker tracker tracks at a time.".to_string(),
        n => format!(" {n} CoTracker trackers track at a time."),
    };
    format!("{which}. To start one again, select it. Then click Back, Both or Forward.{turns}")
}

/// The notice window (after the panels). True: the person asked for the report.
pub fn notice(ctx: &egui::Context, world: &mut World) -> bool {
    let Some(Notice { why: Some(why), paused }) = world.get_resource::<Notice>().copied() else { return false };
    let (mut close, mut report) = (false, false);
    egui::Window::new(egui::RichText::new("\u{26a0} trackertools started again").color(style::LIVE))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 48.0))
        .default_width(440.0)
        .show(ctx, |ui| {
            ui.set_max_width(440.0);
            ui.label(notice_text(why));
            if paused > 0 {
                ui.add_space(4.0);
                ui.label(paused_text(paused));
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                close = ui.button("OK").clicked();
                report = ui.button("Report a problem").clicked();
            });
        });
    if close || report {
        world.resource_mut::<Notice>().why = None;
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reason_goes_through_the_variable_and_back() {
        for why in [Why::Gpu, Why::GpuUnsaved, Why::Panic] {
            assert_eq!(Why::from_word(why.word()), Some(why));
        }
        assert_eq!(Why::from_word("other"), None);
    }
}
