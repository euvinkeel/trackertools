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
| CoTracker3 | CUDA, with v1's CUDA graphs | MPS (the GPU) if a trial window runs there, else the CPU. **Untested on a real Mac:** the ready message and the worker's stderr say which it used |

## Plan: using the M4 Pro

The M4 Pro has a 12–14-core CPU, a 16–20-core GPU, a 16-core Neural Engine, a media engine that decodes and encodes H.264, HEVC and ProRes in hardware, and unified memory (the CPU and GPU share it, so "upload" is a pointer, not a copy). The steps, in the order I'd do them, each measured before and after with what the repository already has:

1. **Measure the baseline on the Mac first.** `bash scripts/mac_baseline.sh` does all of this and writes one file, `~/Desktop/trackertools_mac.txt`. It installs what's missing (Apple's developer tools, Homebrew, ffmpeg, Rust 1.95), clones or updates the `cloud/all` branch, and records the machine, the build, clippy, every test and the two measurements below. It also runs the app three times, each opening the window and quitting by itself: 10 s of playback with frame pacing, the seek and step benchmark, and the scripted sketch demo. Last come ffmpeg's decode speed with and without VideoToolbox (step 2's first question), and CoTracker3 if its venv exists.
   - `cargo test -p tt_track --release --test cursor -- --nocapture` prints the tracker's fps and accuracy per stretch.
   - `cargo test -p tt_media --release --test player -- --nocapture` prints whether backward playback keeps up.
   - `TT_AUTOPLAY_SECS=10 cargo run -p tt_app --release -- <video>` logs playback frame pacing (the M1 acceptance runs).

   Everything below is a guess until these numbers exist.
2. **Hardware decoding (VideoToolbox).**
   - `DecodeOptions::hwaccel` already passes `-hwaccel` to ffmpeg: try `videotoolbox` for playback and tracker jobs and compare decode fps (spike S1's harness: `cargo run -p tt_media --release --example s1_decode -- <video> <ref.framemd5>`, with its hardware-decoder option).
   - The cost that stays is copying each frame out of ffmpeg's pipe (about 3 MB per 1080p frame).
   - The bigger step, only if the numbers ask for it: decode in-process with VideoToolbox and hand the CVPixelBuffer (an IOSurface) to Metal as a texture. That's zero-copy display, which DESIGN §13 already names as a later optimization, Media Foundation on Windows and VideoToolbox here.
3. **An all-intra ProRes proxy for scrubbing and J.** The media engine encodes ProRes in hardware (`prores_videotoolbox`). Every frame is a keyframe, so stepping and playing backward never decode a group (the backward fill in `tt_media::player` becomes trivial). It costs disk (ProRes Proxy is ~45 Mb/s at 1080p) and is worth a comparison against the H.264 proxy.
4. **High-rate pointer input.** The one sizeable piece: a macOS pointer service like the Windows one (`tt_app/src/pointer.rs`).
   - Turn off AppKit's mouse coalescing (`NSEvent.mouseCoalescingEnabled = false`) and read each `NSEvent`'s timestamp (seconds since boot, on the same clock as `mach_absolute_time`) in a local event monitor. Map it onto the app clock as the Windows service maps QPC.
   - First a spike like S3: how many reports per second a trackpad and a mouse give, and whether the timestamps are monotonic and ≤ 1 ms apart.
   - A CGEventTap would see the same events off the main thread, but needs the Input Monitoring permission; avoid it unless the monitor falls short.
5. **The template tracker on the CPU cores.**
   - Today one job runs one side of a tracker on one thread. The correlation (`ncc::best_match`) is a tight float loop that the compiler already vectorizes with NEON.
   - Cheap wins to measure: searching the looks in parallel, and a wider `MAX_JOBS` on 12+ cores.
   - Only if still slow: the wide search as a Metal compute shader over the patch. Unified memory means the patch needn't be copied.
6. **CoTracker3 on the GPU (MPS), then maybe the Neural Engine.**
   - Measure MPS against the CPU (the worker reports its device).
   - If ops fall back to the CPU (the worker sets `PYTORCH_ENABLE_MPS_FALLBACK`), find which: `grid_sample` and `einsum` in the correlation are the likely ones.
   - Try fp16 on MPS; v1's `fp16_encoder` switch covers the encoder.
   - Further steps, each a real project:
     - convert the feature encoder (`fnet`, most of the time per frame) to Core ML so it runs on the Neural Engine, keeping the rest on MPS;
     - port the model to MLX.
7. **Memory.** The frame cache defaults to 2 GB. With 24–48 GB of unified memory, a larger default (a share of physical memory) keeps more of a clip decoded for scrubbing.
8. **Packaging, when it's wanted.** An `.app` bundle (`cargo-bundle`), signed so that macOS permission prompts stick. Only arm64 is needed.

Each step lands like the rest of v2: tests or a benchmark with numbers in the PR, and DESIGN/ROADMAP updated with what changed after hands-on use.
