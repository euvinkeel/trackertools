//! Pointer service (spike S3 → production): every mouse report from Windows
//! raw input on a dedicated thread, stamped with QueryPerformanceCounter and
//! the absolute cursor position at that moment (~1 kHz while moving).
//!
//! On macOS: an AppKit local event monitor (the app's own events, so no
//! permission is asked) with mouse-event coalescing off, so every report the
//! mouse or trackpad makes arrives, each with the event's own time (seconds
//! since the system started: the clock `Instant` uses there, so one offset
//! maps them too) and its position in the window. Elsewhere: egui's latest
//! position once a frame.
//!
//! - Times are converted to the app's wall clock (`WallClock`, seconds since
//!   the shell's epoch): Rust's `Instant` is QPC-based on Windows, so one
//!   offset measured at startup maps them exactly.
//! - Windows keeps one raw-input registration per device class per process;
//!   this service owns it (winit's is superseded). egui still receives
//!   ordinary cursor messages for the UI.
//! - The sketch tool reads samples since a given time and converts screen
//!   pixels → window points (DPI) → canvas pixels (the shown space; the core
//!   maps those to the source).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bevy_ecs::prelude::*;
use tt_core::tool::PointerFrame;

use crate::panels::viewport::ViewportMapping;

#[derive(Clone, Copy, Debug)]
pub struct PointerSample {
    /// Wall-clock seconds (same epoch as `WallClock::now`).
    pub t: f64,
    /// Cursor position: physical screen pixels on Windows; window points
    /// from the client area's top-left on macOS ([`window_pos`] maps both).
    pub x: f64,
    pub y: f64,
    /// Raw device motion (counts; unaccelerated; on macOS the event's delta,
    /// in points). 0, 0 for button-only reports.
    pub dx: i32,
    pub dy: i32,
    /// Button transitions in this report (`RI_MOUSE_*` flags; physical buttons).
    pub buttons: u16,
}

/// `PointerSample::buttons` flags.
pub const LEFT_DOWN: u16 = 0x0001;
pub const LEFT_UP: u16 = 0x0002;

#[derive(Resource, Clone)]
pub struct PointerService {
    samples: Arc<Mutex<VecDeque<PointerSample>>>,
    pub running: bool,
}

/// Keep ~20 s of samples at 1 kHz.
const KEEP: usize = 20_000;

impl PointerService {
    /// Start the raw-input thread. `epoch` is the shell's wall-clock origin.
    pub fn start(epoch: Instant) -> Self {
        let samples = Arc::new(Mutex::new(VecDeque::with_capacity(KEEP)));
        #[cfg(windows)]
        let running = {
            let sink = samples.clone();
            std::thread::Builder::new().name("pointer".into()).spawn(move || win::run(sink, epoch)).is_ok()
        };
        #[cfg(target_os = "macos")]
        let running = mac::run(samples.clone(), epoch);
        #[cfg(not(any(windows, target_os = "macos")))]
        let running = {
            let _ = epoch;
            false
        };
        Self { samples, running }
    }

    /// Samples with `t > after`, oldest first.
    pub fn since(&self, after: f64) -> Vec<PointerSample> {
        let q = self.samples.lock().unwrap();
        let start = q.partition_point(|s| s.t <= after);
        q.range(start..).copied().collect()
    }

    /// Time of the latest report in `(after, before]` with any of `flags`.
    pub fn last_button(&self, flags: u16, after: f64, before: f64) -> Option<f64> {
        let q = self.samples.lock().unwrap();
        q.iter().rev().skip_while(|s| s.t > before).take_while(|s| s.t > after).find(|s| s.buttons & flags != 0).map(|s| s.t)
    }

    /// The most recent `seconds` of samples (for probes and live trails).
    pub fn recent(&self, seconds: f64) -> Vec<PointerSample> {
        let q = self.samples.lock().unwrap();
        let Some(last) = q.back().map(|s| s.t) else { return Vec::new() };
        let start = q.partition_point(|s| s.t < last - seconds);
        q.range(start..).copied().collect()
    }
}

/// This app frame's [`PointerFrame`]: the raw samples since the last call,
/// mapped through the viewport as drawn in the previous UI pass into source
/// pixels; button transitions from egui (which widget was hit), timed by the
/// raw reports. Without the service, egui's one position per frame stands in.
pub fn frame(ctx: &egui::Context, service: &PointerService, read: &mut f64, now: f64, map: &ViewportMapping) -> PointerFrame {
    let ppp = ctx.pixels_per_point();
    let (inner, pressed, down, released, origin, latest, hover, wheel) = ctx.input(|i| {
        (
            i.viewport().inner_rect,
            i.pointer.primary_pressed(),
            i.pointer.primary_down(),
            i.pointer.primary_released(),
            i.pointer.press_origin(),
            i.pointer.latest_pos(),
            i.pointer.hover_pos(),
            wheel_notches(i),
        )
    });
    let mut samples = Vec::new();
    match inner.filter(|_| service.running) {
        Some(inner) => {
            for s in service.since(*read) {
                *read = s.t;
                // Window points → source pixels.
                samples.push(map.to_canvas_at(s.t, window_pos(&s, ppp, inner)));
            }
        }
        None => {
            if let Some(p) = latest {
                samples.push(map.to_canvas_at(now, p));
            }
        }
    }
    let on_viewport = |p: egui::Pos2| {
        map.panel.contains(p) && ctx.layer_id_at(p).is_none_or(|l| l.order == egui::Order::Background)
    };
    // The report that carried the transition, if the service saw it (egui events carry no time).
    let when = |flags: u16| service.last_button(flags, now - 0.25, now + 0.02).map_or(now, |t| t.min(now));
    let over = hover.is_some_and(on_viewport);
    PointerFrame {
        samples,
        hover: hover.filter(|_| over).map(|p| map.to_canvas(p)),
        pressed: (pressed && origin.is_some_and(on_viewport)).then(|| when(LEFT_DOWN)),
        down,
        released: released.then(|| when(LEFT_UP)),
        wheel: if over { wheel } else { 0.0 },
        scale: map.points_per_canvas(),
        ..PointerFrame::default()
    }
}

/// A sample's position in window points (egui's). Windows' are physical
/// screen pixels (the client origin is `inner.min`); macOS's are window points already.
pub fn window_pos(s: &PointerSample, ppp: f32, inner: egui::Rect) -> egui::Pos2 {
    if cfg!(target_os = "macos") {
        egui::pos2(s.x as f32, s.y as f32)
    } else {
        egui::pos2(s.x as f32 / ppp - inner.min.x, s.y as f32 / ppp - inner.min.y)
    }
}

/// Mouse-wheel notches this frame (+ = away from you), whatever unit the OS reported.
fn wheel_notches(i: &egui::InputState) -> f32 {
    i.events
        .iter()
        .map(|e| match e {
            egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Line, delta, .. } => delta.y,
            egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta, .. } => delta.y / 50.0,
            egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Page, delta, .. } => delta.y * 10.0,
            _ => 0.0,
        })
        .sum()
}

#[cfg(windows)]
mod win {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

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

    use super::{KEEP, PointerSample};

    pub fn run(sink: Arc<Mutex<VecDeque<PointerSample>>>, epoch: Instant) {
        unsafe {
            let mut freq = 0i64;
            QueryPerformanceFrequency(&mut freq);
            // QPC seconds → wall-clock seconds: measure both "now"s together.
            let mut q = 0i64;
            QueryPerformanceCounter(&mut q);
            let offset = q as f64 / freq as f64 - epoch.elapsed().as_secs_f64();

            let class: Vec<u16> = "tt_pointer\0".encode_utf16().collect();
            let hinstance = GetModuleHandleW(std::ptr::null());
            let wc = WNDCLASSW { lpfnWndProc: Some(DefWindowProcW), hInstance: hinstance, lpszClassName: class.as_ptr(), ..std::mem::zeroed() };
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
                tracing::warn!("pointer service: could not create a message window");
                return;
            }
            let device = RAWINPUTDEVICE { usUsagePage: 0x01, usUsage: 0x02, dwFlags: RIDEV_INPUTSINK, hwndTarget: hwnd };
            if RegisterRawInputDevices(&device, 1, std::mem::size_of::<RAWINPUTDEVICE>() as u32) == 0 {
                tracing::warn!("pointer service: RegisterRawInputDevices failed");
                return;
            }
            tracing::info!("pointer service: raw mouse input registered");
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
                        let sample = PointerSample {
                            t: now as f64 / freq as f64 - offset,
                            x: p.x as f64,
                            y: p.y as f64,
                            dx: m.lLastX,
                            dy: m.lLastY,
                            buttons: m.Anonymous.Anonymous.usButtonFlags,
                        };
                        let mut q = sink.lock().unwrap();
                        q.push_back(sample);
                        if q.len() > KEEP {
                            q.pop_front();
                        }
                    }
                }
                DispatchMessageW(&msg);
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::collections::VecDeque;
    use std::ptr::NonNull;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use block2::RcBlock;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSEvent, NSEventMask, NSEventType};
    use objc2_foundation::NSProcessInfo;

    use super::{KEEP, LEFT_DOWN, LEFT_UP, PointerSample};

    /// Watch the app's mouse events with coalescing off. True if it runs
    /// (it must start on the main thread, as the shell does).
    pub fn run(sink: Arc<Mutex<VecDeque<PointerSample>>>, epoch: Instant) -> bool {
        let Some(mtm) = MainThreadMarker::new() else {
            tracing::warn!("pointer service: not started on the main thread");
            return false;
        };
        // Event times → wall-clock seconds: measure both "now"s together.
        let offset = NSProcessInfo::processInfo().systemUptime() - epoch.elapsed().as_secs_f64();
        NSEvent::setMouseCoalescingEnabled(false);
        let mask = NSEventMask::MouseMoved
            | NSEventMask::LeftMouseDragged
            | NSEventMask::RightMouseDragged
            | NSEventMask::OtherMouseDragged
            | NSEventMask::LeftMouseDown
            | NSEventMask::LeftMouseUp;
        let block = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
            // SAFETY: AppKit hands the monitor a valid event for the call.
            if let Some(sample) = sample(unsafe { event.as_ref() }, mtm, offset) {
                let mut q = sink.lock().unwrap();
                q.push_back(sample);
                if q.len() > KEEP {
                    q.pop_front();
                }
            }
            // Passed on unchanged: winit (and egui) get every event as before.
            event.as_ptr()
        });
        // SAFETY: the handler returns the event it was given; the monitor lives as long as the app.
        match unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(mask, &block) } {
            Some(monitor) => {
                std::mem::forget(monitor);
                tracing::info!("pointer service: AppKit mouse events, coalescing off");
                true
            }
            None => {
                tracing::warn!("pointer service: AppKit did not add the event monitor");
                false
            }
        }
    }

    /// The event as a sample: its time, and where it is in its window's
    /// content (points from the top-left, as egui has them).
    fn sample(e: &NSEvent, mtm: MainThreadMarker, offset: f64) -> Option<PointerSample> {
        let view = e.window(mtm)?.contentView()?;
        let p = view.convertPoint_fromView(e.locationInWindow(), None);
        let y = if view.isFlipped() { p.y } else { view.bounds().size.height - p.y };
        let buttons = match e.r#type() {
            NSEventType::LeftMouseDown => LEFT_DOWN,
            NSEventType::LeftMouseUp => LEFT_UP,
            _ => 0,
        };
        let (dx, dy) = if buttons == 0 { (e.deltaX().round() as i32, e.deltaY().round() as i32) } else { (0, 0) };
        Some(PointerSample { t: e.timestamp() - offset, x: p.x, y, dx, dy, buttons })
    }
}
