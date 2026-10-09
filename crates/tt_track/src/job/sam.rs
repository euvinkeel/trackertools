//! SAM 2.1 (Meta, Apache-2.0) for cursor trackers: which pixels of a paint
//! are the cursor (`editor/sam_worker.py`, run by the doctor's Python, the
//! one CoTracker's worker runs on). One worker for the app, started the
//! first time a paint is learned and kept for the next; requests one at a
//! time. Without SAM set up, cursor trackers learn without it.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};

/// SAM 2.1's small model (`sam2.1_hiera_small.pt`): `TT_SAM_WEIGHTS`, else the doctor's download.
pub fn weights() -> PathBuf {
    std::env::var_os("TT_SAM_WEIGHTS").map(PathBuf::from).unwrap_or_else(|| super::learned::installed_dir().join("sam2.1_hiera_small.pt"))
}

/// The worker script: next to CoTracker's.
pub fn worker() -> PathBuf {
    let (_, cotracker) = super::learned::worker_command();
    cotracker.with_file_name("sam_worker.py")
}

/// Whether SAM can run here: CoTracker's Python (SAM runs on it), the
/// worker script and the weights. (Whether that Python has the `sam2`
/// package shows when the worker starts.) Err: what's missing, for the person.
pub fn availability() -> Result<(), String> {
    super::learned::availability().map_err(|_| "SAM 2 needs CoTracker's Python: CoTracker is not set up on this computer.".to_string())?;
    if !worker().is_file() || !weights().is_file() {
        return Err("SAM 2 is not set up on this computer.".into());
    }
    Ok(())
}

/// A candidate mask: crop pixels, row-major (true: the cursor), and SAM's score for it.
pub type Candidate = (Vec<bool>, f32);

struct Worker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The worker, once started; Err: it couldn't start (not tried again until [`forget`]).
static WORKER: Mutex<Option<std::result::Result<Worker, String>>> = Mutex::new(None);

fn start() -> std::result::Result<Worker, String> {
    let (python, _) = super::learned::worker_command();
    let mut cmd = Command::new(&python);
    cmd.arg(worker()).arg("--weights").arg(weights()).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());
    // (Development: SAM's package from elsewhere than the Python's own.)
    if let Some(extra) = std::env::var_os("TT_SAM_PYTHONPATH") {
        cmd.env("PYTHONPATH", extra);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn().map_err(|e| format!("SAM 2 does not start: {}: {e}", python.display()))?;
    let stdin = child.stdin.take().ok_or("no stdin")?;
    let mut stdout = BufReader::new(child.stdout.take().ok_or("no stdout")?);
    let mut line = String::new();
    stdout.read_line(&mut line).map_err(|e| format!("SAM 2 does not start: {e}"))?;
    let msg: serde_json::Value = serde_json::from_str(&line).map_err(|_| format!("SAM 2 does not start: it said {line:?}"))?;
    if let Some(e) = msg.get("error").and_then(|e| e.as_str()) {
        return Err(e.to_string());
    }
    tracing::info!("SAM 2 ready on {}", msg["ready"]["device"].as_str().unwrap_or("?"));
    Ok(Worker { child, stdin, stdout })
}

/// Start again next time (something was installed, or it stopped).
pub fn forget() {
    *WORKER.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// SAM's candidate masks for a `w × h` RGB crop, prompted with `points`
/// (crop pixels; true: on the cursor, false: not), the crop enlarged
/// `scale` times first.
pub fn segment(rgb: &[u8], w: usize, h: usize, points: &[([f64; 2], bool)], scale: u32) -> Result<Vec<Candidate>> {
    if rgb.len() != w * h * 3 {
        bail!("a crop of {} bytes is not {w} × {h} RGB", rgb.len());
    }
    let mut held = WORKER.lock().unwrap_or_else(|e| e.into_inner());
    if held.is_none() {
        *held = Some(start());
    }
    let worker = match held.as_mut().expect("started") {
        Ok(w) => w,
        Err(e) => bail!("{e}"),
    };
    let asked = (|| -> Result<Vec<Candidate>> {
        let pts: Vec<[f64; 3]> = points.iter().map(|(p, on)| [p[0], p[1], if *on { 1.0 } else { 0.0 }]).collect();
        let req = serde_json::json!({ "w": w, "h": h, "points": pts, "scale": scale });
        worker.stdin.write_all(format!("{req}\n").as_bytes())?;
        worker.stdin.write_all(rgb)?;
        worker.stdin.flush()?;
        let mut line = String::new();
        worker.stdout.read_line(&mut line)?;
        let msg: serde_json::Value = serde_json::from_str(&line).with_context(|| format!("SAM 2 said {line:?}"))?;
        if let Some(e) = msg.get("error").and_then(|e| e.as_str()) {
            bail!("SAM 2: {e}");
        }
        let n = msg["n"].as_u64().context("no masks")? as usize;
        let scores: Vec<f32> = msg["scores"].as_array().map(|a| a.iter().map(|s| s.as_f64().unwrap_or(0.0) as f32).collect()).unwrap_or_default();
        let mut out = Vec::with_capacity(n);
        let mut buf = vec![0u8; w * h];
        for i in 0..n {
            worker.stdout.read_exact(&mut buf)?;
            out.push((buf.iter().map(|b| *b != 0).collect(), scores.get(i).copied().unwrap_or(0.0)));
        }
        Ok(out)
    })();
    // (A worker that broke mid-request starts again next time.)
    if asked.is_err() {
        *held = None;
    }
    asked
}
