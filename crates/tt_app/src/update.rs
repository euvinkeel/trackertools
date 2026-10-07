//! Updates: the repository's latest release on GitHub, installed in one click
//! (Settings → Updates, or the top bar's button when a new version is out).
//!
//! The repository is the source of truth. Pushing a tag `vX.Y.Z` makes the
//! release workflow (`.github/workflows/release.yml`) build
//! `trackertools-windows-x64.zip` and publish it with the changes since the
//! last tag as its notes. A copy that workflow built knows its version (the
//! tag, `TT_VERSION` at build time) and can replace itself; a copy built from
//! source says so and leaves updating to git.
//!
//! The download is checked before anything is installed: it must come over
//! HTTPS (a redirect to plain http is refused), and its SHA-256 must match
//! the one GitHub records for the release file (the asset's `digest`), or the
//! `.sha256` file the workflow publishes beside it. A release with neither
//! isn't installed. (This catches a broken or swapped download; it can't
//! catch a bad release published from the repository's own account.)
//!
//! No libraries for it: downloads go through `curl` and the zip opens with
//! `tar`, both part of Windows 10/11 and macOS. Installing renames each file it
//! replaces to `<name>.old` (Windows lets a running program be renamed, not
//! overwritten) and moves the new one in; the old ones are deleted at the next
//! start. Then trackertools saves your work, closes, and starts the new version.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bevy_ecs::prelude::*;
use tt_core::{AppBuilder, Class, Module};

/// Where releases come from: `owner/name` on GitHub. People can only get
/// updates from it if it's public (a private repository's releases need a
/// GitHub sign-in).
pub const REPO: &str = "euvinkeel/trackertools";

/// This copy's version: the tag it was released as, or the crate's version
/// with "-dev" for a copy built from source.
pub fn version() -> String {
    match option_env!("TT_VERSION") {
        Some(tag) => tag.trim_start_matches('v').to_string(),
        None => format!("{}-dev", env!("CARGO_PKG_VERSION")),
    }
}

/// Built by the release workflow: it can replace itself.
pub fn installable() -> bool {
    option_env!("TT_VERSION").is_some()
}

/// The release download made for this computer.
pub fn asset_name() -> Option<&'static str> {
    if cfg!(all(windows, target_arch = "x86_64")) {
        Some("trackertools-windows-x64.zip")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("trackertools-macos-arm64.zip")
    } else {
        None
    }
}

/// A published version.
#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version: String,
    /// What changed (the release's description).
    pub notes: String,
    /// Its page on GitHub.
    pub page: String,
    /// The download for this computer, if there is one.
    pub download: Option<Download>,
}

/// A release's file for this computer.
#[derive(Clone, Debug, PartialEq)]
pub struct Download {
    pub url: String,
    /// Bytes (0 if unknown).
    pub size: u64,
    /// Its SHA-256 (lowercase hex) as GitHub records it, if it does.
    pub sha256: Option<String>,
    /// The `.sha256` file published beside it, if there is one.
    pub sha256_url: Option<String>,
}

/// Something went wrong: what to tell a person, and what to tell a developer.
#[derive(Clone, Debug, PartialEq)]
pub struct Problem {
    pub what: String,
    pub details: String,
}

impl Problem {
    pub(crate) fn new(what: &str, details: impl std::fmt::Display) -> Self {
        Self { what: what.to_string(), details: details.to_string() }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Downloading { release: Release, got: u64, total: u64 },
    /// Downloaded and unpacked into `staged`: installed on the next frame.
    Ready { release: Release, staged: PathBuf },
    Restarting,
    Failed(Problem),
}

/// The updater (a resource): its state is shared with the thread doing the work.
#[derive(Resource, Clone)]
pub struct Updater {
    state: Arc<Mutex<State>>,
    /// Look for a new version when trackertools starts (a user setting).
    pub check_on_start: bool,
    /// A version the user chose to skip: no prompt for it (a user setting).
    pub skipped: Option<String>,
    /// A version whose prompt was closed with Later, this run.
    pub later: Option<String>,
}

impl Default for Updater {
    fn default() -> Self {
        Self { state: Arc::new(Mutex::new(State::Idle)), check_on_start: true, skipped: None, later: None }
    }
}

/// Set when an update is installed: the program to start once this one has saved and closed.
#[derive(Resource, Default)]
pub struct RestartWith(pub Option<PathBuf>);

impl Updater {
    pub fn state(&self) -> State {
        self.state.lock().expect("updater state").clone()
    }

    fn set(&self, s: State) {
        *self.state.lock().expect("updater state") = s;
    }

    /// A new version this copy can install, if one was found.
    pub fn available(&self) -> Option<Release> {
        match self.state() {
            State::Available(r) if installable() && r.download.is_some() => Some(r),
            _ => None,
        }
    }

    /// [`available`](Self::available), unless the user skipped that version: what the dot on the Settings tab shows.
    pub fn news(&self) -> Option<Release> {
        self.available().filter(|r| self.skipped.as_deref() != Some(r.version.as_str()))
    }

    pub fn busy(&self) -> bool {
        matches!(self.state(), State::Checking | State::Downloading { .. } | State::Ready { .. } | State::Restarting)
    }

    /// Looks for a new version in the background. `quiet`: the check at start,
    /// which only speaks up when there is one.
    pub fn check(&self, quiet: bool) {
        if self.busy() {
            return;
        }
        self.set(State::Checking);
        let me = self.clone();
        std::thread::spawn(move || {
            let url = std::env::var("TT_UPDATE_FEED").unwrap_or_else(|_| format!("https://api.github.com/repos/{REPO}/releases/latest"));
            me.set(match fetch_release(&url) {
                Ok(r) if newer(&r.version, &version()) => State::Available(r),
                Ok(_) if quiet => State::Idle,
                Ok(_) => State::UpToDate,
                Err(e) if quiet => {
                    tracing::info!("update check: {} ({})", e.what, e.details);
                    State::Idle
                }
                Err(e) => State::Failed(e),
            });
        });
    }

    /// Downloads and unpacks `release` in the background; [`drive`] installs it.
    pub fn update(&self, release: Release) {
        if self.busy() {
            return;
        }
        let Some(file) = release.download.clone() else { return };
        let size = file.size;
        self.set(State::Downloading { release: release.clone(), got: 0, total: size });
        let me = self.clone();
        std::thread::spawn(move || {
            let dir = std::env::temp_dir().join(format!("trackertools-update-{}", release.version));
            let progress = |got| me.set(State::Downloading { release: release.clone(), got, total: size });
            me.set(match download(&file, &dir, progress) {
                Ok(staged) => State::Ready { release, staged },
                Err(e) => State::Failed(e),
            });
        });
    }
}

/// Whether version `a` is newer than `b` ("1.2.10" > "1.2.9"; a "-dev" or
/// other suffix ranks below the plain version).
pub fn newer(a: &str, b: &str) -> bool {
    let parts = |v: &str| -> (Vec<u64>, bool) {
        let v = v.trim().trim_start_matches('v');
        let (num, suffix) = v.split_once('-').unwrap_or((v, ""));
        (num.split('.').map(|p| p.parse().unwrap_or(0)).collect(), suffix.is_empty())
    };
    let ((mut na, plain_a), (mut nb, plain_b)) = (parts(a), parts(b));
    let n = na.len().max(nb.len());
    na.resize(n, 0);
    nb.resize(n, 0);
    na > nb || (na == nb && plain_a && !plain_b)
}

/// A console tool run without a window of its own (the app has no console).
pub(crate) fn quiet(program: PathBuf) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
}

pub(crate) fn system_tool(name: &str) -> PathBuf {
    // Windows' own curl and tar (System32): another tar on the PATH (Git's) can't open zips.
    if cfg!(windows) {
        let root = std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        let path = root.join("System32").join(format!("{name}.exe"));
        if path.is_file() {
            return path;
        }
    }
    PathBuf::from(name)
}

/// Where curl may go for `url`: an https address only over https (redirects
/// too). A `file://` one only in tests or with `TT_UPDATE_FEED` set (a
/// developer's feed); anything else is refused.
pub(crate) fn protocols(url: &str) -> Result<[&'static str; 4], Problem> {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("https://") {
        Ok(["--proto", "=https", "--proto-redir", "=https"])
    } else if lower.starts_with("file://") && (cfg!(test) || std::env::var_os("TT_UPDATE_FEED").is_some()) {
        Ok(["--proto", "=file", "--proto-redir", "=file"])
    } else {
        Err(Problem::new("The update address is not a secure (https) address, so trackertools does not use it.", url))
    }
}

/// The latest release, from GitHub's API (or a `file://` feed, for tests).
pub fn fetch_release(url: &str) -> Result<Release, Problem> {
    let out = quiet(system_tool("curl"))
        .args(["-sS", "-L", "--max-time", "20", "-H", "Accept: application/vnd.github+json", "-H", "User-Agent: trackertools-updater", "-w", "\n%{http_code}"])
        .args(protocols(url)?)
        .arg(url)
        .output()
        .map_err(|e| Problem::new("Couldn't start the download tool (curl), which comes with Windows 10 and later.", e))?;
    if !out.status.success() {
        return Err(Problem::new("Couldn't reach GitHub. Check your internet connection and try again.", String::from_utf8_lossy(&out.stderr)));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let (body, code) = text.rsplit_once('\n').unwrap_or((&text, ""));
    match code.trim() {
        // (000: a file:// feed.)
        "200" | "000" => parse_release(body),
        "404" => Err(Problem::new("No published version was found yet.", format!("{url}: 404 (the repository is private, or has no releases)"))),
        "403" | "429" => Err(Problem::new("GitHub is limiting requests right now. Try again in an hour.", format!("{url}: {code}"))),
        other => Err(Problem::new("GitHub answered with an error. Try again later.", format!("{url}: {other}\n{body}"))),
    }
}

/// A release as GitHub's API describes it.
pub fn parse_release(json: &str) -> Result<Release, Problem> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| Problem::new("GitHub's answer didn't make sense.", e))?;
    let tag = v["tag_name"].as_str().ok_or_else(|| Problem::new("GitHub's answer didn't make sense.", "no tag_name"))?;
    let download = asset_name().and_then(|name| {
        let assets = v["assets"].as_array()?;
        let a = assets.iter().find(|a| a["name"].as_str() == Some(name))?;
        let sha256 = a["digest"].as_str().and_then(|d| d.strip_prefix("sha256:")).map(str::to_ascii_lowercase);
        let sidecar = format!("{name}.sha256");
        let sha256_url = assets.iter().find(|a| a["name"].as_str() == Some(sidecar.as_str())).and_then(|a| a["browser_download_url"].as_str()).map(str::to_string);
        Some(Download { url: a["browser_download_url"].as_str()?.to_string(), size: a["size"].as_u64().unwrap_or(0), sha256, sha256_url })
    });
    Ok(Release {
        version: tag.trim_start_matches('v').to_string(),
        notes: v["body"].as_str().unwrap_or_default().trim().to_string(),
        page: v["html_url"].as_str().unwrap_or_default().to_string(),
        download,
    })
}

/// The SHA-256 the download must have: GitHub's record of it, else the
/// `.sha256` file beside it. Neither: no update (a release can't be checked).
fn expected_sha256(file: &Download) -> Result<String, Problem> {
    let cannot = "This version can't be checked, so trackertools doesn't install it. Download it from the release page instead.";
    let text = match (&file.sha256, &file.sha256_url) {
        (Some(h), _) => h.clone(),
        (None, Some(url)) => {
            let out = quiet(system_tool("curl"))
                .args(["-sS", "-L", "--fail", "--max-time", "30", "-H", "User-Agent: trackertools-updater"])
                .args(protocols(url)?)
                .arg(url)
                .output()
                .map_err(|e| Problem::new(cannot, e))?;
            if !out.status.success() {
                return Err(Problem::new(cannot, format!("{url}: {}", String::from_utf8_lossy(&out.stderr).trim())));
            }
            String::from_utf8_lossy(&out.stdout).split_whitespace().next().unwrap_or_default().to_ascii_lowercase()
        }
        (None, None) => return Err(Problem::new(cannot, format!("{}: no digest and no .sha256 file", file.url))),
    };
    if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Problem::new(cannot, format!("not a SHA-256: {text:?}")));
    }
    Ok(text)
}

/// Downloads `file` into `dir`, checks it and unpacks it; returns the folder
/// holding the new program. `progress` hears the bytes so far.
pub fn download(file: &Download, dir: &Path, progress: impl Fn(u64)) -> Result<PathBuf, Problem> {
    let (url, size) = (file.url.as_str(), file.size);
    let protocols = protocols(url)?;
    let expected = expected_sha256(file)?;
    // Our own folder in the temp directory, left from an earlier try at most.
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).map_err(|e| Problem::new("Couldn't make a folder for the download.", e))?;
    let zip = dir.join("update.zip");
    let mut child = quiet(system_tool("curl"))
        .args(["-sS", "-L", "--fail", "--retry", "2", "-H", "User-Agent: trackertools-updater"])
        .args(protocols)
        .arg("-o")
        .arg(&zip)
        .arg(url)
        .spawn()
        .map_err(|e| Problem::new("Couldn't start the download tool (curl), which comes with Windows 10 and later.", e))?;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| Problem::new("The download stopped.", e))? {
            break status;
        }
        progress(std::fs::metadata(&zip).map_or(0, |m| m.len()));
        std::thread::sleep(Duration::from_millis(150));
    };
    let got = std::fs::metadata(&zip).map_or(0, |m| m.len());
    progress(got);
    if !status.success() {
        return Err(Problem::new("The download didn't finish. Check your internet connection and try again.", format!("curl: {status}")));
    }
    if size > 0 && got != size {
        return Err(Problem::new("The download came out incomplete. Try again.", format!("{got} of {size} bytes")));
    }
    let actual = crate::setup::sha256(&zip).map_err(|e| Problem::new("Couldn't check the download.", e))?;
    if actual != expected {
        let _ = std::fs::remove_file(&zip);
        return Err(Problem::new("The download is not the published file, so trackertools doesn't install it. Try again later.", format!("SHA-256 {actual}, expected {expected}")));
    }
    let staged = dir.join("new");
    std::fs::create_dir_all(&staged).map_err(|e| Problem::new("Couldn't unpack the update.", e))?;
    let out = quiet(system_tool("tar"))
        .arg("-xf")
        .arg(&zip)
        .arg("-C")
        .arg(&staged)
        .output()
        .map_err(|e| Problem::new("Couldn't unpack the update (tar, which comes with Windows 10 and later, didn't start).", e))?;
    if !out.status.success() {
        return Err(Problem::new("Couldn't unpack the update.", String::from_utf8_lossy(&out.stderr)));
    }
    // The program sits at the top of the zip, or in one folder there.
    let exe = program_name();
    let root = std::iter::once(staged.clone())
        .chain(std::fs::read_dir(&staged).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()))
        .find(|d| d.join(&exe).is_file())
        .ok_or_else(|| Problem::new("The update didn't contain the program.", format!("no {} in {}", exe.to_string_lossy(), staged.display())))?;
    Ok(root)
}

/// This program's file name (trackertools.exe).
fn program_name() -> std::ffi::OsString {
    std::env::current_exe().ok().and_then(|e| e.file_name().map(|n| n.to_owned())).unwrap_or_else(|| format!("trackertools{}", std::env::consts::EXE_SUFFIX).into())
}

fn old_name(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(".old");
    path.with_file_name(name)
}

/// Moves every file of `staged` into `home` (keeping folders), each file it
/// replaces renamed to `<name>.old` first; on a failure, puts back what it moved.
pub fn install_into(staged: &Path, home: &Path) -> Result<(), Problem> {
    fn files(dir: &Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() { files(&p, out) } else { out.push(p) }
        }
    }
    let mut todo = Vec::new();
    files(staged, &mut todo);
    let mut done: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();
    let undo = |done: &[(PathBuf, Option<PathBuf>)]| {
        for (dst, old) in done.iter().rev() {
            let _ = std::fs::remove_file(dst);
            if let Some(old) = old {
                let _ = std::fs::rename(old, dst);
            }
        }
    };
    for src in todo {
        let dst = home.join(src.strip_prefix(staged).expect("inside staged"));
        let step = || -> std::io::Result<Option<PathBuf>> {
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let old = if dst.exists() {
                let old = old_name(&dst);
                let _ = std::fs::remove_file(&old);
                std::fs::rename(&dst, &old)?;
                Some(old)
            } else {
                None
            };
            // A rename, or a copy when the temp folder is on another drive.
            if std::fs::rename(&src, &dst).is_err()
                && let Err(e) = std::fs::copy(&src, &dst)
            {
                if let Some(old) = &old {
                    let _ = std::fs::rename(old, &dst);
                }
                return Err(e);
            }
            Ok(old)
        };
        match step() {
            Ok(old) => done.push((dst, old)),
            Err(e) => {
                undo(&done);
                return Err(Problem::new(
                    "Couldn't replace trackertools' files (is its folder read-only?). Move the trackertools folder into your Documents and try again.",
                    format!("{}: {e}", dst.display()),
                ));
            }
        }
    }
    Ok(())
}

/// Deletes what the last update renamed aside (`<name>.old` next to a `<name>`).
pub fn clean_up_after_update(home: &Path) {
    fn walk(dir: &Path, depth: usize) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() && depth > 0 {
                walk(&p, depth - 1);
            } else if p.extension().is_some_and(|x| x == "old") && p.with_extension("").exists() {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
    walk(home, 2);
}

/// Every frame: keeps the window drawing while there's progress to show, and
/// installs a finished download: files in place, then save, close, and start
/// the new version (in `Shell::on_exit`).
pub fn drive(ctx: &egui::Context, world: &mut World) {
    let up = world.resource::<Updater>().clone();
    match up.state() {
        State::Checking | State::Downloading { .. } => ctx.request_repaint_after(Duration::from_millis(150)),
        State::Ready { staged, .. } => {
            let Some(home) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) else { return };
            match install_into(&staged, &home) {
                Ok(()) => {
                    world.resource_mut::<RestartWith>().0 = Some(home.join(program_name()));
                    up.set(State::Restarting);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                Err(e) => up.set(State::Failed(e)),
            }
        }
        _ => {}
    }
}

/// GitHub's generated release notes as plain lines: no headings or the
/// "Full Changelog" link, bullets as bullets, and no "by @someone in <link>"
/// after each change.
pub fn plain_notes(notes: &str) -> String {
    notes
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("**Full Changelog**"))
        .map(|l| {
            let l = l.strip_prefix("* ").or_else(|| l.strip_prefix("- ")).map_or_else(|| l.to_string(), |rest| format!("\u{2022} {rest}"));
            match l.rfind(" by @") {
                Some(i) if l[i..].contains(" in http") => l[..i].to_string(),
                _ => l,
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// When a new version is found (at start, or by Check for updates): a
/// window asks once, as most programs do, with what's new. Update and
/// restart, Later (until the next start; the dot on the Settings tab and the
/// top bar's button stay), or Skip this version (no more asking for it).
pub fn prompt(ctx: &egui::Context, world: &mut World) {
    let up = world.resource::<Updater>().clone();
    let Some(release) = up.available() else { return };
    if up.skipped.as_deref() == Some(release.version.as_str()) || up.later.as_deref() == Some(release.version.as_str()) {
        return;
    }
    let mut answer = None;
    egui::Window::new("A new version of trackertools")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ctx, |ui| {
            ui.set_max_width(420.0);
            ui.label(egui::RichText::new(format!("trackertools {} is out. You have {}.", release.version, version())).strong());
            ui.label("Updating saves your work, closes trackertools and opens the new version. It takes about a minute.");
            if !release.notes.is_empty() {
                ui.add_space(4.0);
                egui::CollapsingHeader::new("What's new").default_open(true).show(ui, |ui| {
                    egui::ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
                        ui.label(plain_notes(&release.notes));
                    });
                });
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button(egui::RichText::new("Update and restart").strong()).clicked() {
                    answer = Some(0);
                }
                if ui.button("Later").on_hover_text("Ask again when trackertools starts. The Settings tab and the top bar keep a way to update.").clicked() {
                    answer = Some(1);
                }
                if ui.button("Skip this version").on_hover_text("Don't ask about this version again. You can still update from Settings, Updates.").clicked() {
                    answer = Some(2);
                }
            });
        });
    let mut up = world.resource_mut::<Updater>();
    match answer {
        Some(0) => up.update(release),
        Some(1) => up.later = Some(release.version),
        Some(2) => up.skipped = Some(release.version),
        _ => {}
    }
}

/// After the app saved and is closing: start the new version, if one was installed.
pub fn restart_if_updated(world: &World) {
    if let Some(exe) = &world.resource::<RestartWith>().0 {
        let mut cmd = Command::new(exe);
        // (Not a start after an error, even if this one was.)
        cmd.env_remove(crate::recover::RECOVERED);
        if let Some(dir) = exe.parent() {
            cmd.current_dir(dir);
        }
        if let Err(e) = cmd.spawn() {
            tracing::error!("could not start the updated trackertools ({}): {e}", exe.display());
        }
    }
}

pub struct UpdateModule;

impl Module for UpdateModule {
    fn build(&self, app: &mut AppBuilder) {
        app.declare::<Updater>(Class::Session).init_resource::<Updater>().declare::<RestartWith>(Class::Session).init_resource::<RestartWith>();
    }
}

/// At start: tidy what the last update left, and (a released copy, if the
/// setting is on) look for a new version quietly.
pub fn on_start(world: &World) {
    // (A copy built from source lives in cargo's target folder: left alone.)
    if !installable() {
        return;
    }
    if let Some(home) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        clean_up_after_update(&home);
    }
    let up = world.resource::<Updater>();
    if up.check_on_start {
        up.check(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number() {
        assert!(newer("0.2.0", "0.1.0"));
        assert!(newer("0.1.10", "0.1.9"));
        assert!(newer("v1.0.0", "0.9.9"));
        assert!(newer("0.2.0", "0.1.0-dev"));
        assert!(newer("0.1.0", "0.1.0-dev"), "a release beats the build from source it came from");
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.1.9", "0.1.10"));
        assert!(!newer("0.1", "0.1.0"));
    }

    #[test]
    fn a_release_reads_with_this_computers_download() {
        let name = asset_name().unwrap_or("none");
        let json = format!(
            r#"{{"tag_name": "v0.3.1", "html_url": "https://github.com/x/y/releases/tag/v0.3.1", "body": "Sketches work as trackers.\n",
                "assets": [{{"name": "other.zip", "browser_download_url": "https://x/other.zip", "size": 1}},
                           {{"name": "{name}", "browser_download_url": "https://x/{name}", "size": 1234, "digest": "sha256:ABC123"}}]}}"#
        );
        let r = parse_release(&json).expect("reads");
        assert_eq!((r.version.as_str(), r.notes.as_str()), ("0.3.1", "Sketches work as trackers."));
        if asset_name().is_some() {
            let d = r.download.expect("a download");
            assert_eq!((d.url, d.size, d.sha256.as_deref()), (format!("https://x/{name}"), 1234, Some("abc123")));
        }
        let none = parse_release(r#"{"tag_name": "v0.3.2", "assets": []}"#).expect("reads");
        assert_eq!(none.download, None, "no download for this computer");
    }

    /// The whole path but the restart, offline: a release feed and its zip as
    /// files, downloaded, unpacked, and installed over an older copy.
    #[cfg(windows)]
    #[test]
    fn an_update_downloads_unpacks_and_replaces_the_files() {
        let root = std::env::temp_dir().join(format!("tt-update-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (pkg, home) = (root.join("pkg"), root.join("home"));
        std::fs::create_dir_all(pkg.join("docs")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let exe = program_name();
        std::fs::write(pkg.join(&exe), b"new program").unwrap();
        std::fs::write(pkg.join("ffmpeg.exe"), b"new ffmpeg").unwrap();
        std::fs::write(pkg.join("docs").join("README.txt"), b"read me").unwrap();
        std::fs::write(home.join(&exe), b"old program").unwrap();
        std::fs::write(home.join("keep.txt"), b"mine").unwrap();
        // The zip, as the release workflow makes it (files at its top).
        let zip = root.join("trackertools-windows-x64.zip");
        let made = Command::new(system_tool("tar")).arg("-a").arg("-cf").arg(&zip).arg("-C").arg(&pkg).arg(".").status().unwrap();
        assert!(made.success());
        let size = std::fs::metadata(&zip).unwrap().len();
        let url = format!("file:///{}", zip.display().to_string().replace('\\', "/"));
        let feed = root.join("latest.json");
        let digest = crate::setup::sha256(&zip).unwrap();
        std::fs::write(&feed, format!(r#"{{"tag_name": "v9.9.9", "assets": [{{"name": "trackertools-windows-x64.zip", "browser_download_url": "{url}", "size": {size}, "digest": "sha256:{digest}"}}]}}"#)).unwrap();

        let release = fetch_release(&format!("file:///{}", feed.display().to_string().replace('\\', "/"))).expect("the feed reads");
        assert_eq!(release.version, "9.9.9");
        let file = release.download.expect("a download");
        let size = file.size;
        // A download that isn't the published file is refused (and one with no checksum at all).
        let wrong = Download { sha256: Some("0".repeat(64)), ..file.clone() };
        let refused = download(&wrong, &root.join("dl"), |_| ()).expect_err("a wrong checksum is refused");
        assert!(refused.details.contains("expected 000"), "{}", refused.details);
        let unchecked = Download { sha256: None, sha256_url: None, ..file.clone() };
        assert!(download(&unchecked, &root.join("dl"), |_| ()).is_err(), "no checksum: no update");
        // The `.sha256` file beside it counts when GitHub has no digest.
        std::fs::write(root.join("sidecar.sha256"), format!("{digest}  trackertools-windows-x64.zip\n")).unwrap();
        let sidecar = Download { sha256: None, sha256_url: Some(format!("file:///{}", root.join("sidecar.sha256").display().to_string().replace('\\', "/"))), ..file.clone() };
        assert!(download(&sidecar, &root.join("dl"), |_| ()).is_ok(), "checked against the .sha256 file");
        let seen = std::sync::Mutex::new(0);
        let staged = download(&file, &root.join("dl"), |n| *seen.lock().unwrap() = n).expect("downloads and unpacks");
        assert_eq!(*seen.lock().unwrap(), size, "progress reaches the whole size");
        install_into(&staged, &home).expect("installs");
        assert_eq!(std::fs::read(home.join(&exe)).unwrap(), b"new program");
        assert_eq!(std::fs::read(home.join("ffmpeg.exe")).unwrap(), b"new ffmpeg");
        assert_eq!(std::fs::read(home.join("docs").join("README.txt")).unwrap(), b"read me");
        assert_eq!(std::fs::read(home.join("keep.txt")).unwrap(), b"mine", "files the update doesn't have stay");
        assert_eq!(std::fs::read(old_name(&home.join(&exe))).unwrap(), b"old program", "the old program is set aside");
        clean_up_after_update(&home);
        assert!(!old_name(&home.join(&exe)).exists(), "and deleted at the next start");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn only_https_addresses_are_used() {
        assert!(protocols("https://github.com/x").is_ok());
        assert_eq!(protocols("HTTPS://github.com/x").unwrap(), ["--proto", "=https", "--proto-redir", "=https"], "redirects stay on https");
        assert!(protocols("http://github.com/x").is_err());
        assert!(protocols("ftp://x/y").is_err());
    }

    #[test]
    fn release_notes_read_as_plain_lines() {
        let notes = "## What's Changed\n* Several projects on one video by @euvinkeel in https://github.com/x/y/pull/17\n* Sketch colours\n\n**Full Changelog**: https://github.com/x/y/compare/v0.2.0...v0.2.1";
        assert_eq!(plain_notes(notes), "\u{2022} Several projects on one video\n\u{2022} Sketch colours");
    }
}
