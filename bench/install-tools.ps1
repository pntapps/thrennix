# Installs the third-party tools used by the benchmark (aria2).
# Windows PowerShell 5.1 compatible.
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'

function Find-Aria2 {
    $cmd = Get-Command aria2c -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    # winget installs portable packages here and may not have refreshed PATH in this session.
    $links = Join-Path $env:LOCALAPPDATA 'Microsoft\WinGet\Links\aria2c.exe'
    if (Test-Path $links) { return $links }
    $pkgRoot = Join-Path $env:LOCALAPPDATA 'Microsoft\WinGet\Packages'
    if (Test-Path $pkgRoot) {
        $found = Get-ChildItem -Path $pkgRoot -Filter aria2c.exe -Recurse -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($found) { return $found.FullName }
    }
    return $null
}

$aria = Find-Aria2
if ($aria) {
    Write-Host "aria2c already installed: $aria"
} else {
    $winget = Get-Command winget -ErrorAction SilentlyContinue
    if (-not $winget) {
        throw "winget not found. Install aria2 manually from https://github.com/aria2/aria2/releases and put aria2c.exe on PATH."
    }
    Write-Host "Installing aria2 via winget..."
    & winget install --id aria2.aria2 -e --silent --accept-package-agreements --accept-source-agreements
    if ($LASTEXITCODE -ne 0) {
        Write-Warning "winget exited with code $LASTEXITCODE"
    }
    $aria = Find-Aria2
    if (-not $aria) { throw "aria2c still not found after installation." }
    Write-Host "aria2c installed: $aria"
}

$v = & $aria --version | Select-Object -First 1
Write-Host $v

$idm = 'C:\Program Files (x86)\Internet Download Manager\IDMan.exe'
if (Test-Path $idm) {
    Write-Host "IDM found: $idm"
} else {
    Write-Host "IDM not found (optional)."
}
