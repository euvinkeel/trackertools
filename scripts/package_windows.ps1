# Builds trackertools as one portable program to send someone:
# dist/trackertools.exe, with the C runtime linked in and no console window.
# Nothing else goes with it: on its first start it sets up FFmpeg itself
# (crates/tt_app/src/setup.rs), with instructions on screen.
#
#   scripts/package_windows.ps1 [-Version v0.1.0] [-InstallTo C:\apps]
#
# -Version is what the app calls itself (Settings > Updates compares it with
# GitHub's releases). -InstallTo also puts the program in that folder (a
# folder on the PATH: `trackertools` starts it). The build goes to
# target/portable, so a copy running from target/release is left alone; a
# copy running from dist/ or the -InstallTo folder is renamed out of the way
# (Windows lets a running program be renamed, not replaced): the new one
# starts next time.
param(
    [string] $Version = "v0.1.0",
    [string] $InstallTo
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

# Put the program at `$to`; a running copy there goes aside to trackertools.old.exe first.
function Place([string] $from, [string] $to) {
    $old = [System.IO.Path]::ChangeExtension($to, ".old.exe")
    if (Test-Path -LiteralPath $old) { Remove-Item -LiteralPath $old -Force -ErrorAction SilentlyContinue }
    try {
        Copy-Item -LiteralPath $from $to -Force -ErrorAction Stop
    } catch {
        Rename-Item -LiteralPath $to ([System.IO.Path]::GetFileName($old)) -Force
        Copy-Item -LiteralPath $from $to -Force
        "($to was running: the new one starts next time)"
    }
}

$dist = Join-Path $repo "dist"
New-Item -ItemType Directory -Force $dist | Out-Null
$exe = Join-Path $dist "trackertools.exe"
Place (Join-Path $repo "target/portable/release/trackertools.exe") $exe
"{0}: {1:N1} MB" -f $exe, ((Get-Item $exe).Length / 1MB)
if ($InstallTo) {
    $InstallTo = [System.IO.Path]::GetFullPath($InstallTo)
    New-Item -ItemType Directory -Force $InstallTo | Out-Null
    $installed = Join-Path $InstallTo "trackertools.exe"
    Place $exe $installed
    "Installed $installed"
} else {
    "Send $exe"
}
