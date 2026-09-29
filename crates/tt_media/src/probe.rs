//! Colour properties of the video stream (matrix and range) for display.
//! The MP4 sample table doesn't carry them; ask ffprobe once at open, and fall
//! back to the usual convention (BT.709 for HD, BT.601 for SD; limited range).

use std::path::Path;
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Matrix {
    Bt709,
    Bt601,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorInfo {
    pub matrix: Matrix,
    pub full_range: bool,
}

impl ColorInfo {
    pub fn default_for_height(height: u32) -> Self {
        Self { matrix: if height >= 720 { Matrix::Bt709 } else { Matrix::Bt601 }, full_range: false }
    }
}

pub fn probe_color(path: &Path, height: u32) -> ColorInfo {
    let fallback = ColorInfo::default_for_height(height);
    let ffprobe = crate::ffmpeg::tool("ffprobe", "FFPROBE");
    let mut cmd = Command::new(ffprobe);
    cmd.args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=color_space,color_range", "-of", "default=nw=1"])
        .arg(path);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let Ok(out) = cmd.output() else { return fallback };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut info = fallback;
    for line in text.lines() {
        match line.split_once('=') {
            Some(("color_space", v)) if v.contains("601") || v == "smpte170m" || v == "bt470bg" => info.matrix = Matrix::Bt601,
            Some(("color_space", v)) if v.contains("709") => info.matrix = Matrix::Bt709,
            Some(("color_range", "pc")) => info.full_range = true,
            Some(("color_range", "tv")) => info.full_range = false,
            _ => {}
        }
    }
    info
}
