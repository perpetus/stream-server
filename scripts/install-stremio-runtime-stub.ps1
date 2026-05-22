param(
    [string]$StremioDir = (Join-Path $env:LOCALAPPDATA 'Programs\Stremio')
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Stop-StremioProcess {
    param([string]$Name)

    $processes = Get-Process -Name $Name -ErrorAction SilentlyContinue
    foreach ($process in $processes) {
        Write-Host "Stopping $($process.ProcessName) (pid $($process.Id))"
        Stop-Process -Id $process.Id -Force
    }
}

function Write-FileHash {
    param([string]$Path)

    $hash = Get-FileHash -LiteralPath $Path -Algorithm SHA256
    Write-Host "$($hash.Path)"
    Write-Host "  SHA256 $($hash.Hash)"
}

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = (Resolve-Path (Join-Path $scriptDir '..')).Path
$stremioPath = (Resolve-Path -LiteralPath $StremioDir).Path

$serverJsPath = Join-Path $stremioPath 'server.js'
$runtimePath = Join-Path $stremioPath 'stremio-runtime.exe'
$runtimeBackupPath = Join-Path $stremioPath 'stremio-runtime.node.exe'
$installedServerPath = Join-Path $stremioPath 'stream-server.exe'

if (-not (Test-Path -LiteralPath $serverJsPath)) {
    throw "server.js was not found at $serverJsPath"
}

Stop-StremioProcess -Name 'stremio-shell-ng'
Stop-StremioProcess -Name 'stremio-runtime'
Stop-StremioProcess -Name 'stremio-runtime.node'
Stop-StremioProcess -Name 'stream-server'

if (-not (Test-Path -LiteralPath $runtimeBackupPath)) {
    if (-not (Test-Path -LiteralPath $runtimePath)) {
        throw "Cannot create runtime backup because $runtimePath does not exist"
    }

    Write-Host "Backing up original runtime:"
    Write-Host "  $runtimePath"
    Write-Host "  -> $runtimeBackupPath"
    Copy-Item -LiteralPath $runtimePath -Destination $runtimeBackupPath
} else {
    Write-Host "Runtime backup already exists, leaving it untouched:"
    Write-Host "  $runtimeBackupPath"
}

Push-Location $repoRoot
try {
    cargo build --release -p stremio-runtime-stub
    cargo build --release -p server
} finally {
    Pop-Location
}

$builtStubPath = Join-Path $repoRoot 'target\release\stremio-runtime.exe'
$builtServerPath = Join-Path $repoRoot 'target\release\server.exe'

if (-not (Test-Path -LiteralPath $builtStubPath)) {
    throw "Built stub was not found at $builtStubPath"
}

if (-not (Test-Path -LiteralPath $builtServerPath)) {
    throw "Built server was not found at $builtServerPath"
}

Copy-Item -LiteralPath $builtStubPath -Destination $runtimePath -Force
Copy-Item -LiteralPath $builtServerPath -Destination $installedServerPath -Force

if (-not (Test-Path -LiteralPath $installedServerPath)) {
    throw "stream-server.exe was not installed to $installedServerPath"
}

Write-Host ''
Write-Host 'Installed Stremio runtime stub:'
Write-FileHash -Path $runtimePath
Write-Host ''
Write-Host 'Installed Rust stream server:'
Write-FileHash -Path $installedServerPath
Write-Host ''
Write-Host 'server.js remains untouched:'
Write-FileHash -Path $serverJsPath
Write-Host ''
Write-Host 'Rollback runtime backup:'
Write-FileHash -Path $runtimeBackupPath
