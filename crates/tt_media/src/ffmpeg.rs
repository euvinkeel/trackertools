//! Decoding through an ffmpeg subprocess (DESIGN §13 step 2).
//!
//! A [`FrameStream`] starts ffmpeg positioned *exactly* at a frame (input `-ss`
//! with accurate seeking: ffmpeg decodes from the preceding keyframe and drops
//! everything before the target, handling open GOPs and B-frame reordering
//! itself) and reads raw NV12 frames from its stdout. With `-fps_mode
//! passthrough` every decoded frame is emitted once, in presentation order, so
//! the n-th frame read is `frames[first + n]` of the [`VideoIndex`].

use std::io::{BufRead, BufReader, ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};

use crate::index::VideoIndex;

#[derive(Clone, Debug)]
pub struct DecodeOptions {
    /// ffmpeg executable (see [`tool`]).
    pub ffmpeg: PathBuf,
    /// Hardware decoder (`cuda`, `d3d11va`, …); frames are copied back to NV12.
    pub hwaccel: Option<String>,
}

impl Default for DecodeOptions {
    fn default() -> Self {
        Self { ffmpeg: tool("ffmpeg", "FFMPEG"), hwaccel: None }
    }
}

/// An ffmpeg executable (`ffmpeg`, `ffprobe`): the environment variable `var`
/// if set, else `name` on PATH. On macOS, when PATH doesn't have it, also
/// Homebrew's folders: an app opened from Finder or the Dock gets a minimal
/// PATH without them.
pub fn tool(name: &str, var: &str) -> PathBuf {
    if let Some(path) = std::env::var_os(var).filter(|v| !v.is_empty()) {
        return PathBuf::from(path);
    }
    let exe = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    // A copy next to the program first: a release ships its own.
    if let Some(beside) = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join(&exe))).filter(|p| p.is_file()) {
        return beside;
    }
    let on_path = std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(&exe).is_file()));
    if !on_path && cfg!(target_os = "macos") {
        let brew = ["/opt/homebrew/bin", "/usr/local/bin"].iter().map(|d| Path::new(d).join(name)).find(|p| p.is_file());
        if let Some(path) = brew {
            return path;
        }
    }
    PathBuf::from(name)
}

/// ffmpeg's options before and after `-i` that start decoding exactly at
/// presented frame `first` (an index into `index.frames`).
pub(crate) fn seek_args(index: &VideoIndex, first: usize) -> (Vec<String>, Vec<String>) {
    if first == 0 {
        return (Vec::new(), Vec::new());
    }
    // Seek a quarter frame before the target: the previous frame is dropped,
    // the target kept, with no floating-point tie at the boundary.
    let quarter = 0.25 / index.fps.as_f64();
    match index.leading_frame_start(first) {
        // Normal case: ffmpeg seeks to the right keyframe and trims to the target.
        None => (vec!["-ss".into(), format!("{:.6}", index.seconds(first) - quarter)], Vec::new()),
        // Open-GOP leading frame: position at an earlier keyframe, then trim
        // on the output side (still inside ffmpeg: skipped frames never
        // cross the pipe). Output timestamps start at 0 at the input seek.
        Some(start) => (
            vec!["-ss".into(), format!("{:.6}", (index.seconds(start) - quarter).max(0.0))],
            vec!["-ss".into(), format!("{:.6}", index.seconds(first) - index.seconds(start))],
        ),
    }
}

pub struct FrameStream {
    child: Child,
    log: Arc<Mutex<Vec<String>>>,
    out: BufReader<ChildStdout>,
    /// Index into `VideoIndex::frames` of the next frame to be read.
    next: usize,
    end: usize,
    frame_bytes: usize,
}

impl FrameStream {
    /// Start decoding at presented frame `first` (an index into `index.frames`).
    pub fn start(index: &VideoIndex, first: usize, opts: &DecodeOptions) -> Result<Self> {
        if first >= index.frames.len() {
            bail!("frame {first} is past the end ({} frames)", index.frames.len());
        }
        let mut cmd = Command::new(&opts.ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
        if let Some(hw) = &opts.hwaccel {
            cmd.args(["-hwaccel", hw]);
        }
        let (before, after) = seek_args(index, first);
        cmd.args(before).arg("-i").arg(&index.path).args(after);
        cmd.args(["-map", "0:v:0", "-an", "-sn", "-dn", "-fps_mode", "passthrough", "-pix_fmt", "nv12", "-f", "rawvideo", "pipe:1"]);
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = cmd.spawn().with_context(|| format!("starting {}", opts.ffmpeg.display()))?;
        let stdout = child.stdout.take().context("ffmpeg stdout")?;
        // Keep ffmpeg's last messages for error reports; anything it prints after
        // we stop reading (a write to the closed pipe) is expected and dropped.
        let stderr = child.stderr.take().context("ffmpeg stderr")?;
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = log.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut l = sink.lock().unwrap();
                l.push(line);
                if l.len() > 20 {
                    l.remove(0);
                }
            }
        });
        Ok(Self {
            child,
            log,
            // Large buffer: a 1080p NV12 frame is ~3 MB.
            out: BufReader::with_capacity(8 << 20, stdout),
            next: first,
            end: index.frames.len(),
            frame_bytes: index.nv12_frame_bytes(),
        })
    }

    pub fn frame_bytes(&self) -> usize {
        self.frame_bytes
    }

    /// Index of the frame the next `read` returns.
    pub fn position(&self) -> usize {
        self.next
    }

    /// Read the next frame into `buf` (resized to one NV12 frame). Returns the
    /// presented index of the frame, or `None` at the end of the video.
    pub fn read(&mut self, buf: &mut Vec<u8>) -> Result<Option<usize>> {
        if self.next >= self.end {
            return Ok(None);
        }
        buf.resize(self.frame_bytes, 0);
        match self.out.read_exact(buf) {
            Ok(()) => {
                let p = self.next;
                self.next += 1;
                Ok(Some(p))
            }
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => {
                let _ = self.child.wait();
                let log = self.log.lock().unwrap().join("\n");
                bail!("ffmpeg ended early at frame {} of {}:\n{log}", self.next, self.end)
            }
            Err(e) => Err(e).context("reading decoded frame"),
        }
    }

    /// Skip frames until the next `read` returns `target` (must be ahead).
    pub fn skip_to(&mut self, target: usize, scratch: &mut Vec<u8>) -> Result<()> {
        while self.next < target {
            if self.read(scratch)?.is_none() {
                break;
            }
        }
        Ok(())
    }
}

impl Drop for FrameStream {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_is_the_variable_if_set_else_its_name_or_a_found_path() {
        // A variable nothing else reads, so tests running in parallel don't see it.
        unsafe { std::env::set_var("TT_TEST_TOOL", "/some/ffmpeg") };
        assert_eq!(tool("ffmpeg", "TT_TEST_TOOL"), PathBuf::from("/some/ffmpeg"));
        let found = tool("tt-no-such-tool", "TT_TEST_TOOL_UNSET");
        assert_eq!(found, PathBuf::from("tt-no-such-tool"));
    }
}
