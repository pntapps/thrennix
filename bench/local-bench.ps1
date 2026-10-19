<#
.SYNOPSIS
  Runs bench.ps1 against reproducible local scenarios served by thrennix-testserver.

.EXAMPLE
  .\bench\local-bench.ps1 -Tools thrennix,aria2 -Runs 3
#>
[CmdletBinding()]
param(
    [int]$Runs = 3,
    [string[]]$Tools = @('thrennix', 'aria2', 'idm'),
    [int]$Port = 8090,
    [string]$OutDir = '',
    [switch]$NoBuild,
    [switch]$Quick  # smaller files for a fast smoke run
)

$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
if (-not $OutDir) { $OutDir = Join-Path $here 'results' }
$root = (Resolve-Path (Join-Path $here '..')).Path
$cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
if (-not (Test-Path $cargo)) { $cargo = 'cargo' }

if (-not $NoBuild) {
    Write-Host 'Building release binaries (thrennix-testserver, thrennix-cli)...'
    Push-Location $root
    try {
        & $cargo build --release -p thrennix-testserver -p thrennix-cli
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
    } finally { Pop-Location }
}

$server = Join-Path $root 'target\release\thrennix-testserver.exe'
if (-not (Test-Path $server)) { throw "thrennix-testserver not found at $server" }

$big = '1G'; $mid = '500M'
if ($Quick) { $big = '200M'; $mid = '100M' }
$base = "http://127.0.0.1:$Port"
$scenarios = @(
    @{ Label = "a) $big, 2 MiB/s per connection";              Url = "$base/file/${big}?rate=2048" },
    @{ Label = "b) $big, 40 MiB/s line, 4 MiB/s per conn";     Url = "$base/file/${big}?total_rate=40960&rate=4096" },
    @{ Label = "c) $mid, 150 ms latency, 2% drops";            Url = "$base/file/${mid}?latency=150&fail=0.02" },
    @{ Label = "d) $mid, max 6 conns, 4 MiB/s per conn";       Url = "$base/file/${mid}?max_conns=6&rate=4096" }
)

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$serverLog = Join-Path $OutDir 'testserver.log'
Write-Host "Starting $server on port $Port (log: $serverLog)"
$proc = Start-Process -FilePath $server -ArgumentList @('--port', "$Port") -PassThru -WindowStyle Hidden `
    -RedirectStandardOutput $serverLog -RedirectStandardError "$serverLog.err"
try {
    $up = $false
    for ($i = 0; $i -lt 100; $i++) {
        try {
            $r = Invoke-WebRequest -Uri "$base/stats" -UseBasicParsing -TimeoutSec 2
            if ($r.StatusCode -eq 200) { $up = $true; break }
        } catch { Start-Sleep -Milliseconds 100 }
    }
    if (-not $up) { throw 'thrennix-testserver did not start' }

    & (Join-Path $here 'bench.ps1') -Urls ($scenarios | ForEach-Object { $_.Url }) `
        -Labels ($scenarios | ForEach-Object { $_.Label }) -Runs $Runs -Tools $Tools -OutDir $OutDir

    $stats = Invoke-WebRequest -Uri "$base/stats" -UseBasicParsing
    Write-Host "Server stats: $($stats.Content)"
} finally {
    if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force }
    Write-Host 'thrennix-testserver stopped.'
}
