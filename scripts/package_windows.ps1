# Builds trackertools as one portable program to send someone:
# dist/trackertools.exe, with the C runtime linked in and no console window.
# Nothing else goes with it: on its first start it sets up FFmpeg itself
# (crates/tt_app/src/setup.rs), with instructions on screen.
#
#   scripts/package_windows.ps1 [-Version v0.1.0]
#
# -Version is what the app calls itself (Settings > Updates compares it with
# GitHub's releases). The build goes to target/portable, so a copy running
# from target/release is left alone.
param(
    [string] $Version = "v0.1.0"
)
$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot

# Built with the version, and the C runtime linked in (so it runs without
# Microsoft's Visual C++ redistributable); this shell's settings come back after.
$saved = @{ TT_VERSION = $env:TT_VERSION; RUSTFLAGS = $env:RUSTFLAGS }
try {
    $env:TT_VERSION = $Version
    $env:RUSTFLAGS = "-C target-feature=+crt-static"
    cargo build -p tt_app --release --target-dir (Join-Path $repo "target/portable")
    if ($LASTEXITCODE -ne 0) { throw "the build failed" }
} finally {
    $env:TT_VERSION = $saved.TT_VERSION
    $env:RUSTFLAGS = $saved.RUSTFLAGS
}

$dist = Join-Path $repo "dist"
New-Item -ItemType Directory -Force $dist | Out-Null
$exe = Join-Path $dist "trackertools.exe"
Copy-Item (Join-Path $repo "target/portable/release/trackertools.exe") $exe -Force
Get-Item $exe | Format-Table Name, @{ n = "MB"; e = { [math]::Round($_.Length / 1MB, 1) } }
"Send $exe"
