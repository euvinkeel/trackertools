#!/bin/bash
# trackertools v2 on a Mac: one script that sets up what's missing, builds,
# tests, opens the app for a few scripted runs, measures, and writes
# everything to ONE file on the Desktop (trackertools_mac.txt) to attach.
#
# Run it from Terminal (from anywhere; it clones the repository if needed):
#   bash scripts/mac_baseline.sh
# It keeps going when a step fails: the failure is what the file is for.
# Takes ~15-30 minutes. The app's window opens and closes by itself 3 times.

set -u
OUT="$HOME/Desktop/trackertools_mac.txt"
BRANCH=cloud/all
: > "$OUT"
say() { printf '\n>>> %s\n' "$*"; printf '\n== %s\n' "$*" >> "$OUT"; }
run() { say "$*"; "$@" >> "$OUT" 2>&1; echo "(exit $?)" >> "$OUT"; }

# --- 1. The tools -----------------------------------------------------------
if ! xcode-select -p >/dev/null 2>&1; then
  xcode-select --install
  echo
  echo "A window asks to install Apple's developer tools: click Install, wait until it's done,"
  echo "then run this script again."
  exit 1
fi
if ! command -v brew >/dev/null 2>&1; then
  for b in /opt/homebrew/bin/brew /usr/local/bin/brew; do [ -x "$b" ] && eval "$("$b" shellenv)"; done
fi
if ! command -v brew >/dev/null 2>&1; then
  say "installing Homebrew (it asks for your Mac password)"
  /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
  eval "$(/opt/homebrew/bin/brew shellenv)"
fi
command -v ffmpeg >/dev/null 2>&1 || { say "installing ffmpeg"; brew install ffmpeg; }
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
if ! command -v rustup >/dev/null 2>&1; then
  say "installing Rust"
  curl https://sh.rustup.rs -sSf | sh -s -- -y
  . "$HOME/.cargo/env"
fi
rustup toolchain install 1.95 --profile minimal --component clippy >/dev/null 2>&1
rustup default 1.95 >/dev/null 2>&1

# --- 2. The code ------------------------------------------------------------
here="$(cd "$(dirname "$0")/.." 2>/dev/null && pwd)"
if [ -f "$here/Cargo.toml" ] && [ -d "$here/crates/tt_app" ]; then
  cd "$here"
else
  [ -d "$HOME/trackertools/.git" ] || git clone https://github.com/euvinkeel/trackertools "$HOME/trackertools"
  cd "$HOME/trackertools" || exit 1
fi
git fetch -q origin "$BRANCH" && git checkout -q "$BRANCH" && git pull -q --ff-only origin "$BRANCH"

say "machine"
{ sw_vers; sysctl -n machdep.cpu.brand_string hw.ncpu hw.memsize; rustc --version; ffmpeg -version | head -1; git log --oneline -1; } >> "$OUT" 2>&1

# --- 3. Build, check, test --------------------------------------------------
run cargo xtask fixtures
run cargo build --workspace --release --all-targets
run cargo clippy --workspace --all-targets -- -D warnings
run cargo test --workspace --release
run cargo test -p tt_track --release --test cursor -- --nocapture
run cargo test -p tt_media --release --test player -- --nocapture

# --- 4. The app itself: each run opens the window, does its thing, quits ----
app() {  # app <what> <env...>: the app on the sprite clip, stopped after 3 minutes if stuck
  say "app: $1"
  shift
  env "$@" ./target/release/trackertools fixtures/sprite_1080p60.mp4 >> "$OUT" 2>&1 &
  pid=$!
  ( sleep 180; kill "$pid" 2>/dev/null && echo "(stopped: still open after 3 minutes)" >> "$OUT" ) &
  guard=$!
  wait "$pid"; echo "(exit $?)" >> "$OUT"
  kill "$guard" 2>/dev/null
}
echo ">>> The app's window will open and close by itself 3 times. Don't touch it."
app "plays 10 s and logs frame pacing" TT_AUTOPLAY_SECS=10
app "times seeks and frame steps" TT_BENCH_STEPS=1
app "sketches the sprite with a scripted hand" TT_SKETCH_DEMO=fixtures/sprite_truth.json

# --- 5. Hardware decoding (VideoToolbox) against software --------------------
for hw in none videotoolbox; do
  if [ "$hw" = none ]; then set -- ; else set -- -hwaccel "$hw"; fi
  say "ffmpeg decodes the 1080p60 clip to NV12, hwaccel $hw"
  ffmpeg -hide_banner -benchmark "$@" -i fixtures/sprite_1080p60.mp4 -pix_fmt nv12 -f null - 2>&1 | tr '\r' '\n' | grep -E '^frame=|bench: utime|rror' | tail -3 >> "$OUT"
done

# --- 6. CoTracker3, only if its Python setup exists (docs/v2/APPLE_SILICON.md) --
if [ -x .venv/bin/python ]; then
  run .venv/bin/python editor/tests/test_cotracker_worker.py
  run cargo test -p tt_track --release --test cotracker -- --nocapture
else
  say "CoTracker3: skipped (no .venv)"
fi

echo
echo "Done. Attach this file:  $OUT"
open -R "$OUT" 2>/dev/null
