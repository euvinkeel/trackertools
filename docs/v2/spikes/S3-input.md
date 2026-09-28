# Spike S3: high-rate, timestamped pointer input

2026-09-27 · probe: `crates/tt_app/src/input_probe.rs` (run with `TT_INPUT_PROBE=1`) · the user moved the mouse for ~20 s

**Question.** Can we capture every pointer movement with a usable timestamp, which motion sketching (DESIGN §8) depends on?

## Results

Summaries every 5 s over the preceding 2 s window. Rates count reports while moving; a mouse reports only on motion.

| Window | Raw input (Windows) | Report spacing p50 / p99 | Cursor-position updates | egui |
|---|---|---|---|---|
| 5 s | 304 Hz | 1.00 / 9.96 ms | 288 Hz | 44 events/s at 175 UI fps |
| 10 s (slow motion) | 115 Hz | 8.00 / 10.30 ms | 110 Hz | 160 events/s |
| 15 s (fast, continuous) | **837 Hz** | **1.00 / 4.89 ms** | **788 Hz** | 103 events/s |
| 20 s | 218 Hz | 2.94 / 21.0 ms | 112 Hz | 84 events/s |

- **The raw input path passes.**
  - The mouse is a 1 kHz device: p50 spacing is 1.00 ms during motion.
  - QueryPerformanceCounter stamps are monotonic.
  - `GetCursorPos` at each report gives the absolute on-screen cursor, post-acceleration, which is exactly what the user sees and follows. It changes on ~95% of reports.
- **egui fails for capture.** It gives about one pointer event per UI frame, with no per-event timestamps: up to ~12× fewer samples than the hardware delivers during fast motion. It remains fine for UI interaction.

## Decision

- **Motion sketch capture uses a dedicated input thread (Windows raw input)**, recording `(QPC time, absolute cursor, raw delta)` per HID report. It is published to the world as a timestamped stream.
- **Clock alignment:** Rust's `Instant` is QPC-based on Windows, so QPC seconds map exactly onto the app's `WallClock` via one offset measured at startup.
- **Mapping to the image:** screen positions map to viewport and source coordinates through the window's client origin and DPI scale, taken from egui/winit each frame.
- **One registration per process.** Windows keeps one raw-input registration per device class per process, so our thread supersedes winit's mouse registration. The app therefore owns *all* raw mouse data through this service, including relative deltas for future pointer-lock drags such as finetune nudging. egui keeps receiving ordinary cursor messages for the UI.
- **Non-Windows platforms** would need their own backend. Out of scope: the app targets Windows.
