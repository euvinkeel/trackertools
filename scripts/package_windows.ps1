# Builds trackertools as a portable Windows zip to send to someone:
# trackertools.exe (the C runtime linked in, no console window) with
# ffmpeg.exe and ffprobe.exe beside it, FFmpeg's license and a README. They
# unzip it anywhere and double-click trackertools.exe; nothing to install.
# The same zip as the release workflow's (.github/workflows/release.yml),
# made on this computer.
#
#   scripts/package_windows.ps1 [-Version v0.1.0] [-Ffmpeg <folder with ffmpeg.exe and ffprobe.exe>]
#
# -Version is what the app calls itself (Settings > Updates compares it with
# GitHub's releases). The build goes to target/portable, so a copy running
# from target/release is left alone; the folder and its zip go to dist/.
param(
    [string] $Version = "v0.1.0",
    [string] $Ffmpeg
)
$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot

# ffmpeg and ffprobe: the folder given, else where the ones on PATH really
# are (past a package manager's small launcher, Chocolatey's for one).
if (-not $Ffmpeg) {
    $exe = (Get-Command ffmpeg.exe -ErrorAction Stop).Source
    $Ffmpeg = Split-Path $exe
    if ((Get-Item $exe).Length -lt 10MB -or -not (Test-Path (Join-Path $Ffmpeg "ffprobe.exe"))) {
        $real = Get-ChildItem "$env:ChocolateyInstall\lib\ffmpeg*" -Recurse -Filter ffmpeg.exe -ErrorAction SilentlyContinue | Where-Object Length -gt 10MB | Select-Object -First 1
        if ($real) { $Ffmpeg = $real.DirectoryName }
    }
}
foreach ($tool in "ffmpeg.exe", "ffprobe.exe") {
    if (-not (Test-Path (Join-Path $Ffmpeg $tool))) { throw "No $tool in ${Ffmpeg} (pass -Ffmpeg <folder>)" }
}

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

$name = "trackertools-$Version-windows-x64"
$dist = Join-Path $repo "dist"
$pkg = Join-Path $dist $name
if (Test-Path $pkg) { Remove-Item -Recurse -Force $pkg }
New-Item -ItemType Directory -Force $pkg | Out-Null
Copy-Item (Join-Path $repo "target/portable/release/trackertools.exe") $pkg
Copy-Item (Join-Path $Ffmpeg "ffmpeg.exe"), (Join-Path $Ffmpeg "ffprobe.exe") $pkg
$license = Get-ChildItem (Split-Path $Ffmpeg) -Recurse -Filter "LICENSE*" -ErrorAction SilentlyContinue | Select-Object -First 1
if ($license) { Copy-Item $license.FullName (Join-Path $pkg "FFMPEG-LICENSE.txt") }
Copy-Item (Join-Path $repo "packaging/README.txt") $pkg
$zip = "$pkg.zip"
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path (Join-Path $pkg "*") -DestinationPath $zip
Get-ChildItem $pkg, $zip | Format-Table Name, @{ n = "MB"; e = { [math]::Round($_.Length / 1MB, 1) } }
"Send $zip"
