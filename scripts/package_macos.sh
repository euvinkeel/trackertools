#!/bin/bash
# Builds trackertools for a Mac with Apple silicon as one program to send
# someone, and the release download the updater looks for:
# dist/trackertools and dist/trackertools-macos-arm64.zip (+ .sha256).
# FFmpeg isn't bundled: the app's first start says to `brew install ffmpeg`
# (crates/tt_app/src/setup.rs).
#
#   scripts/package_macos.sh [--version v0.3.1] [--install-to ~/bin]
#
# --version is what the app calls itself (Settings > Updates compares it with
# GitHub's releases). Without it, the latest version tag in the repository,
# so a build of newer code is never offered an older release as an update.
# --install-to also puts the program in that folder (a folder on the PATH:
# `trackertools` starts it). The build goes to target/portable, so a copy
# running from target/release is left alone. A running copy can be replaced:
# macOS keeps the old file open until it closes.
#
# The program is signed ad hoc (the linker does it on Apple silicon), not
# with a Developer ID: a copy downloaded with a browser is quarantined, and
# macOS asks before it opens it the first time (Control-click > Open). The
# updater's downloads (curl) aren't quarantined.
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
version=""
install_to=""
while [ $# -gt 0 ]; do
  case "$1" in
    --version) version="$2"; shift 2 ;;
    --install-to) install_to="$2"; shift 2 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done
if [ -z "$version" ]; then
  version="$(git -C "$repo" describe --tags --abbrev=0 2>/dev/null || true)"
  [ -n "$version" ] || version="v0.1.0"
fi
echo "Version $version"

[ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] || { echo "This script builds on a Mac with Apple silicon." >&2; exit 1; }
# (Started by the app's Build and restart from Finder or the Dock, PATH doesn't have cargo.)
command -v cargo >/dev/null 2>&1 || { [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"; }

TT_VERSION="$version" cargo build -p tt_app --release --target-dir "$repo/target/portable"

dist="$repo/dist"
mkdir -p "$dist"
# A new file (not written over the old one): a running copy keeps its own.
rm -f "$dist/trackertools"
cp "$repo/target/portable/release/trackertools" "$dist/trackertools"
codesign --verify "$dist/trackertools"
printf '%s: %.1f MB\n' "$dist/trackertools" "$(echo "$(stat -f %z "$dist/trackertools") / 1048576" | bc -l)"

# The zip holds the program alone, at its top (the updater installs from it);
# zip keeps its executable bit.
zip="$dist/trackertools-macos-arm64.zip"
rm -f "$zip" "$zip.sha256"
(cd "$dist" && zip -q -X "$(basename "$zip")" trackertools)
(cd "$dist" && shasum -a 256 "$(basename "$zip")" > "$(basename "$zip").sha256")
cat "$zip.sha256"

if [ -n "$install_to" ]; then
  mkdir -p "$install_to"
  rm -f "$install_to/trackertools"
  cp "$dist/trackertools" "$install_to/trackertools"
  echo "Installed: $install_to/trackertools"
fi
