//! Setting CoTracker up on a computer that only has the program (the
//! doctor, `crate::setup`). CoTracker3 runs in a Python worker
//! (tt_track::job::learned) with PyTorch on an NVIDIA graphics card (Windows)
//! or on Apple silicon's graphics (macOS: PyTorch's MPS). All of it goes into
//! `cotracker\` in the data folder (`tt_track::job::cotracker_dir`):
//!
//! - `uv\`: uv, Astral's single-file Python manager, from its GitHub release
//!   (checked against its SHA-256). It needs no admin rights and changes
//!   nothing else on the computer.
//! - `python\`, `env\`: a Python 3.12 of uv's own, and an environment with
//!   PyTorch 2.14.0 built for the card: CUDA 13.0 for Blackwell (the RTX 50
//!   cards; the CUDA 12.6 build has no code for them), else CUDA 12.6, what
//!   the worker was developed with; on a Mac, PyPI's own build, which has
//!   MPS. And the worker's other packages (NumPy,
//!   PyAV, OpenCV), at the versions it was developed with. pip installs
//!   them (from the Python itself, `ensurepip`), not uv: uv unpacks into its
//!   cache and then renames the folder, which Windows refused ("Access is
//!   denied") for PyTorch's thousands of new DLLs while Defender scanned
//!   them; pip unpacks in place. Its temporary folder (the download's
//!   progress) and uv's cache go afterwards.
//! - `code\`: the worker's code, packed into the program (build.rs).
//! - `scaled_online.pth`: Meta's CoTracker3 model from Hugging Face, checked
//!   against its SHA-256. CC BY-NC 4.0, non-commercial use only: the doctor
//!   says so before.
//!
//! Before all of it, Windows' Visual C++ runtime, if it is missing or older
//! than what PyTorch was built with: PyTorch's DLLs load it from Windows
//! (`msvcp140.dll`, `msvcp140_atomic_wait.dll`), and uv's Python doesn't
//! bring it. Microsoft's installer (checked: signed by Microsoft) puts it
//! there, after Windows asks the person for permission.
//!
//! Then the worker starts once, to see that it loads the model on the card
//! (on a Mac, on MPS, or on the processor when MPS does not work).
//! Every step says what it does in a sentence (ASD-STE100, as all of setup).
//! `TT_COTRACKER_CUDA=cu126|cu130` picks the PyTorch build, and
//! `TT_VC_RUNTIME=<version>|none` pretends a Visual C++ runtime (tests).

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bevy_ecs::prelude::*;

use crate::setup::{download, fetch_text, find, sha256};
use crate::update::{Problem, quiet, system_tool};

mod code {
    include!(concat!(env!("OUT_DIR"), "/cotracker_code.rs"));
}

/// uv's release download for this computer (a `.sha256` is beside it).
pub const UV_ARCHIVE: &str = if cfg!(target_os = "macos") {
    "https://github.com/astral-sh/uv/releases/latest/download/uv-aarch64-apple-darwin.tar.gz"
} else {
    "https://github.com/astral-sh/uv/releases/latest/download/uv-x86_64-pc-windows-msvc.zip"
};
/// trackertools can set CoTracker up on this computer: Windows on x64, macOS on Apple silicon.
pub const CAN_SET_UP: bool = cfg!(any(all(windows, target_arch = "x86_64"), all(target_os = "macos", target_arch = "aarch64")));
pub const MODEL: &str = "https://huggingface.co/facebook/cotracker3/resolve/main/scaled_online.pth";
/// The model's SHA-256, as Hugging Face publishes it (and v1's copy has it).
pub const MODEL_SHA256: &str = "205d34789f19699d64b22cf93f9b697f15f28d4025240e31532e504109837218";
pub const MODEL_BYTES: u64 = 101_695_610;
const PYTHON: &str = "3.12";
const TORCH: &str = "torch==2.14.0";
const PACKAGES: [&str; 3] = ["numpy==2.5.2", "av==18.1.0", "opencv-python-headless==5.0.0.93"];
/// Microsoft's installer for the newest Visual C++ runtime (its permanent link).
pub const VC_RUNTIME: &str = "https://aka.ms/vc14/vc_redist.x64.exe";
/// PyTorch 2.14.0's DLLs are linked with MSVC 14.42, and a program needs a
/// Visual C++ runtime at least as new as what built it.
const VC_RUNTIME_MIN: (u32, u32) = (14, 42);
/// What PyTorch's DLLs load from it, in System32.
const VC_RUNTIME_DLLS: [&str; 4] = ["vcruntime140.dll", "vcruntime140_1.dll", "msvcp140.dll", "msvcp140_atomic_wait.dll"];

const AGAIN: &str = "Then click Set up CoTracker again.";
const ASK: &str = "Click Copy report. Send the report to the person who gave you trackertools.";

/// The graphics CoTracker uses: an NVIDIA card, as its driver's
/// `nvidia-smi` tells, or a Mac's Apple silicon.
#[derive(Clone, Debug, PartialEq)]
pub struct Gpu {
    pub name: String,
    /// CUDA compute capability (Blackwell's RTX 50 cards: 12.0). (0, 0) for Apple silicon.
    pub compute: (u32, u32),
    /// The NVIDIA driver's version, or macOS's.
    pub driver: String,
    /// Apple silicon's graphics (PyTorch's MPS), not an NVIDIA card.
    pub apple: bool,
}

/// The graphics CoTracker can use on this computer: an NVIDIA card on
/// Windows, Apple silicon on a Mac.
pub fn gpu() -> Option<Gpu> {
    if cfg!(windows) {
        nvidia_gpu()
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        apple_gpu()
    } else {
        None
    }
}

/// The Mac's chip (`Apple M3 Pro`) and macOS's version.
fn apple_gpu() -> Option<Gpu> {
    let said = |program: &str, args: &[&str]| {
        let out = quiet(PathBuf::from(program)).args(args).output().ok().filter(|o| o.status.success())?;
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|s| !s.is_empty())
    };
    let name = said("/usr/sbin/sysctl", &["-n", "machdep.cpu.brand_string"]).filter(|n| n.starts_with("Apple"))?;
    let driver = said("/usr/bin/sw_vers", &["-productVersion"]).map_or_else(|| "macOS".into(), |v| format!("macOS {v}"));
    Some(Gpu { name, compute: (0, 0), driver, apple: true })
}

/// The computer's NVIDIA graphics card, if it has one with a driver.
pub fn nvidia_gpu() -> Option<Gpu> {
    let smi = [system_tool("nvidia-smi"), PathBuf::from(r"C:\Program Files\NVIDIA Corporation\NVSMI\nvidia-smi.exe")]
        .into_iter()
        .find(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("nvidia-smi"));
    let out = quiet(smi).args(["--query-gpu=name,compute_cap,driver_version", "--format=csv,noheader"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_gpu(&String::from_utf8_lossy(&out.stdout))
}

/// The first card of `nvidia-smi --query-gpu=name,compute_cap,driver_version --format=csv,noheader`.
fn parse_gpu(text: &str) -> Option<Gpu> {
    let line = text.lines().find(|l| !l.trim().is_empty())?;
    let parts: Vec<&str> = line.split(',').map(str::trim).collect();
    let (major, minor) = parts.get(1)?.split_once('.')?;
    Some(Gpu { name: parts.first()?.to_string(), compute: (major.parse().ok()?, minor.parse().ok()?), driver: parts.get(2)?.to_string(), apple: false })
}

/// The PyTorch build a card needs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Plan {
    /// PyTorch's index for it (`cu130`); None: PyPI's own build (a Mac's, with MPS).
    pub index: Option<&'static str>,
    /// What it is built for, for people (`CUDA 13.0`).
    pub label: &'static str,
    /// The device the worker must load on (`cuda`, `mps`).
    pub device: &'static str,
    /// The oldest NVIDIA driver that runs it.
    pub min_driver: u32,
    /// PyTorch's download, for people (`2 GB`), and all of the setup's.
    pub torch_size: &'static str,
    pub total_size: &'static str,
}

const CUDA_13: Plan = Plan { index: Some("cu130"), label: "CUDA 13.0", device: "cuda", min_driver: 580, torch_size: "2 GB", total_size: "2.5 GB" };
const CUDA_12: Plan = Plan { index: Some("cu126"), label: "CUDA 12.6", device: "cuda", min_driver: 528, torch_size: "2 GB", total_size: "2.5 GB" };
const MPS: Plan = Plan { index: None, label: "Apple silicon", device: "mps", min_driver: 0, torch_size: "130 MB", total_size: "0.3 GB" };

/// The PyTorch build for `gpu`, or why it can't run CoTracker (a sentence for the person).
pub fn plan(gpu: &Gpu) -> Result<Plan, String> {
    if gpu.apple {
        return Ok(MPS);
    }
    let plan = match std::env::var("TT_COTRACKER_CUDA").as_deref() {
        Ok("cu130") => CUDA_13,
        Ok("cu126") => CUDA_12,
        _ => match gpu.compute.0 {
            10.. => CUDA_13,
            5..=9 => CUDA_12,
            _ => return Err(format!("The {} is too old for CoTracker.", gpu.name)),
        },
    };
    let driver: u32 = gpu.driver.split('.').next().and_then(|d| d.parse().ok()).unwrap_or(0);
    if driver < plan.min_driver {
        return Err(format!(
            "CoTracker needs NVIDIA driver {} or later. This computer has driver {}. Update the NVIDIA driver. {AGAIN}",
            plan.min_driver, gpu.driver
        ));
    }
    Ok(plan)
}

/// Windows' Visual C++ runtime, as PyTorch needs it.
#[derive(Clone, Debug, PartialEq)]
pub enum Runtime {
    /// New enough (its version).
    Ready(String),
    /// Too old (its version): the setup installs the newest.
    Old(String),
    /// Not there: the setup installs it.
    Missing,
}

/// The Visual C++ runtime Windows has: the version its installer wrote in
/// the registry, if its DLLs are in System32.
pub fn vc_runtime() -> Runtime {
    let version = match std::env::var("TT_VC_RUNTIME") {
        Ok(v) => Some(v).filter(|v| v != "none"),
        Err(_) => quiet(system_tool("reg"))
            .args(["query", r"HKLM\SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\x64", "/reg:64"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| parse_vc_runtime(&String::from_utf8_lossy(&o.stdout)))
            .filter(|_| VC_RUNTIME_DLLS.iter().all(|d| system32().join(d).is_file())),
    };
    match version {
        Some(v) if new_enough(&v) => Runtime::Ready(v),
        Some(v) => Runtime::Old(v),
        None => Runtime::Missing,
    }
}

/// The version in `reg query` of the runtime's key (`v14.51.36247.00`), if it says it's installed.
fn parse_vc_runtime(text: &str) -> Option<String> {
    let value = |name: &str| text.lines().find_map(|l| {
        let mut words = l.split_whitespace();
        (words.next() == Some(name)).then(|| words.nth(1)).flatten()
    });
    (value("Installed") == Some("0x1")).then_some(())?;
    Some(value("Version")?.trim_start_matches('v').to_string())
}

fn new_enough(version: &str) -> bool {
    let mut parts = version.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    (parts.next().unwrap_or(0), parts.next().unwrap_or(0)) >= VC_RUNTIME_MIN
}

fn system32() -> PathBuf {
    std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from).join("System32")
}

/// Download Microsoft's installer for the Visual C++ runtime into `dir`,
/// check that Microsoft signed it, and run it: Windows asks the person for
/// permission. True: Windows uses the new runtime after a restart.
fn install_runtime(dir: &Path) -> Result<bool, Problem> {
    let stopped = |d: String| Problem::new(&format!("The download of the Microsoft Visual C++ runtime stopped. Make sure that the computer is connected to the internet. {AGAIN}"), d);
    let tmp = dir.join("tmp");
    std::fs::create_dir_all(&tmp).map_err(|e| stopped(e.to_string()))?;
    let exe = tmp.join("vc_redist.x64.exe");
    download(VC_RUNTIME, &exe, &|_| {}).map_err(|p| stopped(p.details))?;
    let ran = signed_by_microsoft(&exe)
        .map_err(|why| Problem::new(&format!("The download of the Microsoft Visual C++ runtime is not correct. {AGAIN}"), why))
        .and_then(|()| {
            tracing::info!("CoTracker setup: {} /install /passive /norestart", exe.display());
            quiet(exe.clone())
                .args(["/install", "/passive", "/norestart"])
                .status()
                .map_err(|e| Problem::new(&format!("trackertools cannot start the installation of the Microsoft Visual C++ runtime. {ASK}"), e))
        });
    let _ = std::fs::remove_file(&exe);
    let code = ran?.code().unwrap_or(-1);
    tracing::info!("CoTracker setup: the Visual C++ runtime's installer stopped with {code}");
    runtime_installed(code)
}

/// What the runtime installer's exit `code` means. True: Windows uses the new runtime after a restart.
fn runtime_installed(code: i32) -> Result<bool, Problem> {
    let what = format!("vc_redist.x64.exe exit code {code}");
    match code {
        // Installed; or a newer one is there already.
        0 | 1638 => Ok(false),
        3010 | 1641 => Ok(true),
        // Cancelled: the person said no when Windows asked for permission (ERROR_CANCELLED as itself and as an HRESULT).
        1602 | 1223 | -2_147_023_673 => Err(Problem::new(
            "Windows asked for permission to install the Microsoft Visual C++ runtime, and the permission was not given. CoTracker needs it. Click Set up CoTracker again. Then click Yes.",
            what,
        )),
        1618 => Err(Problem::new(&format!("Another installation is in progress on this computer. Wait until it is complete. {AGAIN}"), what)),
        _ => Err(Problem::new(&format!("trackertools cannot install the Microsoft Visual C++ runtime. {ASK}"), what)),
    }
}

/// Ok if Windows finds a good signature on `file`, and Microsoft's.
fn signed_by_microsoft(file: &Path) -> Result<(), String> {
    // (Started from PowerShell 7, the program has its module path, where Windows PowerShell can't load the command.)
    let out = quiet(system32().join(r"WindowsPowerShell\v1.0\powershell.exe"))
        .args(["-NoProfile", "-NonInteractive", "-Command", "$s = Get-AuthenticodeSignature -LiteralPath $env:TT_SIGNED; $s.Status.ToString() + '|' + $s.SignerCertificate.Subject"])
        .env("TT_SIGNED", file)
        .env_remove("PSModulePath")
        .output()
        .map_err(|e| format!("powershell: {e}"))?;
    let said = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if said.starts_with("Valid|") && said.contains("O=Microsoft Corporation") {
        Ok(())
    } else {
        Err(format!("the signature: {said} {}", String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// Where a CoTracker setup is.
#[derive(Clone, Debug, PartialEq, Default)]
pub enum Step {
    #[default]
    Idle,
    /// Windows' Visual C++ runtime (Windows asks for permission).
    Runtime,
    Uv,
    Python,
    /// PyTorch for the plan: bytes of it on the disk so far.
    Torch { plan: Plan, bytes: u64 },
    Packages,
    Code,
    Model { got: u64, total: u64 },
    Test,
    /// Ready on this card.
    Done(String),
    Failed(Problem),
}

impl Step {
    pub fn busy(&self) -> bool {
        !matches!(self, Step::Idle | Step::Done(_) | Step::Failed(_))
    }

    /// In a sentence, for the person.
    pub fn text(&self) -> String {
        let gb = |b: u64| b as f64 / 1e9;
        match self {
            Step::Idle => String::new(),
            Step::Runtime => "trackertools installs the Microsoft Visual C++ runtime from Microsoft (approximately 20 MB). Windows asks for permission. Click Yes.".into(),
            Step::Uv => "trackertools downloads uv, a Python installer (18 MB).".into(),
            Step::Python => "trackertools installs Python 3.12 for CoTracker.".into(),
            Step::Torch { plan, bytes } => {
                let now = if *bytes < 1_000_000_000 { format!("{:.0} MB", *bytes as f64 / 1e6) } else { format!("{:.1} GB", gb(*bytes)) };
                format!("trackertools downloads and installs PyTorch for {} (approximately {}): {now} on the disk now.", plan.label, plan.torch_size)
            }
            Step::Packages => "trackertools installs NumPy, PyAV and OpenCV.".into(),
            Step::Code => "trackertools copies the CoTracker code.".into(),
            Step::Model { got, total } => format!("trackertools downloads the CoTracker model: {:.0} MB of {:.0} MB.", *got as f64 / 1e6, *total as f64 / 1e6),
            Step::Test => "trackertools starts CoTracker on the graphics. This can take 1 minute.".into(),
            Step::Done(gpu) => format!("CoTracker is ready on the {gpu}."),
            Step::Failed(p) => p.what.clone(),
        }
    }

    /// A few words, for the top bar.
    pub fn short(&self) -> String {
        match self {
            Step::Runtime => "Visual C++".into(),
            Step::Uv | Step::Python => "Python".into(),
            Step::Torch { bytes, .. } => format!("PyTorch, {:.1} GB", *bytes as f64 / 1e9),
            Step::Packages => "packages".into(),
            Step::Code => "code".into(),
            Step::Model { got, total } => format!("model, {}%", (100 * got).checked_div(*total).unwrap_or(0)),
            Step::Test => "test".into(),
            other => other.text(),
        }
    }
}

/// A CoTracker setup running in the background, shared with the doctor.
#[derive(Clone, Default)]
pub struct Setup {
    step: Arc<Mutex<Step>>,
    started: Arc<Mutex<Option<Instant>>>,
    finished: Arc<Mutex<Option<Instant>>>,
}

impl Setup {
    pub fn step(&self) -> Step {
        self.step.lock().expect("step").clone()
    }

    /// Seconds since it started (while it runs).
    pub fn elapsed(&self) -> Option<u64> {
        self.started.lock().expect("started").map(|t| t.elapsed().as_secs())
    }

    /// Seconds since it finished, if it did.
    pub fn finished(&self) -> Option<u64> {
        self.finished.lock().expect("finished").map(|t| t.elapsed().as_secs())
    }

    /// Set CoTracker up for `gpu` in the data folder.
    pub fn start(&self, gpu: Gpu) {
        if self.step().busy() {
            return;
        }
        *self.step.lock().expect("step") = Step::Uv;
        *self.started.lock().expect("started") = Some(Instant::now());
        let me = self.clone();
        std::thread::spawn(move || {
            let dir = tt_track::job::cotracker_dir();
            tracing::info!("CoTracker setup for the {} (CUDA capability {}.{}, driver {}) in {}", gpu.name, gpu.compute.0, gpu.compute.1, gpu.driver, dir.display());
            let step = |s: Step| *me.step.lock().expect("step") = s;
            let done = install(&dir, &gpu, &step);
            tt_track::job::forget_cotracker_availability();
            match &done {
                Ok(device) => tracing::info!("CoTracker is set up: the worker loaded on {device}"),
                Err(p) => tracing::warn!("CoTracker setup: {} ({})", p.what, p.details),
            }
            step(match done {
                Ok(_) => Step::Done(gpu.name.trim_start_matches("NVIDIA ").to_string()),
                Err(p) => Step::Failed(p),
            });
            *me.started.lock().expect("started") = None;
            *me.finished.lock().expect("finished") = Some(Instant::now());
        });
    }
}

/// Set CoTracker up for `gpu` in `dir`, telling `step` each step. The
/// device the worker loaded the model on.
pub fn install(dir: &Path, gpu: &Gpu, step: &dyn Fn(Step)) -> Result<String, Problem> {
    let plan = plan(gpu).map_err(|why| Problem::new(&why, &gpu.name))?;
    let cannot_write = |e: std::io::Error| Problem::new("trackertools cannot write to its folder. Make sure that the disk has approximately 6 GB free. Then click Set up CoTracker again.", e);
    std::fs::create_dir_all(dir).map_err(cannot_write)?;

    // First, so that Windows asks for permission while the person reads the setup's steps.
    let mut restart = false;
    if cfg!(windows) && !matches!(vc_runtime(), Runtime::Ready(_)) {
        step(Step::Runtime);
        restart = install_runtime(dir)?;
    }

    step(Step::Uv);
    let uv = get_uv(dir)?;

    step(Step::Python);
    let env = dir.join("env");
    let python = env.join(if cfg!(windows) { "Scripts/python.exe" } else { "bin/python" });
    let no_python = |d: String| Problem::new(&format!("trackertools cannot install Python. Make sure that the computer is connected to the internet. {AGAIN}"), d);
    if !python.is_file() {
        let py = format!("--python={PYTHON}");
        run(uv_command(&uv, dir).args(["venv".as_ref(), env.as_os_str(), py.as_ref()]), None).map_err(no_python)?;
    }
    // pip comes with Python (no download).
    if !pip(&python, dir).arg("--version").output().is_ok_and(|o| o.status.success()) {
        run(python_command(&python, dir).args(["-m", "ensurepip", "--upgrade"]), None).map_err(no_python)?;
    }

    // pip downloads into its temporary folder (here), so its size is the download's progress.
    let tmp = dir.join("tmp");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(cannot_write)?;
    step(Step::Torch { plan, bytes: 0 });
    let mut torch = pip(&python, dir);
    torch.args(["install", "--no-cache-dir", TORCH]);
    if let Some(index) = plan.index {
        torch.args(["--index-url", &format!("https://download.pytorch.org/whl/{index}")]);
    }
    run(&mut torch, Some((&tmp, &|bytes| step(Step::Torch { plan, bytes }))))
        .map_err(|d| Problem::new(&format!("trackertools cannot install PyTorch. Make sure that the computer is connected to the internet and that the disk has approximately 6 GB free. {AGAIN}"), d))?;

    step(Step::Packages);
    run(pip(&python, dir).args(["install", "--no-cache-dir"]).args(PACKAGES), None)
        .map_err(|d| Problem::new(&format!("trackertools cannot install NumPy, PyAV and OpenCV. Make sure that the computer is connected to the internet. {AGAIN}"), d))?;

    step(Step::Code);
    let code = dir.join("code");
    write_code(&code).map_err(cannot_write)?;

    let model = dir.join("scaled_online.pth");
    if !sha256(&model).is_ok_and(|h| h == MODEL_SHA256) {
        step(Step::Model { got: 0, total: MODEL_BYTES });
        let part = dir.join("scaled_online.pth.part");
        download(MODEL, &part, &|got| step(Step::Model { got, total: MODEL_BYTES }))
            .map_err(|p| Problem::new(&format!("The download of the CoTracker model stopped. Make sure that the computer is connected to the internet. {AGAIN}"), p.details))?;
        let got = sha256(&part).map_err(cannot_write)?;
        if got != MODEL_SHA256 {
            let _ = std::fs::remove_file(&part);
            return Err(Problem::new(&format!("The CoTracker model download is not correct. {AGAIN}"), format!("SHA-256 {got}, expected {MODEL_SHA256}")));
        }
        let _ = std::fs::remove_file(&model);
        std::fs::rename(&part, &model).map_err(cannot_write)?;
    }

    step(Step::Test);
    let device = test_worker(&python, &code.join("editor").join("cotracker_worker.py"), &model).map_err(|p| match restart {
        true => Problem::new(&format!("Windows must restart to complete the installation of the Microsoft Visual C++ runtime. Restart the computer. {AGAIN}"), p.details),
        false => p,
    })?;
    // (On a Mac the worker uses the processor when MPS does not work: slower, but it tracks.)
    if device != plan.device && !(gpu.apple && device == "cpu") {
        return Err(Problem::new(&format!("CoTracker starts, but it cannot use the graphics card. Update the NVIDIA driver. {AGAIN}"), format!("the worker loaded on {device}")));
    }
    // pip's temporary folder and uv's cache (Python's download): the environment has everything it needs.
    let _ = std::fs::remove_dir_all(&tmp);
    let _ = std::fs::remove_dir_all(dir.join("cache"));
    Ok(device)
}

/// uv in `dir\uv` (downloaded and checked the first time).
fn get_uv(dir: &Path) -> Result<PathBuf, Problem> {
    get_uv_from(dir, UV_ARCHIVE)
}

fn get_uv_from(dir: &Path, url: &str) -> Result<PathBuf, Problem> {
    let home = dir.join("uv");
    let uv = home.join(format!("uv{}", std::env::consts::EXE_SUFFIX));
    if quiet(uv.clone()).arg("--version").output().is_ok_and(|o| o.status.success()) {
        return Ok(uv);
    }
    let stopped = |d: String| Problem::new(&format!("The download of uv stopped. Make sure that the computer is connected to the internet. {AGAIN}"), d);
    std::fs::create_dir_all(&home).map_err(|e| stopped(e.to_string()))?;
    // (A .zip on Windows, a .tar.gz on a Mac: tar opens both.)
    let zip = home.join(url.rsplit('/').next().unwrap_or("uv.zip"));
    download(url, &zip, &|_| {}).map_err(|p| stopped(p.details))?;
    let expected = fetch_text(&format!("{url}.sha256")).map_err(stopped)?;
    let expected = expected.split_whitespace().next().unwrap_or_default().to_ascii_lowercase();
    let got = sha256(&zip).map_err(|e| stopped(e.to_string()))?;
    if expected.len() != 64 || got != expected {
        return Err(Problem::new(&format!("The download of uv is not correct. {AGAIN}"), format!("SHA-256 {got}, expected {expected}")));
    }
    let out = quiet(system_tool("tar")).arg("-xf").arg(&zip).arg("-C").arg(&home).output().map_err(|e| stopped(e.to_string()))?;
    let _ = std::fs::remove_file(&zip);
    if !out.status.success() {
        return Err(stopped(String::from_utf8_lossy(&out.stderr).into_owned()));
    }
    // (The program may sit in a folder inside the zip.)
    if !uv.is_file()
        && let Some(found) = find(&home, &format!("uv{}", std::env::consts::EXE_SUFFIX))
    {
        let _ = std::fs::copy(found, &uv);
    }
    Ok(uv)
}

/// uv, with its own Python and cache in `dir`, and none of the person's own uv or Python settings.
fn uv_command(uv: &Path, dir: &Path) -> Command {
    let mut cmd: Command = quiet(uv.to_path_buf());
    cmd.env("UV_PYTHON_INSTALL_DIR", dir.join("python"))
        .env("UV_CACHE_DIR", dir.join("cache"))
        .env("UV_PYTHON_PREFERENCE", "only-managed")
        .env("UV_NO_CONFIG", "1")
        .env("UV_NO_PROGRESS", "1");
    for v in ["VIRTUAL_ENV", "PYTHONHOME", "PYTHONPATH", "UV_INDEX_URL", "UV_EXTRA_INDEX_URL", "UV_DEFAULT_INDEX", "UV_INDEX"] {
        cmd.env_remove(v);
    }
    cmd
}

/// The environment's Python, with none of the person's own Python or pip
/// settings, its temporary folder in `dir`.
fn python_command(python: &Path, dir: &Path) -> Command {
    let mut cmd: Command = quiet(python.to_path_buf());
    let tmp = dir.join("tmp");
    cmd.env("TMP", &tmp)
        .env("TEMP", &tmp)
        .env("PYTHONNOUSERSITE", "1")
        .env("PIP_CONFIG_FILE", if cfg!(windows) { "nul" } else { "/dev/null" })
        .env("PIP_NO_INPUT", "1")
        .env("PIP_DISABLE_PIP_VERSION_CHECK", "1")
        .env("PIP_NO_CACHE_DIR", "1")
        .env("PIP_PROGRESS_BAR", "off");
    for v in ["PYTHONHOME", "PYTHONPATH", "PIP_INDEX_URL", "PIP_EXTRA_INDEX_URL", "PIP_REQUIRE_VIRTUALENV", "PIP_USER", "PIP_TARGET", "PIP_PREFIX"] {
        cmd.env_remove(v);
    }
    cmd
}

/// `python -m pip`, as [`python_command`].
fn pip(python: &Path, dir: &Path) -> Command {
    let mut cmd = python_command(python, dir);
    cmd.args(["-m", "pip"]);
    cmd
}

/// Run `cmd`, logging what it says. While it runs, `watch` is told how big
/// a folder is (a download's progress). Err: its last words.
fn run(cmd: &mut Command, watch: Option<(&Path, &dyn Fn(u64))>) -> Result<(), String> {
    let what = format!("{} {}", cmd.get_program().to_string_lossy(), cmd.get_args().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" "));
    tracing::info!("CoTracker setup: {what}");
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| format!("{what}: {e}"))?;
    let said = Arc::new(Mutex::new(Vec::<String>::new()));
    let readers: Vec<_> = [child.stdout.take().map(|o| Box::new(o) as Box<dyn std::io::Read + Send>), child.stderr.take().map(|e| Box::new(e) as Box<dyn std::io::Read + Send>)]
        .into_iter()
        .flatten()
        .map(|pipe| {
            let said = said.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                    tracing::info!("  {line}");
                    let mut s = said.lock().expect("lines");
                    s.push(line);
                    if s.len() > 40 {
                        s.remove(0);
                    }
                }
            })
        })
        .collect();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if let Some((folder, tell)) = watch {
            tell(folder_size(folder));
        }
        std::thread::sleep(Duration::from_millis(1500));
    };
    for r in readers {
        let _ = r.join();
    }
    if status.success() {
        Ok(())
    } else {
        Err(format!("{what}: {status}\n{}", said.lock().expect("lines").join("\n")))
    }
}

/// The bytes in a folder and the folders in it. (Each file's own size: the
/// folder's listing doesn't update a file still being written, on Windows.)
fn folder_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => folder_size(&e.path()),
            _ => std::fs::metadata(e.path()).map_or(0, |m| m.len()),
        })
        .sum()
}

/// Write the worker's code (packed into the program) into `dir`, replacing what was there.
pub fn write_code(dir: &Path) -> std::io::Result<()> {
    let _ = std::fs::remove_dir_all(dir);
    for (path, bytes) in code::FILES {
        let to = dir.join(path);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(to, bytes)?;
    }
    Ok(())
}

/// Start the worker and wait (up to 5 minutes) for it to load the model:
/// the device it loaded on (`cuda`, `cpu`, `mps`).
pub fn test_worker(python: &Path, worker: &Path, model: &Path) -> Result<String, Problem> {
    let failed = |d: String| Problem::new(&format!("CoTracker does not start. {ASK}"), d);
    let mut child = quiet(python.to_path_buf())
        .arg(worker)
        .arg("--weights")
        .arg(model)
        .env_remove("PYTHONHOME")
        .env_remove("PYTHONPATH")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| failed(format!("{}: {e}", python.display())))?;
    let errors = Arc::new(Mutex::new(Vec::<String>::new()));
    if let Some(stderr) = child.stderr.take() {
        let errors = errors.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                tracing::info!("CoTracker worker: {line}");
                let mut e = errors.lock().expect("lines");
                e.push(line);
                if e.len() > 30 {
                    e.remove(0);
                }
            }
        });
    }
    let (tx, rx) = channel();
    if let Some(stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    let deadline = Instant::now() + Duration::from_secs(300);
    let said = |e: &Arc<Mutex<Vec<String>>>| e.lock().expect("lines").join("\n");
    let result = loop {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(line) => {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
                if let Some(ready) = v.get("ready") {
                    break Ok(ready.get("device").and_then(|d| d.as_str()).unwrap_or("unknown").to_string());
                }
                if let Some(e) = v.get("error") {
                    break Err(failed(format!("{e}\n{}", said(&errors))));
                }
            }
            Err(RecvTimeoutError::Disconnected) => break Err(failed(format!("the worker stopped\n{}", said(&errors)))),
            Err(RecvTimeoutError::Timeout) if Instant::now() > deadline => break Err(failed("no answer in 5 minutes".into())),
            Err(RecvTimeoutError::Timeout) => {}
        }
    };
    let _ = child.kill();
    let _ = child.wait();
    result
}

// ------------------------------------------------------------ started early

/// CoTracker's engine started as soon as a video is open (on request: "auto
/// start up cotracker engine so we don't have to warm it up the moment we
/// place a cotracker point (but warn the user via quick popup/notif if that
/// fails, try not to make it crash the app)"): tt_track's shared worker,
/// loaded and warmed up in its own process while the person works, and kept
/// loaded. It is tried once a run (and again after the doctor sets
/// CoTracker up); a failure shows a notice with the reason, and is not tried
/// again by itself. Not after the graphics card stopped and the app started
/// again: the card is left alone for that run.
#[derive(Resource, Debug)]
pub struct EarlyStart {
    /// The setting (Settings > CoTracker): start it when a video opens.
    pub enabled: bool,
    tried: bool,
    /// CoTracker was set up when last looked (set up anew: tried again).
    available: bool,
    /// The notice shown: the reason, and since when.
    notice: Option<(String, Instant)>,
    /// The failure already shown (not shown twice).
    told: Option<String>,
    /// This run started after the graphics card stopped.
    after_gpu_loss: bool,
}

impl Default for EarlyStart {
    fn default() -> Self {
        let after_gpu_loss = matches!(crate::recover::recovered(), Some(crate::recover::Why::Gpu | crate::recover::Why::GpuUnsaved));
        Self { enabled: true, tried: false, available: false, notice: None, told: None, after_gpu_loss }
    }
}

/// How long the notice stays (it has an OK button too).
const NOTICE_FOR: Duration = Duration::from_secs(20);

/// Every frame: start the engine when it should, and show its notice.
/// True: the person asked for the doctor.
pub fn early_start(ctx: &egui::Context, world: &mut World) -> bool {
    let video = world.get_resource::<crate::media::Media>().is_some();
    let available = tt_track::job::cotracker_availability().is_ok();
    let mut e = world.resource_mut::<EarlyStart>();
    if available && !e.available {
        // Set up (anew): one more try.
        e.tried = false;
    }
    e.available = available;
    tt_track::job::keep_cotracker_warm(e.enabled);
    if e.enabled && video && available && !e.tried && !e.after_gpu_loss {
        e.tried = true;
        tracing::info!("starting CoTracker's engine early (a video is open)");
        // (An error is kept by the engine: shown below.)
        let _ = tt_track::job::warm_up_cotracker();
    }
    if let tt_track::job::CoTrackerEngine::Failed(why) = tt_track::job::cotracker_engine()
        && e.tried
        && e.told.as_ref() != Some(&why)
    {
        e.told = Some(why.clone());
        e.notice = Some((why, Instant::now()));
    }
    let Some((why, since)) = e.notice.clone() else { return false };
    if since.elapsed() >= NOTICE_FOR {
        e.notice = None;
        return false;
    }
    ctx.request_repaint_after(Duration::from_millis(500));
    let (mut close, mut doctor) = (false, false);
    egui::Area::new(egui::Id::new("cotracker-notice")).anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-16.0, -48.0)).order(egui::Order::Foreground).show(ctx, |ui| {
        egui::Frame::popup(ui.style()).show(ui, |ui| {
            ui.set_max_width(380.0);
            ui.label(egui::RichText::new("\u{26a0} CoTracker could not start").strong().color(egui::Color32::from_rgb(0xfb, 0xbf, 0x24)));
            ui.label("You can use template trackers. CoTracker trackers start CoTracker again when they track.");
            let short: String = why.chars().take(240).collect();
            ui.label(egui::RichText::new(short).small().weak()).on_hover_text(why.as_str());
            ui.horizontal(|ui| {
                doctor = ui.button("Open the doctor").clicked();
                close = ui.button("OK").clicked();
            });
        });
    });
    if close || doctor {
        world.resource_mut::<EarlyStart>().notice = None;
    }
    doctor
}

/// The top bar's word while the engine starts early: Some while it loads.
pub fn early_start_label(world: &World) -> Option<&'static str> {
    let e = world.get_resource::<EarlyStart>()?;
    (e.enabled && e.tried && tt_track::job::cotracker_engine() == tt_track::job::CoTrackerEngine::Starting).then_some("\u{23f3} CoTracker is starting")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu(name: &str, compute: (u32, u32), driver: &str) -> Gpu {
        Gpu { name: name.into(), compute, driver: driver.into(), apple: false }
    }

    #[test]
    fn the_card_is_read_from_nvidia_smi() {
        assert_eq!(parse_gpu("NVIDIA GeForce RTX 5080, 12.0, 581.57\n"), Some(gpu("NVIDIA GeForce RTX 5080", (12, 0), "581.57")));
        assert_eq!(parse_gpu("\nNVIDIA GeForce RTX 4090, 8.9, 591.86\nNVIDIA GeForce GTX 1060, 6.1, 591.86\n"), Some(gpu("NVIDIA GeForce RTX 4090", (8, 9), "591.86")), "the first card");
        assert_eq!(parse_gpu("No devices were found"), None);
    }

    #[test]
    fn blackwell_gets_cuda_13_and_the_rest_cuda_12() {
        assert_eq!(plan(&gpu("NVIDIA GeForce RTX 5080", (12, 0), "581.57")), Ok(CUDA_13));
        assert_eq!(plan(&gpu("NVIDIA GeForce RTX 4090", (8, 9), "591.86")), Ok(CUDA_12));
        assert_eq!(plan(&gpu("NVIDIA GeForce GTX 1060", (6, 1), "560.94")), Ok(CUDA_12));
        let old_driver = plan(&gpu("NVIDIA GeForce RTX 5080", (12, 0), "572.16")).unwrap_err();
        assert!(old_driver.starts_with("CoTracker needs NVIDIA driver 580 or later. This computer has driver 572.16. Update the NVIDIA driver."), "{old_driver}");
        assert_eq!(plan(&gpu("NVIDIA GeForce GTX 780", (3, 5), "474.30")), Err("The NVIDIA GeForce GTX 780 is too old for CoTracker.".into()));
    }

    #[test]
    fn apple_silicon_gets_pypis_pytorch_on_mps() {
        let mac = Gpu { name: "Apple M3 Pro".into(), compute: (0, 0), driver: "macOS 26.1".into(), apple: true };
        assert_eq!(plan(&mac), Ok(MPS));
        assert_eq!(MPS.index, None);
        assert_eq!(MPS.device, "mps");
    }

    /// This Mac's chip is found (every Apple silicon Mac has one).
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn this_macs_chip_is_found() {
        let g = super::gpu().expect("Apple silicon");
        eprintln!("{g:?}");
        assert!(g.apple && g.name.starts_with("Apple M") && g.driver.starts_with("macOS "), "{g:?}");
    }

    #[test]
    fn the_visual_cpp_runtime_is_read_from_the_registry() {
        let key = "\r\nHKEY_LOCAL_MACHINE\\SOFTWARE\\Microsoft\\VisualStudio\\14.0\\VC\\Runtimes\\x64\r\n    Version    REG_SZ    v14.51.36247.00\r\n    Installed    REG_DWORD    0x1\r\n    Major    REG_DWORD    0xe\r\n\r\n";
        assert_eq!(parse_vc_runtime(key), Some("14.51.36247.00".into()));
        assert_eq!(parse_vc_runtime(&key.replace("0x1", "0x0")), None, "not installed");
        assert_eq!(parse_vc_runtime("ERROR: The system was unable to find the specified registry key or value."), None);
        assert!(new_enough("14.51.36247.00") && new_enough("14.42.34433.00") && new_enough("15.0"));
        assert!(!new_enough("14.40.33810.00") && !new_enough("14.29.30139.00") && !new_enough("14.0.24215.1"), "older than PyTorch's MSVC 14.42");
    }

    #[test]
    fn the_runtime_installers_exit_codes() {
        assert_eq!(runtime_installed(0).ok(), Some(false));
        assert_eq!(runtime_installed(1638).ok(), Some(false), "a newer one is there");
        assert_eq!(runtime_installed(3010).ok(), Some(true), "used after a restart");
        for no in [1602, 1223, -2_147_023_673] {
            assert!(runtime_installed(no).unwrap_err().what.starts_with("Windows asked for permission"), "{no}");
        }
        assert!(runtime_installed(1603).unwrap_err().what.starts_with("trackertools cannot install the Microsoft Visual C++ runtime."));
    }

    /// Windows' own curl is signed by Microsoft; this test program isn't signed.
    #[cfg(windows)]
    #[test]
    fn only_microsofts_signature_passes() {
        signed_by_microsoft(&system32().join("curl.exe")).expect("curl, signed by Microsoft");
        let why = signed_by_microsoft(&std::env::current_exe().expect("this test")).unwrap_err();
        assert!(why.contains("NotSigned"), "{why}");
    }

    /// This computer's runtime (a developer's has one: Visual Studio's).
    #[cfg(windows)]
    #[test]
    fn this_computers_runtime_is_found() {
        if std::env::var_os("TT_VC_RUNTIME").is_some() || !VC_RUNTIME_DLLS.iter().all(|d| system32().join(d).is_file()) {
            return;
        }
        let runtime = vc_runtime();
        eprintln!("{runtime:?}");
        assert_ne!(runtime, Runtime::Missing);
    }

    #[test]
    fn the_code_is_written_out_as_the_worker_expects_it() {
        let dir = std::env::temp_dir().join(format!("tt-cotracker-code-{}", std::process::id()));
        write_code(&dir).expect("written");
        for f in ["editor/cotracker_worker.py", "editor/engine.py", "editor/frames.py", "cotracker/models/build_cotracker.py", "cotracker/models/core/cotracker/cotracker3_online.py", "LICENSE.md"] {
            assert!(dir.join(f).is_file(), "{f}");
        }
        assert!(code::FILES.len() >= 30, "{} files", code::FILES.len());
        assert!(!code::FILES.iter().any(|(p, _)| p.contains("__pycache__")));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The whole setup for real, into `TT_COTRACKER_SETUP_TEST` (a scratch
    /// folder): uv, Python, PyTorch, the packages, the code, the model, and
    /// the worker loading on this computer's NVIDIA card or Apple silicon.
    /// Downloads about 2.5 GB (0.3 GB on a Mac), so only when asked:
    /// `cargo test -p tt_app full_setup -- --ignored`
    /// (`TT_COTRACKER_CUDA=cu130` installs what an RTX 50 card gets).
    #[test]
    #[ignore]
    fn full_setup_for_real() {
        let dir = PathBuf::from(std::env::var("TT_COTRACKER_SETUP_TEST").expect("TT_COTRACKER_SETUP_TEST: a scratch folder"));
        let gpu = super::gpu().expect("an NVIDIA card or Apple silicon");
        let t0 = Instant::now();
        let last = Mutex::new(String::new());
        let step = |s: Step| {
            let text = s.text();
            let mut l = last.lock().unwrap();
            // (The download's progress: every step, and PyTorch at most every 10 seconds.)
            let key = text.split(':').next().unwrap_or_default().to_string();
            if *l != key || t0.elapsed().as_secs().is_multiple_of(10) {
                eprintln!("[{:>4} s] {text}", t0.elapsed().as_secs());
                *l = key;
            }
        };
        let device = install(&dir, &gpu, &step).unwrap_or_else(|p| panic!("{}
{}", p.what, p.details));
        eprintln!("[{:>4} s] done: CoTracker loads on {device}", t0.elapsed().as_secs());
        assert_eq!(device, plan(&gpu).expect("a plan").device);
        assert!(dir.join("env").is_dir() && dir.join("scaled_online.pth").is_file() && !dir.join("cache").exists(), "installed, cache gone");
    }

    /// The worker's code as written out, run by this computer's CoTracker
    /// Python (the repository's .venv) on its model: it loads. Skipped
    /// without them.
    #[test]
    fn the_written_code_loads_the_model() {
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
        let python = repo.join(".venv").join(if cfg!(windows) { "Scripts/python.exe" } else { "bin/python" });
        let model = tt_track::job::cotracker_model().filter(|m| m.is_file());
        let (true, Some(model)) = (python.is_file(), model) else {
            eprintln!("skipped: no CoTracker Python or model here");
            return;
        };
        let dir = std::env::temp_dir().join(format!("tt-cotracker-run-{}", std::process::id()));
        write_code(&dir).expect("written");
        let device = test_worker(&python, &dir.join("editor").join("cotracker_worker.py"), &model).expect("the worker loads the model");
        eprintln!("CoTracker loaded on {device}");
        assert!(["cuda", "cpu", "mps"].contains(&device.as_str()));
        let _ = std::fs::remove_dir_all(dir);
    }
}
