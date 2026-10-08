//! First-run setup and the doctor. trackertools reads and writes video with
//! FFmpeg (`ffmpeg` and `ffprobe`), which doesn't come with it. When they
//! can't be found, a small setup window comes before the app: it installs
//! FFmpeg in a folder the person chooses (gyan.dev's Windows "essentials"
//! build, checked against its SHA-256, unpacked with Windows' own curl and
//! tar), or takes the folder of one they have; then the app starts.
//!
//! The doctor (Settings) shows the same checks, and makes a report to send
//! to whoever helps: the checks, this computer, and the end of this run's
//! and the last run's logs (`logs/` in the data folder, written from here).
//! A panic is logged and, on the next start, the top bar offers the report.
//!
//! Every instruction shown follows ASD-STE100 (Simplified Technical
//! English): short sentences, simple present tense, one instruction each,
//! no contractions. The people reading it skip anything longer.
//!
//! At its top, one big button does all of it: JUST DO EVERYTHING FOR ME PLZ
//! (the label is the user's) installs FFmpeg in the folder shown if it isn't
//! there, waits for the checks, and starts the app, saying each step in a
//! box under it (the explanations in STE like the rest).
//!
//! Dev: `TT_SETUP_AUTO=<seconds>` presses that button by itself once the
//! checks are done, its countdown to the start that many seconds (scripted
//! checks of the setup; `TT_FFMPEG_URL` points it at a local zip).

use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bevy_ecs::prelude::*;
use sha2::{Digest, Sha256};
use tt_core::{AppBuilder, Class, Module};

use crate::style;
use crate::update::{Problem, quiet, system_tool};

/// FFmpeg for Windows: the latest release's "essentials" build from
/// gyan.dev (where ffmpeg.org points Windows users), its SHA-256 at the
/// same address + `.sha256`. `TT_FFMPEG_URL` replaces it (tests).
pub const FFMPEG_ZIP: &str = "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip";

const LOG: &str = "trackertools.log";
const PREVIOUS_LOG: &str = "trackertools-previous.log";
/// Starts a panic's line in the log (the next start looks for it).
const PANIC: &str = "PANIC: ";

const CANNOT_WRITE: &str = "trackertools cannot write to this folder. Select a different folder.";
const STOPPED: &str = "The download stopped. Make sure that the computer is connected to the internet. Then click Install FFmpeg again.";
const CANNOT_CHECK: &str = "trackertools cannot check the download. Click Install FFmpeg again.";
const DOES_NOT_START: &str = "FFmpeg does not start. Click Copy report. Send the report to the person who gave you trackertools.";
const ASK_FOR_HELP: &str = "If you have a problem, click Copy report. Then paste the report in a message to the person who gave you trackertools.";

// ---------------------------------------------------------------- logging

pub fn logs_dir() -> PathBuf {
    tt_media::proxy::data_dir().join("logs")
}

/// Logs to stdout and to `logs/trackertools.log` in the data folder (the
/// last run's log kept beside it), and logs panics (in a release, a panic
/// on the main thread also says what to do in a message box).
pub fn start_logging() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let dir = logs_dir();
    let _ = std::fs::create_dir_all(&dir);
    let log = dir.join(LOG);
    if log.exists() {
        let _ = std::fs::rename(&log, dir.join(PREVIOUS_LOG));
    }
    let file = std::fs::File::create(&log).ok().map(|f| tracing_subscriber::fmt::layer().with_ansi(false).with_writer(Mutex::new(f)));
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,wgpu_core=warn,wgpu_hal=warn,naga=warn".into());
    tracing_subscriber::registry().with(filter).with(tracing_subscriber::fmt::layer()).with(file).init();
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let name = std::thread::current().name().unwrap_or("unnamed").to_string();
        if name.starts_with("tracker ") {
            // (A tracker's job catches it and reports it as its error: the app goes on.)
            tracing::error!("panic in the thread {name:?} (the app goes on): {info}\n{}", std::backtrace::Backtrace::force_capture());
        } else {
            tracing::error!("{PANIC}{info}\n{}", std::backtrace::Backtrace::force_capture());
        }
        if name == "main" {
            // The app ends: start it again where it was (crate::recover), else say what to do.
            if crate::recover::after_panic(info) {
                std::process::exit(3);
            }
            if !cfg!(debug_assertions) {
                message(if crate::recover::gpu_panic(info) {
                    crate::recover::GPU_STOPPED
                } else {
                    "trackertools stopped because of an error.\n\nStart trackertools again. Then click Report a problem at the top of the window."
                });
            }
        }
        default(info);
    }));
}

/// The last run stopped on an error (a panic, or the graphics device lost, in its log).
pub fn last_run_failed() -> bool {
    std::fs::read_to_string(logs_dir().join(PREVIOUS_LOG)).is_ok_and(|t| t.contains(PANIC) || t.contains(crate::recover::GPU_LOST_MARK))
}

/// A message box, when there is no window of ours to say it in.
pub fn message(text: &str) {
    let _ = rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title("trackertools")
        .set_description(text)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
}

/// trackertools couldn't open its window (`why`: from eframe): the report goes
/// in the data folder, and a message box says where.
pub fn cannot_start(why: &str) {
    tracing::error!("cannot start: {why}");
    let path = tt_media::proxy::data_dir().join("trackertools-report.txt");
    let saved = std::fs::write(&path, report(&run_checks(None), None)).is_ok();
    let place = if saved { format!("\n\nThe report is in {}.", path.display()) } else { String::new() };
    message(&format!(
        "trackertools cannot start its graphics.\n\nUpdate the graphics driver. Then start trackertools again. If the problem continues, send the report to the person who gave you trackertools.{place}\n\n({why})"
    ));
}

// ------------------------------------------------------- where FFmpeg is

/// Where setup puts FFmpeg unless the person chooses another folder.
pub fn default_dir() -> PathBuf {
    tt_media::proxy::data_dir().join("ffmpeg")
}

fn location_file() -> PathBuf {
    tt_media::proxy::data_dir().join("ffmpeg-location.txt")
}

/// The FFmpeg folder: the one chosen at setup, else the default one.
pub fn chosen_dir() -> PathBuf {
    std::fs::read_to_string(location_file()).ok().map(|s| PathBuf::from(s.trim())).filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(default_dir)
}

/// At start: FFmpeg is looked for in the chosen folder first.
pub fn apply_location() {
    let dir = chosen_dir();
    for name in ["ffmpeg", "ffprobe"] {
        let _ = std::fs::remove_file(dir.join(exe(name)).with_extension("old"));
    }
    tt_media::ffmpeg::set_dir(Some(dir));
}

/// `dir` is the FFmpeg folder from now on (and at the next start).
fn remember(dir: &Path) -> std::io::Result<()> {
    let file = location_file();
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(file, dir.display().to_string())?;
    tt_media::ffmpeg::set_dir(Some(dir.to_path_buf()));
    Ok(())
}

fn exe(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

/// The folder with ffmpeg and ffprobe in `dir`: itself, or its `bin` (an unpacked build).
pub fn ffmpeg_folder(dir: &Path) -> Option<PathBuf> {
    [dir.to_path_buf(), dir.join("bin")].into_iter().find(|d| d.join(exe("ffmpeg")).is_file() && d.join(exe("ffprobe")).is_file())
}

/// The program trackertools runs as `name` (ffmpeg, ffprobe), and its version, if it starts.
pub fn tool_version(name: &str) -> Result<(PathBuf, String), String> {
    let path = tt_media::ffmpeg::tool(name, &name.to_uppercase());
    let out = quiet(path.clone()).args(["-hide_banner", "-version"]).output().map_err(|e| format!("{}: {e}", path.display()))?;
    let first = String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or_default().to_string();
    if !out.status.success() || !first.contains("version") {
        return Err(format!("{}: {first}", path.display()));
    }
    Ok((path, version_of(&first)))
}

/// `ffmpeg version 7.0.1-essentials_build-www.gyan.dev Copyright…` → `7.0.1`
/// (a build without a release number keeps its name, e.g. `N-117500-g0b3c`).
pub fn version_of(line: &str) -> String {
    let Some(v) = line.split_whitespace().skip_while(|w| *w != "version").nth(1) else { return line.trim().to_string() };
    let number: String = v.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    if number.contains('.') { number.trim_end_matches('.').to_string() } else { v.to_string() }
}

/// FFmpeg and ffprobe are there and start.
pub fn ready() -> bool {
    tool_version("ffmpeg").is_ok() && tool_version("ffprobe").is_ok()
}

fn writable(dir: &Path) -> bool {
    let probe = dir.join(".trackertools-write-test");
    let ok = std::fs::create_dir_all(dir).is_ok() && std::fs::write(&probe, b"ok").is_ok();
    let _ = std::fs::remove_file(probe);
    ok
}

// ------------------------------------------------------------------ checks

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Pass,
    Info,
    Warn,
    Fail,
}

/// One thing the doctor looked at, said in a sentence.
#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    pub level: Level,
    pub text: String,
}

impl Check {
    fn new(level: Level, text: impl Into<String>) -> Self {
        Self { level, text: text.into() }
    }

    fn tag(&self) -> &'static str {
        match self.level {
            Level::Pass => "OK",
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Fail => "FAIL",
        }
    }
}

/// What the doctor found: the checks, and whether FFmpeg and ffprobe work
/// (the app needs nothing else to start).
#[derive(Clone, Debug, PartialEq)]
pub struct Checked {
    pub checks: Vec<Check>,
    pub ready: bool,
    /// The graphics CoTracker can use (an NVIDIA card, or a Mac's Apple silicon), if there is one.
    pub gpu: Option<crate::cotracker::Gpu>,
    /// Windows' Visual C++ runtime, for CoTracker's PyTorch (looked at with an NVIDIA card, on Windows).
    pub runtime: Option<crate::cotracker::Runtime>,
}

/// Everything the doctor looks at. `graphics`: the graphics adapter, from the window.
pub fn run_checks(graphics: Option<&str>) -> Checked {
    use Level::*;
    let mut checks = Vec::new();
    let ffmpeg = tool_version("ffmpeg");
    let ffprobe = tool_version("ffprobe");
    for (name, found) in [("FFmpeg", &ffmpeg), ("ffprobe", &ffprobe)] {
        checks.push(match found {
            Ok((path, v)) => Check::new(Pass, format!("{name} {v} is installed: {}.", path.display())),
            Err(_) => Check::new(Fail, format!("{name} is not installed.")),
        });
    }
    if let Ok((path, _)) = &ffmpeg {
        let list = |what: &str| quiet(path.clone()).args(["-hide_banner", what]).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
        let has = |text: &str, codec: &str| text.lines().any(|l| l.split_whitespace().nth(1) == Some(codec));
        let encoders = list("-encoders");
        let cannot: Vec<&str> = [("prores_ks", "ProRes"), ("dnxhd", "DNxHR"), ("libx264", "H.264"), ("aac", "AAC sound"), ("pcm_s16le", "PCM sound")]
            .into_iter()
            .filter(|(c, _)| !has(&encoders, c))
            .map(|(_, n)| n)
            .collect();
        checks.push(if cannot.is_empty() {
            Check::new(Pass, "Export can write ProRes, DNxHR and H.264 video.")
        } else {
            Check::new(Warn, format!("Export cannot write {}. Install FFmpeg again.", cannot.join(", ")))
        });
        let decoders = list("-decoders");
        let unread: Vec<&str> = [("h264", "H.264"), ("hevc", "HEVC (H.265)")].into_iter().filter(|(c, _)| !has(&decoders, c)).map(|(_, n)| n).collect();
        checks.push(if unread.is_empty() {
            Check::new(Pass, "FFmpeg can read H.264 and HEVC video.")
        } else {
            Check::new(Warn, format!("FFmpeg cannot read {} video.", unread.join(", ")))
        });
    }
    checks.push(match graphics {
        Some(g) if ["Basic Render", "llvmpipe", "SwiftShader", "WARP"].iter().any(|s| g.contains(s)) => {
            Check::new(Warn, format!("Graphics: {g}. The video can be slow. Update the graphics driver."))
        }
        Some(g) => Check::new(Pass, format!("Graphics: {g}.")),
        None => Check::new(Info, "Graphics: not known."),
    });
    let data = tt_media::proxy::data_dir();
    checks.push(if writable(&data) {
        Check::new(Pass, format!("trackertools can save its files in {}.", data.display()))
    } else {
        Check::new(Fail, format!("trackertools cannot save its files in {}. Make sure that the folder is not read-only.", data.display()))
    });
    if cfg!(windows) {
        let have = |n: &str| system_tool(n).is_file();
        checks.push(if have("curl") && have("tar") {
            Check::new(Pass, "Updates can download and install.")
        } else {
            Check::new(Warn, "Updates cannot download, because curl or tar is not on this computer.")
        });
    }
    let gpu = crate::cotracker::gpu();
    checks.push(match &gpu {
        Some(g) if g.apple => Check::new(Pass, format!("Apple silicon: {} ({}). CoTracker can use its graphics.", g.name, g.driver)),
        Some(g) => Check::new(Pass, format!("NVIDIA graphics card: {} (CUDA capability {}.{}, driver {}).", g.name, g.compute.0, g.compute.1, g.driver)),
        None => Check::new(Info, format!("There is no {}. CoTracker cannot run on this computer.", needs())),
    });
    let runtime = gpu.as_ref().filter(|_| cfg!(windows)).map(|_| crate::cotracker::vc_runtime());
    if let Some(r) = &runtime {
        use crate::cotracker::Runtime;
        checks.push(match r {
            Runtime::Ready(v) => Check::new(Pass, format!("Microsoft Visual C++ runtime {v} is installed.")),
            Runtime::Old(v) => Check::new(Info, format!("Microsoft Visual C++ runtime {v} is too old for CoTracker. The CoTracker setup installs a newer one.")),
            Runtime::Missing => Check::new(Info, "The Microsoft Visual C++ runtime is not installed. The CoTracker setup installs it."),
        });
    }
    checks.push(match tt_track::job::cotracker_availability() {
        Ok(()) => Check::new(Pass, "CoTracker is ready."),
        Err(why) => Check::new(Info, why),
    });
    Checked { checks, ready: ffmpeg.is_ok() && ffprobe.is_ok(), gpu, runtime }
}

// ------------------------------------------------------------------ report

/// The report to send: the checks, this computer, and the end of the logs
/// (the last run's and this one's).
pub fn report(checked: &Checked, graphics: Option<&str>) -> String {
    report_from(checked, graphics, &logs_dir(), now_utc())
}

fn report_from(checked: &Checked, graphics: Option<&str>, logs: &Path, when: String) -> String {
    let mut r = String::new();
    let _ = writeln!(r, "trackertools report, {when}");
    let _ = writeln!(r, "version: {} ({})", crate::update::version(), if crate::update::installable() { "a release" } else { "built from source" });
    let _ = writeln!(r, "program: {}", std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default());
    let cpus = std::thread::available_parallelism().map_or(0, |n| n.get());
    let _ = writeln!(r, "system: {} {}, {cpus} logical processors{}", std::env::consts::OS, std::env::consts::ARCH, os_version().map(|v| format!(", {v}")).unwrap_or_default());
    let _ = writeln!(r, "graphics: {}", graphics.unwrap_or("not known"));
    let _ = writeln!(r, "data folder: {}", tt_media::proxy::data_dir().display());
    let _ = writeln!(r, "FFmpeg folder: {}", chosen_dir().display());
    let (python, worker) = tt_track::job::worker_command();
    let model = tt_track::job::cotracker_model().map_or("none".to_string(), |m| m.display().to_string());
    let _ = writeln!(r, "CoTracker: python {}, worker {}, model {}", python.display(), worker.display(), model);
    let _ = writeln!(r, "\nchecks:");
    for c in &checked.checks {
        let _ = writeln!(r, "  [{}] {}", c.tag(), c.text);
    }
    if let Ok(crumb) = std::fs::read_to_string(logs.join(crate::recover::BREADCRUMB)) {
        let _ = writeln!(r, "\nthe last start after an error ({}):\n{}", crate::recover::BREADCRUMB, crumb.trim_end());
    }
    for (title, name, keep) in [("the last run's log", PREVIOUS_LOG, 150), ("this run's log", LOG, 250)] {
        let Ok(text) = std::fs::read_to_string(logs.join(name)) else { continue };
        let lines: Vec<&str> = text.lines().collect();
        let from = lines.len().saturating_sub(keep);
        let _ = writeln!(r, "\n{title} (its last {} of {} lines):", lines.len() - from, lines.len());
        for l in &lines[from..] {
            let _ = writeln!(r, "{l}");
        }
    }
    r
}

/// What CoTracker needs on this kind of computer, for people.
fn needs() -> &'static str {
    if cfg!(target_os = "macos") { "Apple silicon (an M1 or later chip)" } else { "NVIDIA graphics card" }
}

/// `ffmpeg.exe and ffprobe.exe` on Windows, `ffmpeg and ffprobe` elsewhere.
fn ffmpeg_files() -> String {
    let x = std::env::consts::EXE_SUFFIX;
    format!("ffmpeg{x} and ffprobe{x}")
}

fn os_version() -> Option<String> {
    if cfg!(target_os = "macos") {
        let out = quiet(PathBuf::from("/usr/bin/sw_vers")).arg("-productVersion").output().ok()?;
        let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
        return (!v.is_empty()).then(|| format!("macOS {v}"));
    }
    if !cfg!(windows) {
        return None;
    }
    let out = quiet(system_tool("cmd")).args(["/c", "ver"]).output().ok()?;
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!v.is_empty()).then_some(v)
}

fn now_utc() -> String {
    utc(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64))
}

/// Seconds since 1970 as `2026-10-03 22:37 UTC` (days to a date: Howard Hinnant's `civil_from_days`).
fn utc(secs: i64) -> String {
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02} UTC", rest / 3600, rest % 3600 / 60)
}

// ----------------------------------------------------------------- install

/// Where an installation of FFmpeg is.
#[derive(Clone, Debug, PartialEq, Default)]
pub enum Install {
    #[default]
    Idle,
    Download { got: u64, total: u64 },
    Verify,
    Extract,
    Test,
    /// Installed: FFmpeg's version.
    Done(String),
    Failed(Problem),
}

impl Install {
    pub fn busy(&self) -> bool {
        matches!(self, Install::Download { .. } | Install::Verify | Install::Extract | Install::Test)
    }

    /// In a sentence, for the person.
    pub fn text(&self) -> String {
        let mb = |b: u64| b as f64 / 1e6;
        match self {
            Install::Idle => String::new(),
            Install::Download { got, total } if *total > 0 => format!("trackertools downloads FFmpeg: {:.0} MB of {:.0} MB.", mb(*got), mb(*total)),
            Install::Download { got, .. } => format!("trackertools downloads FFmpeg: {:.0} MB.", mb(*got)),
            Install::Verify => "trackertools checks the download.".into(),
            Install::Extract => "trackertools extracts the files.".into(),
            Install::Test => "trackertools tests FFmpeg.".into(),
            Install::Done(v) => format!("FFmpeg {v} is installed."),
            Install::Failed(p) => p.what.clone(),
        }
    }
}

/// Install FFmpeg from `url` into `dir` and see that it starts: its version.
pub fn install(url: &str, dir: &Path, step: &dyn Fn(Install)) -> Result<String, Problem> {
    fetch_and_place(url, dir, step)?;
    step(Install::Test);
    let out = quiet(dir.join(exe("ffmpeg"))).args(["-hide_banner", "-version"]).output().map_err(|e| Problem::new(DOES_NOT_START, e))?;
    let first = String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or_default().to_string();
    if !out.status.success() || !first.contains("version") {
        return Err(Problem::new(DOES_NOT_START, first));
    }
    Ok(version_of(&first))
}

/// Download the zip at `url`, check it against the SHA-256 at `url` +
/// `.sha256`, unpack it in a temporary folder, and put its ffmpeg and
/// ffprobe (and FFmpeg's license) in `dir`.
fn fetch_and_place(url: &str, dir: &Path, step: &dyn Fn(Install)) -> Result<(), Problem> {
    if !writable(dir) {
        return Err(Problem::new(CANNOT_WRITE, dir.display()));
    }
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let work = std::env::temp_dir().join(format!("trackertools-ffmpeg-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&work).map_err(|e| Problem::new("trackertools cannot write to the temporary folder.", e))?;
    let done = (|| {
        let zip = work.join("ffmpeg.zip");
        // Where the address leads now (the latest release's file): the zip and its checksum both come from there.
        let (total, url) = head(url);
        step(Install::Download { got: 0, total });
        download(&url, &zip, &|got| step(Install::Download { got, total }))?;
        step(Install::Verify);
        let expected = fetch_text(&format!("{url}.sha256")).map_err(|e| Problem::new(CANNOT_CHECK, e))?;
        let expected = expected.split_whitespace().next().unwrap_or_default().to_ascii_lowercase();
        let got = sha256(&zip).map_err(|e| Problem::new(CANNOT_CHECK, e))?;
        if expected.len() != 64 || got != expected {
            return Err(Problem::new("The download is not correct. Click Install FFmpeg again.", format!("SHA-256 {got}, expected {expected}")));
        }
        step(Install::Extract);
        let files = work.join("files");
        std::fs::create_dir_all(&files).map_err(|e| Problem::new("trackertools cannot write to the temporary folder.", e))?;
        let out = quiet(system_tool("tar")).arg("-xf").arg(&zip).arg("-C").arg(&files).output().map_err(|e| Problem::new("trackertools cannot extract the files. Click Install FFmpeg again.", e))?;
        if !out.status.success() {
            return Err(Problem::new("trackertools cannot extract the files. Click Install FFmpeg again.", String::from_utf8_lossy(&out.stderr)));
        }
        let (Some(ffmpeg), Some(ffprobe)) = (find(&files, &exe("ffmpeg")), find(&files, &exe("ffprobe"))) else {
            return Err(Problem::new("The download does not contain FFmpeg. Click Install FFmpeg again.", &url));
        };
        for (from, name) in [(ffmpeg, exe("ffmpeg")), (ffprobe, exe("ffprobe"))] {
            replace(&from, &dir.join(name)).map_err(|e| Problem::new(CANNOT_WRITE, e))?;
        }
        if let Some(license) = find(&files, "LICENSE").or_else(|| find(&files, "LICENSE.txt")) {
            let _ = std::fs::copy(license, dir.join("FFMPEG-LICENSE.txt"));
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&work);
    done
}

/// The size of what `url` leads to (0: it doesn't say), and where it leads
/// after redirects (`url` itself if that can't be found out).
fn head(url: &str) -> (u64, String) {
    let Ok(out) = quiet(system_tool("curl")).args(["-sIL", "--max-time", "20", "-w", "\n%{url_effective}"]).args(https_only(url)).arg(url).output() else { return (0, url.to_string()) };
    let text = String::from_utf8_lossy(&out.stdout);
    let size = text
        .lines()
        .filter_map(|l| l.split_once(':').filter(|(k, _)| k.trim().eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.trim().parse().ok()))
        .next_back()
        .unwrap_or(0);
    let last = text.lines().map(str::trim).rfind(|l| !l.is_empty()).filter(|l| l.contains("://")).unwrap_or(url);
    (size, last.to_string())
}

pub(crate) fn fetch_text(url: &str) -> Result<String, String> {
    let out = quiet(system_tool("curl")).args(["-sS", "-L", "--fail", "--max-time", "30"]).args(https_only(url)).arg(url).output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("{url}: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `url` to the file `to` (Windows' curl), telling `progress` the bytes so far.
pub(crate) fn download(url: &str, to: &Path, progress: &dyn Fn(u64)) -> Result<(), Problem> {
    let mut child = quiet(system_tool("curl"))
        .args(["-sS", "-L", "--fail", "--retry", "2", "-H", "User-Agent: trackertools-setup"])
        .args(https_only(url))
        .arg("-o")
        .arg(to)
        .arg(url)
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Problem::new(STOPPED, e))?;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| Problem::new(STOPPED, e))? {
            break status;
        }
        progress(std::fs::metadata(to).map_or(0, |m| m.len()));
        std::thread::sleep(Duration::from_millis(150));
    };
    progress(std::fs::metadata(to).map_or(0, |m| m.len()));
    if !status.success() {
        let mut err = String::new();
        if let Some(mut e) = child.stderr.take() {
            let _ = e.read_to_string(&mut err);
        }
        return Err(Problem::new(STOPPED, format!("curl: {status}: {}", err.trim())));
    }
    Ok(())
}

/// For an https address, curl stays on https (a redirect to plain http is
/// refused). Other addresses are the tests' `file://` ones.
pub(crate) fn https_only(url: &str) -> &'static [&'static str] {
    if url.to_ascii_lowercase().starts_with("https://") { &["--proto", "=https", "--proto-redir", "=https"] } else { &[] }
}

pub(crate) fn sha256(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    }))
}

/// The first file called `name` in `dir` or the folders in it.
pub(crate) fn find(dir: &Path, name: &str) -> Option<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).collect();
    entries.sort();
    if let Some(f) = entries.iter().find(|p| p.is_file() && p.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(name))) {
        return Some(f.clone());
    }
    entries.iter().filter(|p| p.is_dir()).find_map(|d| find(d, name))
}

/// Put `from` at `to`. A program in use there (an export running) is
/// renamed out of the way, as Windows lets a running program be renamed but
/// not replaced; the next start removes it.
fn replace(from: &Path, to: &Path) -> std::io::Result<()> {
    if to.exists() && std::fs::remove_file(to).is_err() {
        let old = to.with_extension("old");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(to, &old)?;
    }
    std::fs::copy(from, to).map(|_| ())
}

// ------------------------------------------------------------------ state

/// The setup and the doctor (a resource; the window draws them).
#[derive(Resource)]
pub struct Doctor {
    /// The first-run setup shows instead of the app.
    pub setup: bool,
    /// The doctor's window is open (Settings, the top bar after an error).
    pub open: bool,
    /// The last run stopped on an error: the top bar offers a report.
    pub last_run_failed: bool,
    /// The graphics adapter (name, API, driver), from the window.
    pub graphics: Option<String>,
    checked: Arc<Mutex<Option<Checked>>>,
    install: Arc<Mutex<Install>>,
    /// Where Install FFmpeg puts it.
    install_dir: PathBuf,
    /// Set up CoTracker (crate::cotracker), running in the background.
    co: crate::cotracker::Setup,
    /// Set up TAPNext (crate::cotracker::TapnextSetup), the same.
    tap: crate::cotracker::TapnextSetup,
    /// The install whose result the checks have seen.
    seen_done: bool,
    /// A sentence under the buttons (copied, saved, a folder without FFmpeg): (text, is it a problem).
    note: Option<(String, bool)>,
    /// JUST DO EVERYTHING FOR ME PLZ was pressed: what it is doing.
    everything: Option<Everything>,
    /// Dev (`TT_SETUP_AUTO`): press it by itself once the checks are done; its seconds before the start.
    auto: Option<f64>,
}

/// JUST DO EVERYTHING FOR ME PLZ: FFmpeg installed if it isn't there, the
/// checks, then the app, each step said in the box under the button.
#[derive(Clone, Copy, Debug)]
struct Everything {
    /// FFmpeg already worked when it was pressed: nothing to install.
    had_ffmpeg: bool,
    /// It started the install (once a press).
    installing: bool,
    /// When everything was ready: the app starts `delay` seconds after.
    ready_at: Option<Instant>,
    delay: f64,
    /// It started the CoTracker setup (an NVIDIA card or Apple silicon; it goes on after the app starts).
    cotracker: bool,
}

impl Default for Doctor {
    fn default() -> Self {
        Self {
            setup: false,
            open: false,
            last_run_failed: false,
            graphics: None,
            checked: Arc::default(),
            install: Arc::default(),
            install_dir: chosen_dir(),
            co: crate::cotracker::Setup::default(),
            tap: crate::cotracker::TapnextSetup::default(),
            seen_done: false,
            note: None,
            everything: None,
            auto: std::env::var("TT_SETUP_AUTO").ok().and_then(|s| s.parse().ok()),
        }
    }
}

impl Doctor {
    /// Look at everything again (in the background).
    pub fn recheck(&mut self) {
        *self.checked.lock().expect("checks") = None;
        let (checked, graphics) = (self.checked.clone(), self.graphics.clone());
        std::thread::spawn(move || {
            let c = run_checks(graphics.as_deref());
            *checked.lock().expect("checks") = Some(c);
        });
    }

    fn checked(&self) -> Option<Checked> {
        self.checked.lock().expect("checks").clone()
    }

    fn install_state(&self) -> Install {
        self.install.lock().expect("install").clone()
    }

    /// Something is still going on: keep the window repainting.
    pub fn busy(&self) -> bool {
        self.checked().is_none() || self.install_state().busy() || self.co.step().busy()
    }

    /// The CoTracker setup (for the top bar).
    pub fn cotracker(&self) -> &crate::cotracker::Setup {
        &self.co
    }

    /// Open the doctor (CoTracker's part comes first in it).
    pub fn show(&mut self) {
        self.open = true;
        self.recheck();
    }

    fn start_install(&mut self) {
        if self.install_state().busy() {
            return;
        }
        self.seen_done = false;
        self.note = None;
        let (state, dir) = (self.install.clone(), self.install_dir.clone());
        *state.lock().expect("install") = Install::Download { got: 0, total: 0 };
        std::thread::spawn(move || {
            let url = std::env::var("TT_FFMPEG_URL").unwrap_or_else(|_| FFMPEG_ZIP.to_string());
            let step = |s: Install| *state.lock().expect("install") = s;
            let done = install(&url, &dir, &step).and_then(|v| remember(&dir).map(|()| v).map_err(|e| Problem::new(CANNOT_WRITE, e)));
            match &done {
                Ok(v) => tracing::info!("FFmpeg {v} installed in {}", dir.display()),
                Err(p) => tracing::warn!("FFmpeg setup: {} ({})", p.what, p.details),
            }
            step(match done {
                Ok(v) => Install::Done(v),
                Err(p) => Install::Failed(p),
            });
        });
    }

    /// The big button: do every step (see [`Everything`]); the app starts `delay` seconds after they are done.
    fn do_everything(&mut self, ready: bool, delay: f64) {
        self.note = None;
        self.everything = Some(Everything { had_ffmpeg: ready, installing: false, ready_at: None, delay, cotracker: false });
    }

    /// The person points at the FFmpeg they have.
    fn use_folder(&mut self, picked: &Path) {
        match ffmpeg_folder(picked) {
            Some(dir) => match remember(&dir) {
                Ok(()) => {
                    tracing::info!("FFmpeg folder chosen: {}", dir.display());
                    self.install_dir = dir;
                    self.note = None;
                    self.recheck();
                }
                Err(e) => self.note = Some((format!("{CANNOT_WRITE} ({e})"), true)),
            },
            None => self.note = Some((format!("This folder does not contain {}. Select a different folder.", ffmpeg_files()), true)),
        }
    }
}

pub struct SetupModule;

impl Module for SetupModule {
    fn build(&self, app: &mut AppBuilder) {
        app.declare::<Doctor>(Class::Derived).init_resource::<Doctor>();
        app.declare::<crate::cotracker::EarlyStart>(Class::Derived).init_resource::<crate::cotracker::EarlyStart>();
    }
}

// --------------------------------------------------------------------- UI

/// The first-run setup, filling the window. True: the person starts the app.
pub fn screen(ui: &mut egui::Ui, doctor: &mut Doctor) -> bool {
    let mut start = false;
    egui::CentralPanel::default().show(ui, |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.set_max_width(660.0);
            ui.add_space(8.0);
            start = everything(ui, doctor);
            ui.add_space(10.0);
            ui.heading(egui::RichText::new("Set up trackertools").strong());
            ui.add_space(4.0);
            start |= body(ui, doctor, true);
        });
    });
    start
}

/// Where a step of JUST DO EVERYTHING FOR ME PLZ is.
#[derive(Clone, Copy, PartialEq)]
enum Mark {
    Done,
    Now,
    Later,
    Problem,
}

/// JUST DO EVERYTHING FOR ME PLZ, and once pressed, what it does, step by
/// step, in a box under it. True: it starts the app.
fn everything(ui: &mut egui::Ui, doctor: &mut Doctor) -> bool {
    let checked = doctor.checked();
    let install = doctor.install_state();
    let ready = checked.as_ref().is_some_and(|c| c.ready);
    let can_install = cfg!(all(windows, target_arch = "x86_64"));
    // A check that fails besides FFmpeg (a read-only data folder): it doesn't start.
    let failing = checked.as_ref().filter(|c| c.ready).is_some_and(|c| c.checks.iter().any(|k| k.level == Level::Fail));
    let warnings = checked.as_ref().map_or(0, |c| c.checks.iter().filter(|k| k.level == Level::Warn).count());
    if checked.is_some()
        && let Some(delay) = doctor.auto.take()
    {
        doctor.do_everything(ready, delay);
    }
    let failed = matches!(install, Install::Failed(_)) && doctor.everything.is_some_and(|e| e.installing);
    let stuck = failed || failing || (doctor.everything.is_some() && !ready && !can_install);

    ui.vertical_centered(|ui| {
        let text = egui::RichText::new("JUST DO EVERYTHING FOR ME PLZ").size(24.0).strong().color(style::BG);
        let button = egui::Button::new(text).fill(style::ACCENT).min_size(egui::vec2(ui.available_width().min(600.0), 60.0)).corner_radius(10.0);
        let free = doctor.everything.is_none() || stuck;
        if ui
            .add_enabled(free, button)
            .on_hover_text("trackertools installs FFmpeg, does the checks and starts. You do not do anything.")
            .clicked()
        {
            doctor.do_everything(ready, 3.0);
        }
    });

    let Some(mut e) = doctor.everything else { return false };
    // What it does next.
    if checked.is_some() && !ready && !e.installing && can_install && !install.busy() {
        doctor.start_install();
        e.installing = true;
    }
    // CoTracker, on graphics that can run it: set up in the background once FFmpeg works (the app doesn't wait).
    let gpu = checked.as_ref().and_then(|c| c.gpu.clone());
    let co_ready = tt_track::job::cotracker_availability().is_ok();
    if ready
        && !co_ready
        && !e.cotracker
        && crate::cotracker::CAN_SET_UP
        && let Some(g) = gpu.as_ref().filter(|g| crate::cotracker::plan(g).is_ok())
    {
        doctor.co.start(g.clone());
        e.cotracker = true;
    }
    // Not while Windows asks for permission for the Visual C++ runtime: step 5 says here what to click.
    let asking = doctor.co.step() == crate::cotracker::Step::Runtime;
    let mut start = false;
    if ready && !failing && checked.is_some() {
        let at = *e.ready_at.get_or_insert_with(Instant::now);
        start = at.elapsed().as_secs_f64() >= e.delay && !asking;
        ui.ctx().request_repaint_after(Duration::from_millis(100));
    }
    doctor.everything = Some(e);

    // What it says.
    let red = egui::Color32::from_rgb(0xf4, 0x3f, 0x5e);
    let install_now = doctor.install_state();
    let mut lines: Vec<(Mark, String)> = vec![(Mark::Done, format!("1. trackertools uses this folder for FFmpeg: {}", doctor.install_dir.display()))];
    lines.push(if e.had_ffmpeg || (ready && !e.installing) {
        (Mark::Done, "2. FFmpeg is on this computer. trackertools does not download it.".into())
    } else if !can_install {
        (Mark::Problem, "2. trackertools cannot install FFmpeg on this computer. Install FFmpeg with a package manager. Then click Check again.".into())
    } else {
        match &install_now {
            Install::Idle => (Mark::Now, "2. trackertools gets ready to download FFmpeg.".into()),
            Install::Done(v) => (Mark::Done, format!("2. FFmpeg {v} is installed.")),
            Install::Failed(p) => (Mark::Problem, format!("2. {} If the problem continues, click Copy report below. Send the report to the person who gave you trackertools.", p.what)),
            busy => (Mark::Now, format!("2. {}", busy.text())),
        }
    });
    lines.push(match &checked {
        _ if !(ready || matches!(install_now, Install::Done(_))) => (Mark::Later, "3. trackertools does the checks.".into()),
        None => (Mark::Now, "3. trackertools does the checks.".into()),
        Some(_) if failing => (Mark::Problem, "3. A check failed. Read the checks below. Click Copy report. Send the report to the person who gave you trackertools.".into()),
        Some(_) if warnings == 1 => (Mark::Done, "3. The checks are complete. 1 check has a warning. trackertools can start.".into()),
        Some(_) if warnings > 1 => (Mark::Done, format!("3. The checks are complete. {warnings} checks have a warning. trackertools can start.")),
        Some(_) => (Mark::Done, "3. The checks are complete. All checks are good.".into()),
    });
    lines.push(match e.ready_at {
        Some(_) if !failing && asking => (Mark::Later, "4. trackertools starts when the Microsoft Visual C++ runtime is installed (step 5).".into()),
        Some(at) if !failing => {
            let left = (e.delay - at.elapsed().as_secs_f64()).ceil().max(0.0) as u32;
            match left {
                0 => (Mark::Now, "4. trackertools starts now.".into()),
                1 => (Mark::Now, "4. trackertools starts in 1 second.".into()),
                n => (Mark::Now, format!("4. trackertools starts in {n} seconds.")),
            }
        }
        _ => (Mark::Later, "4. trackertools starts.".into()),
    });
    let co = doctor.co.step();
    lines.push(match &gpu {
        _ if co_ready => (Mark::Done, "5. CoTracker is ready.".into()),
        None if checked.is_some() => (Mark::Later, format!("5. CoTracker needs {}. This computer does not have it. trackertools does not set up CoTracker.", needs())),
        None => (Mark::Later, "5. trackertools looks for graphics for CoTracker.".into()),
        Some(g) => match crate::cotracker::plan(g) {
            Err(why) => (Mark::Problem, format!("5. {why}")),
            Ok(_) => match &co {
                crate::cotracker::Step::Failed(p) => (Mark::Problem, format!("5. {} The Doctor in Settings has the CoTracker steps.", p.what)),
                crate::cotracker::Step::Done(_) => (Mark::Done, format!("5. {}", co.text())),
                crate::cotracker::Step::Runtime => (Mark::Now, format!("5. {}", co.text())),
                s if s.busy() => (Mark::Now, format!("5. {} This continues after trackertools starts.", s.text())),
                _ => (Mark::Later, format!("5. trackertools sets up CoTracker for the {}. This continues after trackertools starts.", g.name)),
            },
        },
    });
    ui.add_space(8.0);
    egui::Frame::new()
        .fill(style::PANEL)
        .stroke(egui::Stroke::new(1.0, if stuck { red } else { style::ACCENT.gamma_multiply(0.7) }))
        .corner_radius(8.0)
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(egui::RichText::new("trackertools does these steps for you:").strong());
            ui.add_space(4.0);
            for (mark, text) in &lines {
                ui.horizontal(|ui| {
                    match mark {
                        Mark::Done => {
                            ui.colored_label(style::ACCENT, "\u{2714}");
                        }
                        Mark::Now => {
                            ui.spinner();
                        }
                        Mark::Later => {
                            ui.colored_label(style::MUTED, "\u{2022}");
                        }
                        Mark::Problem => {
                            ui.colored_label(red, "\u{2716}");
                        }
                    }
                    let rich = egui::RichText::new(text);
                    let rich = match mark {
                        Mark::Later => rich.color(style::MUTED),
                        Mark::Problem => rich.color(red),
                        _ => rich,
                    };
                    ui.add(egui::Label::new(rich).wrap());
                });
                if text.starts_with("2. trackertools downloads")
                    && let Install::Download { got, total } = &install_now
                    && *total > 0
                {
                    ui.add(egui::ProgressBar::new(*got as f32 / *total as f32).desired_width(ui.available_width()));
                }
            }
            if stuck {
                ui.add_space(4.0);
                ui.label("Click JUST DO EVERYTHING FOR ME PLZ again to try again.");
            }
        });
    start
}

/// CoTracker: what it needs, and the button that sets it up (crate::cotracker).
fn cotracker_part(ui: &mut egui::Ui, doctor: &mut Doctor, checked: Option<&Checked>) {
    use crate::cotracker::Step;
    let red = egui::Color32::from_rgb(0xf4, 0x3f, 0x5e);
    let line = |ui: &mut egui::Ui, mark: &str, color: egui::Color32, text: &str| {
        ui.horizontal(|ui| {
            ui.colored_label(color, mark);
            ui.add(egui::Label::new(egui::RichText::new(text).color(if color == red { red } else { style::TEXT })).wrap());
        });
    };
    ui.add_space(10.0);
    ui.separator();
    ui.label(egui::RichText::new("CoTracker").strong());
    ui.label(if cfg!(target_os = "macos") {
        "CoTracker is a second kind of tracker. It uses the graphics of Apple silicon."
    } else {
        "CoTracker is a second kind of tracker. It uses the NVIDIA graphics card."
    });
    let step = doctor.co.step();
    if step.busy() {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.add(egui::Label::new(step.text()).wrap());
        });
        if let Step::Model { got, total } = step
            && total > 0
        {
            ui.add(egui::ProgressBar::new(got as f32 / total as f32).desired_width(360.0));
        }
        if let Some(secs) = doctor.co.elapsed() {
            ui.label(egui::RichText::new(format!("Time: {} min {} s. You can use trackertools while it continues.", secs / 60, secs % 60)).color(style::MUTED));
        }
        return;
    }
    let available = tt_track::job::cotracker_availability().is_ok();
    if available {
        line(ui, "\u{2714}", style::ACCENT, &if let Step::Done(_) = step { step.text() } else { "CoTracker is ready.".into() });
        tapnext(ui, doctor);
        return;
    }
    if let Step::Failed(p) = &step {
        line(ui, "\u{2716}", red, &p.what);
    }
    let Some(c) = checked else {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("trackertools does the checks.");
        });
        return;
    };
    let Some(gpu) = &c.gpu else {
        line(ui, "\u{2013}", style::MUTED, &format!("This computer does not have {}. CoTracker cannot run without it.", needs()));
        return;
    };
    match crate::cotracker::plan(gpu) {
        Err(why) => line(ui, "\u{2716}", red, &why),
        Ok(plan) => {
            ui.label(format!("This computer has an {}. CoTracker can use it.", gpu.name));
            ui.label(format!(
                "To set up CoTracker, trackertools downloads Python, PyTorch for {} and the CoTracker model. The download is approximately {}.",
                plan.label, plan.total_size
            ));
            ui.label("Make sure that the disk has approximately 6 GB free. The setup can take 5 to 30 minutes.");
            if c.runtime.as_ref().is_some_and(|r| !matches!(r, crate::cotracker::Runtime::Ready(_))) {
                ui.label("trackertools also installs the Microsoft Visual C++ runtime from Microsoft. Windows asks for permission. Click Yes.");
            }
            ui.label("The CoTracker model is for non-commercial use only (license: CC BY-NC 4.0).");
            if crate::cotracker::CAN_SET_UP {
                let label = if matches!(step, Step::Failed(_)) { "Set up CoTracker again" } else { "Set up CoTracker" };
                if ui.button(egui::RichText::new(label).strong()).clicked() {
                    doctor.co.start(gpu.clone());
                }
            } else {
                ui.label("trackertools cannot set up CoTracker on this computer.");
            }
        }
    }
}

/// TAPNext, once CoTracker is ready (it runs in CoTracker's worker).
fn tapnext(ui: &mut egui::Ui, doctor: &mut Doctor) {
    use crate::cotracker::TapnextStep;
    let red = egui::Color32::from_rgb(0xf4, 0x3f, 0x5e);
    let line = |ui: &mut egui::Ui, mark: &str, color: egui::Color32, text: &str| {
        ui.horizontal(|ui| {
            ui.colored_label(color, mark);
            ui.add(egui::Label::new(egui::RichText::new(text).color(if color == red { red } else { style::TEXT })).wrap());
        });
    };
    let step = doctor.tap.step();
    if step.busy() {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(step.text());
        });
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
        return;
    }
    if tt_track::job::tapnext_availability().is_ok() {
        line(ui, "\u{2714}", style::ACCENT, "TAPNext is ready (an experimental point tracker: Track tool, TAPNext).");
        return;
    }
    if let TapnextStep::Failed(p) = &step {
        line(ui, "\u{2716}", red, &p.what);
    }
    ui.label("TAPNext is an experimental point tracker from Google DeepMind (license: Apache 2.0). It gives each frame's result at once.");
    ui.label("To set it up, trackertools downloads its model (2.5 GB) and keeps a smaller copy (389 MB).");
    let label = if matches!(step, TapnextStep::Failed(_)) { "Set up TAPNext again" } else { "Set up TAPNext" };
    if ui.button(label).clicked() {
        doctor.tap.start();
    }
}

/// The doctor's window (from Settings, or the top bar after an error).
pub fn window(ctx: &egui::Context, doctor: &mut Doctor) {
    if !doctor.open {
        return;
    }
    let mut open = true;
    let tall = ctx.content_rect().height() * 0.8;
    egui::Window::new("Doctor").open(&mut open).default_width(600.0).collapsible(false).show(ctx, |ui| {
        egui::ScrollArea::vertical().max_height(tall).show(ui, |ui| {
            body(ui, doctor, false);
        });
    });
    doctor.open = open;
}

/// What setup and the doctor show. True: Start trackertools was clicked.
fn body(ui: &mut egui::Ui, doctor: &mut Doctor, first_run: bool) -> bool {
    let checked = doctor.checked();
    let install = doctor.install_state();
    // A finished install: look again (once).
    if matches!(install, Install::Done(_)) && !doctor.seen_done {
        doctor.seen_done = true;
        doctor.recheck();
    }
    let ready = checked.as_ref().is_some_and(|c| c.ready);
    let mut start = false;
    let step = |ui: &mut egui::Ui, text: &str| {
        ui.add_space(8.0);
        ui.label(egui::RichText::new(text).strong());
    };

    if first_run {
        ui.label(if ready {
            "FFmpeg is installed. Click Start trackertools."
        } else {
            "trackertools uses FFmpeg to read and write video files. FFmpeg is not on this computer."
        });
    } else {
        ui.label("The doctor checks FFmpeg, the graphics and the folders. It also makes a report for the person who helps you.");
        // In the app, CoTracker is what people come here for (its button sends them).
        cotracker_part(ui, doctor, checked.as_ref());
    }

    // FFmpeg: where, install, or the one they have.
    step(ui, if first_run { "1. Select a folder for FFmpeg." } else { "FFmpeg folder" });
    ui.horizontal(|ui| {
        if ui.add_enabled(!install.busy(), egui::Button::new("Change\u{2026}")).clicked()
            && let Some(dir) = rfd::FileDialog::new().set_directory(&doctor.install_dir).pick_folder()
        {
            doctor.install_dir = dir;
        }
        ui.add(egui::Label::new(egui::RichText::new(doctor.install_dir.display().to_string()).monospace()).wrap());
    });
    if cfg!(all(windows, target_arch = "x86_64")) {
        step(ui, if first_run { "2. Click Install FFmpeg. The download is approximately 120 MB." } else { "Install FFmpeg in this folder. The download is approximately 120 MB." });
        ui.horizontal(|ui| {
            let label = if ready && !first_run { "Install FFmpeg again" } else { "Install FFmpeg" };
            if ui.add_enabled(!install.busy(), egui::Button::new(egui::RichText::new(label).strong())).clicked() {
                doctor.start_install();
            }
            if install.busy() {
                ui.spinner();
            }
        });
        if let Install::Download { got, total } = &install
            && *total > 0
        {
            ui.add(egui::ProgressBar::new(*got as f32 / *total as f32).desired_width(360.0));
        }
        match &install {
            Install::Idle => {}
            Install::Failed(p) => {
                ui.colored_label(egui::Color32::from_rgb(0xf4, 0x3f, 0x5e), &p.what);
            }
            Install::Done(_) => {
                ui.colored_label(style::ACCENT, install.text());
            }
            _ => {
                ui.label(install.text());
            }
        }
    } else {
        step(ui, "2. Install FFmpeg with a package manager, for example: brew install ffmpeg. Then click Check again.");
    }
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        ui.label(format!("If you have FFmpeg, click Find FFmpeg. Then select the folder that contains {}.", ffmpeg_files()));
        if ui.add_enabled(!install.busy(), egui::Button::new("Find FFmpeg\u{2026}")).clicked()
            && let Some(dir) = rfd::FileDialog::new().pick_folder()
        {
            doctor.use_folder(&dir);
        }
    });

    if first_run {
        step(ui, "3. Click Start trackertools.");
        let go = ui.add_enabled(ready, egui::Button::new(egui::RichText::new("Start trackertools").strong().size(16.0)));
        start = go.on_disabled_hover_text("Start trackertools is available after FFmpeg is installed.").clicked();
    }

    // At setup, CoTracker comes after FFmpeg's steps (above the long list of checks).
    if first_run {
        cotracker_part(ui, doctor, checked.as_ref());
    }

    // The checks.
    ui.add_space(10.0);
    ui.separator();
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Checks").strong());
        if ui.add_enabled(checked.is_some(), egui::Button::new("Check again")).clicked() {
            doctor.recheck();
        }
    });
    match &checked {
        None => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("trackertools does the checks.");
            });
        }
        Some(c) => {
            for check in &c.checks {
                let (mark, color) = match check.level {
                    Level::Pass => ("\u{2714}", style::ACCENT),
                    Level::Info => ("\u{2013}", style::MUTED),
                    Level::Warn => ("\u{26a0}", egui::Color32::from_rgb(0xfb, 0xbf, 0x24)),
                    Level::Fail => ("\u{2716}", egui::Color32::from_rgb(0xf4, 0x3f, 0x5e)),
                };
                ui.horizontal(|ui| {
                    ui.colored_label(color, mark);
                    ui.add(egui::Label::new(&check.text).wrap());
                });
            }
        }
    }

    // The report.
    ui.add_space(10.0);
    ui.separator();
    ui.label(ASK_FOR_HELP);
    ui.horizontal(|ui| {
        let can = checked.is_some();
        if ui.add_enabled(can, egui::Button::new("Copy report")).clicked()
            && let Some(c) = &checked
        {
            ui.ctx().copy_text(report(c, doctor.graphics.as_deref()));
            doctor.note = Some(("The report is on the clipboard.".into(), false));
        }
        if ui.add_enabled(can, egui::Button::new("Save report\u{2026}")).clicked()
            && let Some(c) = &checked
        {
            let mut dialog = rfd::FileDialog::new().set_file_name("trackertools-report.txt").add_filter("Text", &["txt"]);
            if let Some(desktop) = std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join("Desktop")).filter(|d| d.is_dir()) {
                dialog = dialog.set_directory(desktop);
            }
            if let Some(path) = dialog.save_file() {
                doctor.note = Some(match std::fs::write(&path, report(c, doctor.graphics.as_deref())) {
                    Ok(()) => (format!("The report is in {}.", path.display()), false),
                    Err(e) => (format!("trackertools cannot save the report there. Select a different folder. ({e})"), true),
                });
            }
        }
    });
    if let Some((text, problem)) = &doctor.note {
        if *problem {
            ui.colored_label(egui::Color32::from_rgb(0xf4, 0x3f, 0x5e), text);
        } else {
            ui.colored_label(style::ACCENT, text);
        }
    }
    if doctor.busy() {
        ui.ctx().request_repaint_after(Duration::from_millis(120));
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_read_from_the_first_line() {
        assert_eq!(version_of("ffmpeg version 7.0.1-essentials_build-www.gyan.dev Copyright (c) 2000-2024"), "7.0.1");
        assert_eq!(version_of("ffprobe version 9.0.2-essentials_build-www.gyan.dev"), "9.0.2");
        assert_eq!(version_of("ffmpeg version N-117500-g0b3c3e6f6e-20241010 Copyright"), "N-117500-g0b3c3e6f6e-20241010");
        assert_eq!(version_of("ffmpeg version 6.1 Copyright"), "6.1");
    }

    #[test]
    fn dates_are_utc() {
        assert_eq!(utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(utc(951_782_400 + 3_661), "2000-02-29 01:01 UTC");
        assert_eq!(utc(1_759_530_000), "2025-10-03 22:20 UTC");
    }

    /// A zip like gyan.dev's (`<name>/bin/ffmpeg.exe`, …, `<name>/LICENSE`) and its `.sha256`, as a file:// address.
    fn fake_build(dir: &Path, with_ffmpeg: bool) -> String {
        let root = dir.join("src").join("ffmpeg-9.9-essentials_build");
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        if with_ffmpeg {
            std::fs::write(bin.join(exe("ffmpeg")), b"ffmpeg").unwrap();
            std::fs::write(bin.join(exe("ffprobe")), b"ffprobe").unwrap();
        }
        std::fs::write(bin.join(exe("ffplay")), b"ffplay").unwrap();
        std::fs::write(root.join("LICENSE"), b"GPL").unwrap();
        let zip = dir.join("ffmpeg-test.zip");
        let made = quiet(system_tool("tar")).arg("-a").arg("-cf").arg(&zip).arg("-C").arg(dir.join("src")).arg(".").status().unwrap();
        assert!(made.success());
        std::fs::write(dir.join("ffmpeg-test.zip.sha256"), format!("{}\n", sha256(&zip).unwrap())).unwrap();
        format!("file:///{}", zip.display().to_string().replace('\\', "/"))
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tt-setup-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_checked_download_puts_ffmpeg_and_ffprobe_in_the_folder() {
        let d = scratch("ok");
        let url = fake_build(&d, true);
        let dest = d.join("chosen folder");
        let steps = Mutex::new(Vec::new());
        fetch_and_place(&url, &dest, &|s| steps.lock().unwrap().push(s)).expect("installs");
        assert_eq!(std::fs::read(dest.join(exe("ffmpeg"))).unwrap(), b"ffmpeg");
        assert_eq!(std::fs::read(dest.join(exe("ffprobe"))).unwrap(), b"ffprobe");
        assert!(dest.join("FFMPEG-LICENSE.txt").is_file());
        assert!(!dest.join(exe("ffplay")).exists(), "only what trackertools runs");
        let steps = steps.into_inner().unwrap();
        assert!(steps.iter().any(|s| matches!(s, Install::Download { total, .. } if *total > 0)), "the size is known: {steps:?}");
        assert!(steps.contains(&Install::Verify) && steps.contains(&Install::Extract));
        assert_eq!(ffmpeg_folder(&dest), Some(dest.clone()));
        let unpacked = d.join("src").join("ffmpeg-9.9-essentials_build");
        assert_eq!(ffmpeg_folder(&unpacked), Some(unpacked.join("bin")), "an unpacked build: its bin");
        assert_eq!(ffmpeg_folder(&d), None);
        // Installed again over itself (a program in use is renamed away instead).
        fetch_and_place(&url, &dest, &|_| {}).expect("again");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_download_that_does_not_match_its_checksum_is_refused() {
        let d = scratch("bad");
        let url = fake_build(&d, true);
        std::fs::write(d.join("ffmpeg-test.zip.sha256"), format!("{}\n", "0".repeat(64))).unwrap();
        let err = fetch_and_place(&url, &d.join("dest"), &|_| {}).expect_err("refused");
        assert_eq!(err.what, "The download is not correct. Click Install FFmpeg again.");
        assert!(!d.join("dest").join(exe("ffmpeg")).exists());
        // And one without FFmpeg in it.
        let e = scratch("empty");
        let url = fake_build(&e, false);
        assert_eq!(fetch_and_place(&url, &e.join("dest"), &|_| {}).expect_err("refused").what, "The download does not contain FFmpeg. Click Install FFmpeg again.");
        let _ = std::fs::remove_dir_all(d);
        let _ = std::fs::remove_dir_all(e);
    }

    #[test]
    fn the_report_has_the_checks_and_the_ends_of_the_logs() {
        let d = scratch("report");
        let lines: String = (0..400).map(|i| format!("line {i}\n")).collect();
        std::fs::write(d.join(LOG), &lines).unwrap();
        std::fs::write(d.join(PREVIOUS_LOG), format!("start\n{PANIC}panicked at src/main.rs\n")).unwrap();
        let checked = Checked { checks: vec![Check::new(Level::Fail, "FFmpeg is not installed."), Check::new(Level::Pass, "Graphics: a GPU.")], ready: false, gpu: None, runtime: None };
        let r = report_from(&checked, Some("a GPU"), &d, "2026-10-03 22:37 UTC".into());
        assert!(r.starts_with("trackertools report, 2026-10-03 22:37 UTC\n"));
        assert!(r.contains("  [FAIL] FFmpeg is not installed.\n") && r.contains("  [OK] Graphics: a GPU.\n"));
        assert!(r.contains("the last run's log (its last 2 of 2 lines):\nstart\nPANIC: panicked"));
        assert!(r.contains("this run's log (its last 250 of 400 lines):\nline 150\n") && r.ends_with("line 399\n"));
        let _ = std::fs::remove_dir_all(d);
    }
}
