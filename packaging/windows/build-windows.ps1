$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$ProjectRoot = Resolve-Path (Join-Path $ScriptDir "..\..")
Set-Location $ProjectRoot

Write-Host "==> [1/2] Building optimized standalone release executable..."
cargo build --release

$ExePath = Join-Path $ProjectRoot "target\release\usb-tray-sync.exe"
if (-not (Test-Path $ExePath)) {
    throw "Release binary not found at $ExePath"
}

$DistDir = Join-Path $ProjectRoot "dist\windows"
New-Item -ItemType Directory -Force -Path $DistDir | Out-Null
Copy-Item $ExePath (Join-Path $DistDir "USBBackup.exe") -Force
Write-Host "Standalone executable copied to: $(Join-Path $DistDir 'USBBackup.exe')"

Write-Host "==> [2/2] Checking for installer toolchains (Inno Setup / cargo-wix)..."
$Iscc = Get-Command "ISCC.exe" -ErrorAction SilentlyContinue
if ($Iscc) {
    Write-Host "Building Inno Setup installer (.exe)..."
    & $Iscc.Source (Join-Path $ScriptDir "installer.iss")
} else {
    Write-Host "Inno Setup (ISCC.exe) not found in PATH. Skipping Setup.exe generation."
}

$CargoWix = Get-Command "cargo-wix" -ErrorAction SilentlyContinue
if ($CargoWix) {
    Write-Host "Building MSI package via cargo-wix..."
    cargo wix --nocapture
} else {
    Write-Host "cargo-wix not installed (install via 'cargo install cargo-wix'). Skipping .msi generation."
}
