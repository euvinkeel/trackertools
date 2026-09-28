//! Spike S3 (ROADMAP M1): how much pointer data can we get, and how precisely
//! timed? Enabled with `TT_INPUT_PROBE=1`. Compares:
//! - egui: pointer-move events per UI frame (no per-event timestamps);
//! - Windows raw input on a dedicated thread: every HID report, stamped with
//!   QueryPerformanceCounter on arrival, plus the absolute cursor position.
//!
//! Live numbers show in the viewport; a summary is appended every 5 s to
//! `%LOCALAPPDATA%\trackertools\input_probe.log`.
//!
//! Caveat: Windows keeps one raw-input registration per device class per
//! process, so this thread takes mouse raw input over from winit while the
//! probe runs. Production capture would go through winit's device events.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Mutex};

use bevy_ecs::prelude::*;

#[derive(Clone, Copy, Debug)]
pub struct RawSample {
    /// Seconds (QueryPerformanceCounter) when the report arrived.
    pub t: f64,
    pub dx: i32,
    pub dy: i32,
    /// Absolute cursor position (screen pixels) at that moment.
    pub x: i32,
    pub y: i32,
}

#[derive(Resource)]
pub struct InputProbe {
    raw: Arc<Mutex<VecDeque<RawSample>>>,
    /// (UI frame time, pointer-move events that frame)
    egui_frames: VecDeque<(f64, u32)>,
    last_log: f64,
    pub summary: String,
}

impl InputProbe {
    pub fn start() -> Option<Self> {
        std::env::var_os("TT_INPUT_PROBE")?;
        let raw = Arc::new(Mutex::new(VecDeque::new()));
        #[cfg(windows)]
        {
            let sink = raw.clone();
            std::thread::Builder::new().name("raw-input".into()).spawn(move || win::run(sink)).ok()?;
        }
        Some(Self { raw, egui_frames: VecDeque::new(), last_log: 0.0, summary: String::new() })
    }

    /// Record this UI frame's pointer-move events; refresh the summary.
    pub fn frame(&mut self, now: f64, egui_moves: u32) {
        self.egui_frames.push_back((now, egui_moves));
        while self.egui_frames.front().is_some_and(|(t, _)| now - t > 2.0) {
            self.egui_frames.pop_front();
        }
        let raw: Vec<RawSample> = {
            let mut q = self.raw.lock().unwrap();
            let newest = q.back().map_or(0.0, |s| s.t);
            while q.front().is_some_and(|s| newest - s.t > 2.0) {
                q.pop_front();
            }
            q.iter().copied().collect()
        };
        self.summary = summarize(&raw, &self.egui_frames);
        if now - self.last_log > 5.0 {
            self.last_log = now;
            if let Some(path) = log_path() {
                let _ = std::fs::create_dir_all(path.parent().unwrap());
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                    let _ = writeln!(f, "t={now:.1}s  {}", self.summary);
                }
            }
        }
    }
}

fn log_path() -> Option<std::path::PathBuf> {
    Some(std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("trackertools").join("input_probe.log"))
}

fn summarize(raw: &[RawSample], egui: &VecDeque<(f64, u32)>) -> String {
    // Only the last second in which the mouse was actually moving counts.
    let moving: Vec<&RawSample> = raw.iter().filter(|s| s.dx != 0 || s.dy != 0).collect();
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

#[cfg(windows)]
mod win {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    use windows_sys::Win32::UI::Input::{
        GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RID_INPUT, RIDEV_INPUTSINK, RIM_TYPEMOUSE,
        RegisterRawInputDevices,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetCursorPos, GetMessageW, HWND_MESSAGE, MSG, RegisterClassW,
        WM_INPUT, WNDCLASSW,
    };

    use super::RawSample;

    pub fn run(sink: Arc<Mutex<VecDeque<RawSample>>>) {
        unsafe {
            let mut freq = 0i64;
            QueryPerformanceFrequency(&mut freq);
            let class: Vec<u16> = "tt_raw_input\0".encode_utf16().collect();
            let hinstance = GetModuleHandleW(std::ptr::null());
            let wc = WNDCLASSW {
                lpfnWndProc: Some(DefWindowProcW),
                hInstance: hinstance,
                lpszClassName: class.as_ptr(),
                ..std::mem::zeroed()
            };
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                class.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                hinstance,
                std::ptr::null(),
            );
            if hwnd.is_null() {
                tracing::warn!("input probe: could not create a message window");
                return;
            }
            let device = RAWINPUTDEVICE { usUsagePage: 0x01, usUsage: 0x02, dwFlags: RIDEV_INPUTSINK, hwndTarget: hwnd };
            if RegisterRawInputDevices(&device, 1, std::mem::size_of::<RAWINPUTDEVICE>() as u32) == 0 {
                tracing::warn!("input probe: RegisterRawInputDevices failed");
                return;
            }
            tracing::info!("input probe: raw mouse input registered");
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                if msg.message == WM_INPUT {
                    let mut now = 0i64;
                    QueryPerformanceCounter(&mut now);
                    let mut raw: RAWINPUT = std::mem::zeroed();
                    let mut size = std::mem::size_of::<RAWINPUT>() as u32;
                    let got = GetRawInputData(
                        msg.lParam as HRAWINPUT,
                        RID_INPUT,
                        (&mut raw as *mut RAWINPUT).cast(),
                        &mut size,
                        std::mem::size_of::<RAWINPUTHEADER>() as u32,
                    );
                    if got != u32::MAX && raw.header.dwType == RIM_TYPEMOUSE {
                        let m = raw.data.mouse;
                        let mut p = POINT { x: 0, y: 0 };
                        GetCursorPos(&mut p);
                        let sample = RawSample { t: now as f64 / freq as f64, dx: m.lLastX, dy: m.lLastY, x: p.x, y: p.y };
                        let mut q = sink.lock().unwrap();
                        q.push_back(sample);
                        if q.len() > 20_000 {
                            q.pop_front();
                        }
                    }
                }
                DispatchMessageW(&msg);
            }
        }
    }
}
