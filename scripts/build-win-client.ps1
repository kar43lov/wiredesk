# Build the WireDesk client on Windows.
#
# Run from a PowerShell prompt at the repo root:
#     .\scripts\build-win-client.ps1
#
# Building *on* Windows (rather than cross-compiling from the Mac) is what
# gets the app icon into the .exe: build.rs only invokes the resource
# compiler when the build host is Windows, because it needs rc.exe from the
# Windows SDK or windres from mingw-w64.

$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repoRoot

Write-Host '==> Building release binary...'
cargo build --release -p wiredesk-client
if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }

$exe = Join-Path $repoRoot 'target\release\wiredesk-client.exe'
if (-not (Test-Path $exe)) { throw "expected binary not found: $exe" }

Write-Host ''
Write-Host "==> Done. Binary: $exe"
Write-Host '    Run it by double-clicking, or from a shell:'
Write-Host '        .\target\release\wiredesk-client.exe'
Write-Host ''
Write-Host '    First run picks COM3 by default; use Settings -> Port to choose'
Write-Host '    the real adapter, then Save & Restart.'
Write-Host '    Settings and logs live in %APPDATA%\WireDesk\.'
