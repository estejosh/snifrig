# Builds everything, then packs it into one installer: target\release\snifrig-setup.exe
# Runs at Idle priority with 4 jobs so the PC stays usable.
$ErrorActionPreference = "Stop"
$env:CARGO_BUILD_JOBS = 4
$env:CARGO_TARGET_DIR = Join-Path $PSScriptRoot "target"
[System.Diagnostics.Process]::GetCurrentProcess().PriorityClass = "Idle"
cargo build --release --workspace --manifest-path (Join-Path $PSScriptRoot "Cargo.toml")
cargo build --release --manifest-path (Join-Path $PSScriptRoot "setup\Cargo.toml")
Get-Item (Join-Path $env:CARGO_TARGET_DIR "release\snifrig-setup.exe") | Select-Object Name, Length, LastWriteTime
