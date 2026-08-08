<#
.SYNOPSIS
    Build, test, and run GCABB from inside a running GCABB session on Windows.

.DESCRIPTION
    Windows holds an exclusive lock on a running executable, so a self-hosting
    session that rebuilds GCABB into the default `target\debug\gcabb-desktop.exe`
    fails with "Access is denied" (os error 5) as soon as the linker tries to
    replace the binary the developer is currently using.

    This script redirects Cargo to a separate target directory so the running
    GCABB executable is never the link target. Two GCABB builds then coexist:
    the one the developer is running, and the one their session is producing.

    The isolated target directory is deliberately outside `target\` so it is not
    removed by `cargo clean` in the parent checkout, and each worktree gets its
    own directory keyed by name so parallel sessions do not contend either.

.PARAMETER Task
    build (default), test, clippy, fmt, or run.

.PARAMETER TargetDir
    Override the isolated Cargo target directory.

.EXAMPLE
    ./scripts/self-dev.ps1 test

.EXAMPLE
    ./scripts/self-dev.ps1 run
#>
[CmdletBinding()]
param(
    [ValidateSet('build', 'test', 'clippy', 'fmt', 'run')]
    [string]$Task = 'build',

    [string]$TargetDir,

    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$CargoArgs
)

$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repoRoot

if (-not $TargetDir) {
    # Key the directory by worktree name so concurrent self-hosting sessions
    # in different worktrees never share a target directory.
    $worktreeName = Split-Path -Leaf $repoRoot
    $TargetDir = Join-Path $env:LOCALAPPDATA "gcabb\self-dev\$worktreeName"
}

New-Item -ItemType Directory -Force -Path $TargetDir | Out-Null
$env:CARGO_TARGET_DIR = $TargetDir

Write-Host "GCABB self-development build" -ForegroundColor Cyan
Write-Host "  repo:   $repoRoot"
Write-Host "  target: $TargetDir"
Write-Host "  task:   $Task"

# Warn when the developer is running a GCABB binary from this same directory,
# which is the exact contention this script exists to avoid.
$running = Get-Process -Name 'gcabb-desktop' -ErrorAction SilentlyContinue
foreach ($process in $running) {
    $path = $null
    try { $path = $process.Path } catch { $path = $null }
    if ($path -and $path.StartsWith($TargetDir, [StringComparison]::OrdinalIgnoreCase)) {
        Write-Warning @"
A gcabb-desktop process is running from the isolated target directory:
  $path
Linking will fail while it holds the file. Close it, or pass -TargetDir to
choose a different output location.
"@
    }
}

switch ($Task) {
    'build' { cargo build --workspace @CargoArgs }
    'test' { cargo test --workspace @CargoArgs }
    'clippy' { cargo clippy --workspace --all-targets @CargoArgs }
    'fmt' { cargo fmt --all --check @CargoArgs }
    'run' { cargo run -p gcabb-desktop @CargoArgs }
}

if ($LASTEXITCODE -ne 0) {
    Write-Error "cargo $Task failed with exit code $LASTEXITCODE"
    exit $LASTEXITCODE
}

Write-Host "cargo $Task succeeded" -ForegroundColor Green
