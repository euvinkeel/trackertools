# trackertools v2 on a Mac (Apple Silicon)

Status: 2026-09-28. v2 was built and measured on Windows (an RTX 4090 desktop). This page says how to run it on a Mac today, what is different there, and a plan for using an M4 Pro's hardware. Companions: [DESIGN.md](DESIGN.md), [ROADMAP.md](ROADMAP.md).

## Running it today

The whole workspace compiles for `aarch64-apple-darwin` (checked with `cargo check --workspace --all-targets --target aarch64-apple-darwin`). A GitHub Actions job (`.github/workflows/macos.yml`) builds it, runs clippy and runs every test on GitHub's Apple Silicon runner (macos-15, arm64), started by hand from the Actions tab (the account's first automatic runs stopped within seconds, before any step: runner minutes). `cargo xtask fixtures` works with an ffmpeg that lacks the drawtext filter (the counter clips then have no visible number; tests read the barcode), and it keeps making the other clips when one fails. The window draws through wgpu, which uses Metal on a Mac.

```sh
xcode-select --install                                    # C compiler and SDK (SQLite and a few crates build C)
curl https://sh.rustup.rs -sSf | sh                       # Rust; the workspace needs 1.95 or newer
brew install ffmpeg                                       # decoding, proxies, test clips (ffmpeg and ffprobe on PATH)
cargo xtask fixtures                                      # test clips
cargo run -p tt_app --release                             # the app
cargo test --workspace --release                          # the tests
```

For the CoTracker3 method (optional):

```sh
python3 -m venv .venv && .venv/bin/pip install torch numpy av opencv-python-headless && .venv/bin/pip install -e .
# the weights (Meta's, CC-BY-NC; not in the repository):
mkdir -p ~/.cache/torch/hub/checkpoints
curl -L -o ~/.cache/torch/hub/checkpoints/scaled_online.pth https://huggingface.co/facebook/cotracker3/resolve/main/scaled_online.pth
.venv/bin/python editor/tests/test_cotracker_worker.py   # the worker alone
cargo test -p tt_track --release --test cotracker -- --nocapture
```

The app finds `.venv/bin/python` by itself (or set `TT_PYTHON`).

## What is different on a Mac now

| | Windows | Mac |
|---|---|---|
| Data (session, projects, proxies) | `%LOCALAPPDATA%\trackertools` | `~/Library/Application Support/trackertools` (not the temp folder, which macOS clears; `TT_DATA_DIR` overrides both) |
| Pointer input while sketching | a raw-input thread: ~1 kHz, hardware timestamps (spike S3) | egui's pointer, one sample per screen refresh, stamped on arrival. Sketching works; the hand's jiggle (which sizes the box) and fast moves are measured more coarsely |
| Scrub proxy encoder | NVENC, else x264 | VideoToolbox (Apple's media engine), else x264 |
| Decoding | ffmpeg in software (`-hwaccel` optional) | the same |
| Shortcuts | Ctrl | Cmd or Ctrl (both work) |
| Finding ffmpeg | `FFMPEG` / `FFPROBE`, else PATH | the same, then Homebrew's `/opt/homebrew/bin` and `/usr/local/bin`: an app opened from Finder gets a PATH without them |
| Settings → Open the folder | Explorer | Finder (`open`) |
| CoTracker3 | CUDA, with v1's CUDA graphs | MPS (the GPU) if a trial window runs there, else the CPU. Not yet measured on a Mac: the ready message and the worker's stderr say which it used |

## Measured on an M4 Pro (2026-09-28)

The first run of `scripts/mac_baseline.sh`: an M4 Pro (12 cores, 24 GB), macOS 27.0, Rust 1.95, Homebrew's ffmpeg 9.0.1 (built without drawtext). Everything builds, clippy is clean, and every test passes. CoTracker3 wasn't set up, so its tests skipped.

| | M4 Pro | for comparison |
|---|---|---|
| Template tracker, cursor fixture (`--test cursor`) | **233 fps**; within 3 px on 94.2% of frames with the default matching, 99.5% with contrast 3× | the cloud VM: 113 fps, 94.1%. Accuracy agrees to 0.1% (the clips were encoded by another ffmpeg build) |
| Backward playback, 1080p60 GOP 250 (`--test player`) | the exact frame on **100%** of UI frames, 4 decoder starts; decodes 890 fps into memory | the cloud VM: 131–161 fps, not real time |
| App playback, 10 s of the 1080p60 sprite clip | 119.6 UI frames/s (the 120 Hz display), 59.9 video fps shown, 2 UI frames of 1195 without the exact frame | |
| Seek / step back / step forward until the exact frame is on screen | median 8.5 / 8.3 / 8.3 ms (one 120 Hz refresh); p95 61 / 11 / 8.7 ms | |
| Scrub proxy (VideoToolbox) for the 20 s 1080p60 clip | built in 2.6 s | |
| ffmpeg decoding the 1080p60 clip to NV12, software | 1200 frames in 0.23 s (**~5300 fps**, all cores) | |
| the same with `-hwaccel videotoolbox` | **313 fps** (the copy back from the media engine dominates) | |
| Sketch demo (scripted hand, injected samples) | point error median 1.46 px, the sprite inside the region on 100% of frames; the tracker from a dragged square: median 0.04 px | |

What this says: playback, stepping, backward play and tracking are already fast on this machine. The one measured gap left is pointer input: sketching gets one sample per screen refresh (egui's pointer), where Windows gets ~1 kHz. The scripted demo injects its samples, so it doesn't measure that.

## Plan: using the M4 Pro

The M4 Pro has a 12–14-core CPU, a 16–20-core GPU, a 16-core Neural Engine, a media engine that decodes and encodes H.264, HEVC and ProRes in hardware, and unified memory (the CPU and GPU share it, so "upload" is a pointer, not a copy). The steps, re-ordered by the measurements above:

1. **Baseline: done** (above). `bash scripts/mac_baseline.sh` does it again and writes one file, `~/Desktop/trackertools_mac.txt`. It installs what's missing (Apple's developer tools, Homebrew, ffmpeg, Rust 1.95), clones or updates the `cloud/all` branch, and records the machine, the build, clippy, every test, the tracker and backward-playback measurements, three scripted app runs (playback pacing, the seek and step benchmark, the sketch demo), ffmpeg's decode speed with and without VideoToolbox, and CoTracker3 if its venv exists.
2. **High-rate pointer input: next.** A macOS pointer service like the Windows one (`tt_app/src/pointer.rs`).
   - Turn off AppKit's mouse coalescing (`NSEvent.mouseCoalescingEnabled = false`) and read each `NSEvent`'s timestamp (seconds since boot, on the same clock as `mach_absolute_time`) in a local event monitor. Map it onto the app clock as the Windows service maps QPC.
   - First a spike like S3: how many reports per second a trackpad and a mouse give, and whether the timestamps are monotonic and ≤ 1 ms apart.
   - A CGEventTap would see the same events off the main thread, but needs the Input Monitoring permission; avoid it unless the monitor falls short.
3. **CoTracker3 on the GPU (MPS), then maybe the Neural Engine.** Not measured yet (needs the venv and weights).
   - Measure MPS against the CPU (the worker reports its device).
   - If ops fall back to the CPU (the worker sets `PYTORCH_ENABLE_MPS_FALLBACK`), find which: `grid_sample` and `einsum` in the correlation are the likely ones.
   - Try fp16 on MPS; v1's `fp16_encoder` switch covers the encoder.
   - Further steps, each a real project:
     - convert the feature encoder (`fnet`, most of the time per frame) to Core ML so it runs on the Neural Engine, keeping the rest on MPS;
     - port the model to MLX.
4. **Memory.** The frame cache defaults to 2 GB. With 24 GB of unified memory, a larger default (a share of physical memory) keeps more of a clip decoded for scrubbing.
5. **Packaging, when it's wanted.** An `.app` bundle (`cargo-bundle`), signed so that macOS permission prompts stick. Only arm64 is needed.

Dropped or deferred by the measurements:

- **Hardware decoding (VideoToolbox): dropped for now.** Through ffmpeg it decodes at 313 fps against ~5300 in software, because each frame is copied back from the media engine. Software decoding already keeps up everywhere measured. What could still pay is decoding in-process and handing the CVPixelBuffer (an IOSurface) to Metal without a copy, for zero-copy display (DESIGN §13). That's only worth it if 4K footage shows a display cost.
- **An all-intra ProRes proxy (`prores_videotoolbox`): deferred.** Steps take one refresh and backward play finds every frame with the H.264 proxy. Revisit for 4K or very long GOPs.
- **The template tracker on more cores or Metal: deferred.** 233 fps on one core per job is ~2.5× the Windows desktop's cursor-fixture rate. Parallel looks and a wider `MAX_JOBS` remain the cheap first steps if many trackers run at once.

Each step lands like the rest of v2: tests or a benchmark with numbers in the PR, and DESIGN/ROADMAP updated with what changed after hands-on use.
