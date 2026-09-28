//! Renditions (DESIGN §13 step 6): lighter copies of the video on the same
//! frame grid. The scrub proxy is ≤720p with a keyframe every 12 frames and no
//! B-frames, so any frame is at most 11 cheap decodes from a keyframe —
//! backward stepping and seeking stop depending on the source's long GOPs.
//!
//! Encoded with NVENC when available (libx264 otherwise) and `-fps_mode
//! passthrough`, so frame n of the proxy is frame n of the original. That 1:1
//! correspondence is verified after the build; a proxy that fails it is
//! discarded rather than trusted.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result, bail};

use crate::ffmpeg::DecodeOptions;
use crate::index::VideoIndex;

pub const PROXY_HEIGHT: u32 = 720;
pub const PROXY_GOP: u32 = 12;

/// Where a source's scrub proxy lives: a per-user cache keyed by the source's
/// path, size and modification time (a changed source gets a new proxy).
pub fn proxy_path(source: &Path) -> Result<PathBuf> {
    let meta = std::fs::metadata(source)?;
    let mtime = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
    let key = format!("{}|{}|{}", source.canonicalize()?.display(), meta.len(), mtime);
    let hash = key.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3)); // FNV-1a
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    Ok(base.join("trackertools").join("proxies").join(format!("{hash:016x}.mp4")))
}

/// Open an existing proxy if it matches the source frame for frame.
pub fn open_matching(source: &VideoIndex, path: &Path) -> Option<VideoIndex> {
    let proxy = VideoIndex::open(path).ok()?;
    matches(source, &proxy).then_some(proxy)
}

/// Frame n of the proxy is frame n of the source: same frames on the same grid
/// slots. (The grid *length* may differ by how each container records the
/// last frame's duration; the app always uses the source's grid.)
fn matches(source: &VideoIndex, proxy: &VideoIndex) -> bool {
    proxy.frames.len() == source.frames.len() && proxy.grid_of == source.grid_of
}

/// Build the scrub proxy for `source` at `out`. `progress` receives frames
/// encoded so far. Blocking: run on a worker thread.
pub fn build(source: &VideoIndex, out: &Path, opts: &DecodeOptions, progress: Arc<AtomicU32>) -> Result<VideoIndex> {
    std::fs::create_dir_all(out.parent().context("proxy path has no parent")?)?;
    let part = out.with_extension("part.mp4");
    let scale = if source.height > PROXY_HEIGHT { format!("scale=-2:{PROXY_HEIGHT}:flags=bicubic,") } else { String::new() };
    let filter = format!("{scale}format=nv12");

    let mut last_error = String::new();
    let encoders: [&[&str]; 2] = [
        &["-c:v", "h264_nvenc", "-preset", "p4", "-rc", "vbr", "-cq", "23", "-b:v", "0"],
        &["-c:v", "libx264", "-preset", "veryfast", "-crf", "20"],
    ];
    for encoder in encoders {
        progress.store(0, Ordering::Relaxed);
        let mut cmd = Command::new(&opts.ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i"])
            .arg(&source.path)
            .args(["-map", "0:v:0", "-an", "-sn", "-dn", "-fps_mode", "passthrough", "-vf", &filter])
            .args(encoder)
            .args(["-g", &PROXY_GOP.to_string(), "-bf", "0", "-movflags", "+faststart", "-progress", "pipe:1"])
            .arg(&part)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = cmd.spawn().context("starting ffmpeg for the proxy")?;
        let stderr = child.stderr.take().context("ffmpeg stderr")?;
        let err_thread = std::thread::spawn(move || BufReader::new(stderr).lines().map_while(Result::ok).collect::<Vec<_>>());
        for line in BufReader::new(child.stdout.take().context("ffmpeg stdout")?).lines().map_while(Result::ok) {
            if let Some(n) = line.strip_prefix("frame=").and_then(|v| v.trim().parse::<u32>().ok()) {
                progress.store(n, Ordering::Relaxed);
            }
        }
        let status = child.wait()?;
        let errors = err_thread.join().unwrap_or_default();
        if status.success() {
            let proxy = VideoIndex::open(&part)?;
            if !matches(source, &proxy) {
                let _ = std::fs::remove_file(&part);
                bail!(
                    "proxy does not match the source frame for frame ({} vs {} frames)",
                    proxy.frames.len(),
                    source.frames.len()
                );
            }
            std::fs::rename(&part, out)?;
            return VideoIndex::open(out);
        }
        last_error = errors.join("\n");
        let _ = std::fs::remove_file(&part);
    }
    bail!("proxy build failed: {last_error}")
}
