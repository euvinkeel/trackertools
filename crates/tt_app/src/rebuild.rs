//! Build from your code and restart (Settings → Updates) *(on request: "how
//! do i instantly test the new trackertools … can we have a button for
//! that, local restart button")*.
//!
//! Only a copy built on a computer that still has its source checkout shows
//! it (the folder it was built from, known at compile time, with
//! `scripts/package_windows.ps1` and `.git`): a released copy built on
//! GitHub's machines doesn't. It can switch the checkout to another branch
//! first (local ones, and GitHub's after Fetch: a pull request's branch, to
//! try it before merging), when nothing in the checkout is uncommitted. The
//! build is the packaging script's (`-InstallTo` this program's folder,
//! versioned as the latest tag), so the first one takes minutes and later
//! ones less. When it is done trackertools saves, closes and starts the new
//! build, as an update does ([`crate::update::RestartWith`]).

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use bevy_ecs::prelude::*;

use crate::update::{quiet, system_tool};

/// Lines of the build's output kept to show.
const LOG_LINES: usize = 400;

/// The source checkout this program was built from, if this computer still has it.
pub fn source_dir() -> Option<PathBuf> {
    if !cfg!(windows) {
        return None;
    }
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
    let dir = dir.canonicalize().ok()?;
    let dir = PathBuf::from(dir.to_string_lossy().trim_start_matches(r"\\?\"));
    (dir.join("scripts").join("package_windows.ps1").is_file() && dir.join(".git").exists()).then_some(dir)
}

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Idle,
    /// git (fetch, checkout) or the build running; its output so far.
    Running { what: String },
    /// Built and installed: restarting.
    Done,
    Failed(String),
}

/// The checkout as it is now (read when the Settings tab asks).
#[derive(Clone, Debug, Default)]
pub struct Checkout {
    pub branch: String,
    pub commit: String,
    /// `git status --porcelain` lines: uncommitted changes (switching is refused then).
    pub dirty: usize,
    pub local: Vec<String>,
    /// GitHub's branches (`origin/…`) not checked out here yet.
    pub remote: Vec<String>,
}

#[derive(Resource, Clone)]
pub struct Rebuild {
    state: Arc<Mutex<State>>,
    log: Arc<Mutex<Vec<String>>>,
    pub dir: Option<PathBuf>,
    pub checkout: Option<Checkout>,
}

impl Default for Rebuild {
    fn default() -> Self {
        Self { state: Arc::new(Mutex::new(State::Idle)), log: Arc::new(Mutex::new(Vec::new())), dir: source_dir(), checkout: None }
    }
}

impl Rebuild {
    pub fn state(&self) -> State {
        self.state.lock().expect("rebuild state").clone()
    }

    fn set(&self, s: State) {
        *self.state.lock().expect("rebuild state") = s;
    }

    pub fn busy(&self) -> bool {
        matches!(self.state(), State::Running { .. } | State::Done)
    }

    /// The build's output so far (its last lines).
    pub fn log(&self) -> Vec<String> {
        self.log.lock().expect("rebuild log").clone()
    }

    /// Read the checkout's branch, commit and branches again.
    pub fn refresh(&mut self) {
        self.checkout = self.dir.as_deref().map(read_checkout);
    }

    /// `git fetch --prune` in the background (GitHub's branches), then refresh on the next look.
    pub fn fetch(&self) {
        let Some(dir) = self.dir.clone() else { return };
        self.run("Getting GitHub's branches\u{2026}".to_string(), move |me| {
            git(&dir, &["fetch", "--prune", "origin"]).map(|out| me.push(&out)).map_err(|e| format!("git fetch failed: {e}"))
        });
    }

    /// Switch the checkout to `branch` (a local one, or `origin/x`: makes `x` following it).
    pub fn switch(&self, branch: &str) {
        let Some(dir) = self.dir.clone() else { return };
        let name = branch.strip_prefix("origin/").unwrap_or(branch).to_string();
        self.run(format!("Switching to {name}\u{2026}"), move |me| {
            let out = git(&dir, &["checkout", &name]).map_err(|e| format!("git checkout {name} failed: {e}"))?;
            me.push(&out);
            // A branch that follows GitHub's: bring it up to date (fast-forward only: never a merge).
            if let Ok(out) = git(&dir, &["pull", "--ff-only"]) {
                me.push(&out);
            }
            Ok(())
        });
    }

    /// Build the checkout as it is, put it in place of this program, and restart with it.
    pub fn build_and_restart(&self) {
        let (Some(dir), Some(home)) = (self.dir.clone(), std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf))) else { return };
        self.log.lock().expect("rebuild log").clear();
        let me = self.clone();
        self.set(State::Running { what: "Building\u{2026} (the first time takes several minutes)".into() });
        std::thread::spawn(move || {
            let tag = git(&dir, &["describe", "--tags", "--abbrev=0"]).ok().map(|t| t.trim().to_string()).filter(|t| t.starts_with('v')).unwrap_or_else(|| "v0.1.0".into());
            let script = dir.join("scripts").join("package_windows.ps1");
            let mut cmd = quiet(system_tool("WindowsPowerShell\\v1.0\\powershell"));
            cmd.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]).arg(&script).arg("-Version").arg(&tag).arg("-InstallTo").arg(&home);
            cmd.current_dir(&dir).stdout(Stdio::piped()).stderr(Stdio::piped());
            let status = match cmd.spawn() {
                Ok(mut child) => {
                    let err = child.stderr.take().map(|e| {
                        let me = me.clone();
                        std::thread::spawn(move || BufReader::new(e).lines().map_while(Result::ok).for_each(|l| me.push(&l)))
                    });
                    if let Some(out) = child.stdout.take() {
                        BufReader::new(out).lines().map_while(Result::ok).for_each(|l| me.push(&l));
                    }
                    let _ = err.map(|t| t.join());
                    child.wait().map_err(|e| e.to_string())
                }
                Err(e) => Err(e.to_string()),
            };
            match status {
                Ok(s) if s.success() && home.join("trackertools.exe").is_file() => me.set(State::Done),
                Ok(s) => me.set(State::Failed(format!("The build stopped ({s}). The log below says why."))),
                Err(e) => me.set(State::Failed(format!("Couldn't start the build: {e}"))),
            }
        });
    }

    /// Run `work` in the background as the state `what`.
    fn run(&self, what: String, work: impl FnOnce(&Rebuild) -> Result<(), String> + Send + 'static) {
        if self.busy() {
            return;
        }
        self.set(State::Running { what });
        let me = self.clone();
        std::thread::spawn(move || match work(&me) {
            Ok(()) => me.set(State::Idle),
            Err(e) => me.set(State::Failed(e)),
        });
    }

    fn push(&self, text: &str) {
        let mut log = self.log.lock().expect("rebuild log");
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            log.push(line.trim_end().to_string());
        }
        let n = log.len();
        if n > LOG_LINES {
            log.drain(..n - LOG_LINES);
        }
    }
}

/// `git <args>` in `dir`; its output (stdout and stderr), or why it failed.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = quiet(PathBuf::from("git")).arg("-c").arg("safe.directory=*").args(args).current_dir(dir).output().map_err(|e| e.to_string())?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if out.status.success() { Ok(text) } else { Err(text.trim().to_string()) }
}

fn read_checkout(dir: &Path) -> Checkout {
    let lines = |args: &[&str]| git(dir, args).map(|t| t.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect::<Vec<_>>()).unwrap_or_default();
    let branch = lines(&["rev-parse", "--abbrev-ref", "HEAD"]).into_iter().next().unwrap_or_default();
    let commit = lines(&["log", "-1", "--format=%h %s"]).into_iter().next().unwrap_or_default();
    let dirty = lines(&["status", "--porcelain", "--untracked-files=no"]).len();
    let local = lines(&["branch", "--format=%(refname:short)"]);
    let remote = lines(&["branch", "-r", "--format=%(refname:short)"])
        .into_iter()
        .filter(|r| r.starts_with("origin/") && r != "origin/HEAD" && r != "origin" && !local.iter().any(|l| Some(l.as_str()) == r.strip_prefix("origin/")))
        .collect();
    Checkout { branch, commit, dirty, local, remote }
}

/// Every app frame: a finished build restarts trackertools with it.
pub fn drive(ctx: &egui::Context, world: &mut World) {
    let rb = world.resource::<Rebuild>().clone();
    match rb.state() {
        State::Running { .. } => ctx.request_repaint_after(std::time::Duration::from_millis(250)),
        State::Done => {
            if world.resource::<crate::update::RestartWith>().0.is_none()
                && let Some(home) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf))
            {
                tracing::info!("built from the source checkout: restarting");
                world.resource_mut::<crate::update::RestartWith>().0 = Some(home.join("trackertools.exe"));
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        _ => {}
    }
}

pub struct RebuildModule;

impl tt_core::Module for RebuildModule {
    fn build(&self, app: &mut tt_core::AppBuilder) {
        app.declare::<Rebuild>(tt_core::Class::Session).init_resource::<Rebuild>();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This test runs in the source checkout: it is found, and its branch is one of its branches.
    #[test]
    fn the_checkout_is_found_and_read() {
        let Some(dir) = source_dir() else { return };
        let co = read_checkout(&dir);
        assert!(!co.commit.is_empty(), "a commit");
        assert!(co.branch == "HEAD" || co.local.contains(&co.branch), "{} in {:?}", co.branch, co.local);
        assert!(co.remote.iter().all(|r| r.starts_with("origin/")));
    }
}
