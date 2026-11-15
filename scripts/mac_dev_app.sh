#!/bin/bash
# Makes "trackertools dev.app" in ~/Applications: a Dock-friendly way to run
# this checkout's newest code. Each time it opens, it
#   1. pulls main (only when the checkout is on main with nothing uncommitted),
#   2. builds it (cargo build --release: seconds when little changed),
#   3. starts it, from inside the app (so the Dock shows one icon).
# A build that takes more than a moment shows a notification; a failed one
# shows the end of the log. Log: ~/Library/Logs/trackertools-dev.log.
# Settings > Updates > Build and restart works from it too (it installs the
# new build into the app).
#
#   scripts/mac_dev_app.sh [--to ~/Applications]
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
to="$HOME/Applications"
[ "${1:-}" = --to ] && to="$2"
app="$to/trackertools dev.app"
[ "$(uname -s)" = Darwin ] || { echo "This script is for a Mac." >&2; exit 1; }

mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>trackertools dev</string>
  <key>CFBundleDisplayName</key><string>trackertools dev</string>
  <key>CFBundleIdentifier</key><string>com.euvinkeel.trackertools.dev</string>
  <key>CFBundleExecutable</key><string>launch</string>
  <key>CFBundleIconFile</key><string>icon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>dev</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

cat > "$app/Contents/MacOS/launch" <<LAUNCH
#!/bin/bash
# trackertools dev: pull main, build, start (made by scripts/mac_dev_app.sh).
REPO="$repo"
LAUNCH
cat >> "$app/Contents/MacOS/launch" <<'LAUNCH'
HERE="$(cd "$(dirname "$0")" && pwd)"
LOG="$HOME/Library/Logs/trackertools-dev.log"
# (Opened from the Dock, PATH has neither cargo nor Homebrew's ffmpeg.)
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
export PATH="/opt/homebrew/bin:/usr/local/bin:$PATH"
note() { osascript -e "display notification \"$1\" with title \"trackertools dev\"" >/dev/null 2>&1 || true; }
alert() { osascript -e "display alert \"trackertools dev\" message \"$1\"" >/dev/null 2>&1 || true; }

cd "$REPO" 2>/dev/null || { alert "The source folder is not there: $REPO. Run scripts/mac_dev_app.sh again from the checkout."; exit 1; }
# Keep the log short: the last ~2000 lines.
[ -f "$LOG" ] && tail -n 2000 "$LOG" > "$LOG.tmp" && mv "$LOG.tmp" "$LOG"
branch="$(git rev-parse --abbrev-ref HEAD 2>/dev/null)"
{
  echo "=== $(date '+%Y-%m-%d %H:%M:%S')"
  if [ "$branch" = main ] && [ -z "$(git status --porcelain --untracked-files=no)" ]; then
    git pull --ff-only --quiet || echo "(git pull failed: building what is here)"
  else
    echo "(on $branch, or uncommitted changes: building what is here, no pull)"
  fi
  echo "building $(git log -1 --format='%h %s') on $branch"
} >> "$LOG" 2>&1

cargo build -p tt_app --release >> "$LOG" 2>&1 &
build=$!
sleep 2
kill -0 "$build" 2>/dev/null && note "Building $branch… (the first build takes a few minutes)"
if ! wait "$build"; then
  end="$(grep -E '^error' -A6 "$LOG" | tail -n 12 | tr '"\\' "'/")"
  osascript -e "display alert \"trackertools dev: the build failed\" message \"$end\" buttons {\"Open log\", \"OK\"} default button \"OK\"" \
    -e "if button returned of result is \"Open log\" then do shell script \"open -a Console '$LOG'\"" >/dev/null 2>&1 || true
  exit 1
fi
# A new file, not written over the old one (a running copy keeps its own).
cp -f target/release/trackertools "$HERE/trackertools.new" && mv -f "$HERE/trackertools.new" "$HERE/trackertools"
exec "$HERE/trackertools" >> "$LOG" 2>&1
LAUNCH
chmod +x "$app/Contents/MacOS/launch"

# The icon: a crosshair on a dark tile with "dev" under it, drawn by AppKit.
work="$(mktemp -d)"
cat > "$work/icon.swift" <<'SWIFT'
import AppKit
let out = CommandLine.arguments[1]
for (px, name) in [(16, "16x16"), (32, "16x16@2x"), (32, "32x32"), (64, "32x32@2x"), (128, "128x128"), (256, "128x128@2x"), (256, "256x256"), (512, "256x256@2x"), (512, "512x512"), (1024, "512x512@2x")] {
    let s = CGFloat(px)
    let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: px, pixelsHigh: px, bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    let tile = NSRect(x: s * 0.1, y: s * 0.1, width: s * 0.8, height: s * 0.8)
    NSColor(calibratedRed: 0.09, green: 0.11, blue: 0.14, alpha: 1).setFill()
    NSBezierPath(roundedRect: tile, xRadius: s * 0.18, yRadius: s * 0.18).fill()
    let accent = NSColor(calibratedRed: 0.20, green: 0.83, blue: 0.60, alpha: 1)
    accent.setStroke()
    let c = NSPoint(x: s * 0.5, y: s * 0.56), r = s * 0.2
    let ring = NSBezierPath(ovalIn: NSRect(x: c.x - r, y: c.y - r, width: 2 * r, height: 2 * r))
    ring.lineWidth = max(1, s * 0.035); ring.stroke()
    let cross = NSBezierPath()
    for (a, b) in [((c.x - r * 1.45, c.y), (c.x - r * 0.45, c.y)), ((c.x + r * 0.45, c.y), (c.x + r * 1.45, c.y)), ((c.x, c.y - r * 1.45), (c.x, c.y - r * 0.45)), ((c.x, c.y + r * 0.45), (c.x, c.y + r * 1.45))] {
        cross.move(to: NSPoint(x: a.0, y: a.1)); cross.line(to: NSPoint(x: b.0, y: b.1))
    }
    cross.lineWidth = max(1, s * 0.035); cross.stroke()
    accent.setFill()
    NSBezierPath(ovalIn: NSRect(x: c.x - s * 0.03, y: c.y - s * 0.03, width: s * 0.06, height: s * 0.06)).fill()
    if px >= 64 {
        let text = NSAttributedString(string: "dev", attributes: [.font: NSFont.boldSystemFont(ofSize: s * 0.13), .foregroundColor: NSColor.white])
        text.draw(at: NSPoint(x: s * 0.5 - text.size().width / 2, y: s * 0.15))
    }
    NSGraphicsContext.restoreGraphicsState()
    try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: "\(out)/icon_\(name).png"))
}
SWIFT
mkdir -p "$work/icon.iconset"
if swift "$work/icon.swift" "$work/icon.iconset" 2>/dev/null && iconutil -c icns "$work/icon.iconset" -o "$app/Contents/Resources/icon.icns"; then
  :
else
  echo "(no icon: swift or iconutil is missing; the app works without it)"
fi
rm -rf "$work"
# Finder and the Dock read the icon again.
touch "$app"

echo "Made: $app"
echo "Open it once (Finder: Applications > trackertools dev), then right-click its Dock icon > Options > Keep in Dock."
