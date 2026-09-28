//! Spike S3 diagnostics (`TT_INPUT_PROBE=1`): pointer rate and timing from
//! the [`PointerService`] vs egui's per-frame events, shown live in the
//! viewport and appended every 5 s to `input_probe.log` in the data dir
//! (`%LOCALAPPDATA%\trackertools`, or `TT_DATA_DIR`).

use std::collections::VecDeque;
use std::io::Write;

use bevy_ecs::prelude::*;

use crate::pointer::{PointerSample, PointerService};

#[derive(Resource)]
pub struct InputProbe {
    /// (UI frame time, pointer-move events that frame)
    egui_frames: VecDeque<(f64, u32)>,
    last_log: f64,
    pub summary: String,
}

impl InputProbe {
    pub fn start() -> Option<Self> {
        std::env::var_os("TT_INPUT_PROBE")?;
        Some(Self { egui_frames: VecDeque::new(), last_log: 0.0, summary: String::new() })
    }

    /// Record this UI frame's pointer-move events; refresh the summary.
    /// `mapping` = the latest raw sample mapped to window points minus egui's
    /// pointer position (≈ 0 while the mouse rests if the mapping is right).
    pub fn frame(&mut self, now: f64, egui_moves: u32, pointer: &PointerService, mapping: Option<egui::Vec2>) {
        self.egui_frames.push_back((now, egui_moves));
        while self.egui_frames.front().is_some_and(|(t, _)| now - t > 2.0) {
            self.egui_frames.pop_front();
        }
        self.summary = summarize(&pointer.recent(2.0), &self.egui_frames);
        if let Some(d) = mapping {
            self.summary.push_str(&format!(" · raw→window offset ({:+.1}, {:+.1}) pt", d.x, d.y));
        }
        if now - self.last_log > 5.0 {
            self.last_log = now;
            let path = tt_media::proxy::data_dir().join("input_probe.log");
            let _ = std::fs::create_dir_all(path.parent().unwrap());
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(f, "t={now:.1}s  {}", self.summary);
            }
        }
    }
}

fn summarize(raw: &[PointerSample], egui: &VecDeque<(f64, u32)>) -> String {
    let moving: Vec<&PointerSample> = raw.iter().filter(|s| s.dx != 0 || s.dy != 0).collect();
    let raw_part = if moving.len() >= 10 {
        let span = moving.last().unwrap().t - moving.first().unwrap().t;
        let mut dts: Vec<f64> = moving.windows(2).map(|w| (w[1].t - w[0].t) * 1e3).filter(|d| *d < 50.0).collect();
        dts.sort_by(f64::total_cmp);
        let q = |f: f64| dts[((dts.len() - 1) as f64 * f).round() as usize];
        let cursor_moves = moving.windows(2).filter(|w| (w[1].x, w[1].y) != (w[0].x, w[0].y)).count();
        let monotonic = moving.windows(2).all(|w| w[1].t >= w[0].t);
        format!(
            "raw {:.0} Hz (dt p50 {:.2} ms, p99 {:.2} ms, max {:.2} ms, monotonic {monotonic}) · cursor position changes {:.0} Hz",
            (moving.len() - 1) as f64 / span.max(1e-6),
            q(0.5),
            q(0.99),
            dts.last().copied().unwrap_or(0.0),
            cursor_moves as f64 / span.max(1e-6),
        )
    } else {
        "raw: move the mouse (≥10 reports needed)".into()
    };
    let (frames, moves): (u32, u32) = egui.iter().fold((0, 0), |(f, m), (_, n)| (f + 1, m + n));
    let secs = egui.back().map_or(0.0, |b| b.0) - egui.front().map_or(0.0, |f| f.0);
    let max_per_frame = egui.iter().map(|(_, n)| *n).max().unwrap_or(0);
    format!(
        "{raw_part} · egui {:.0} move events/s over {:.0} UI frames/s (max {max_per_frame} per frame)",
        moves as f64 / secs.max(1e-6),
        frames as f64 / secs.max(1e-6)
    )
}
