<#
.SYNOPSIS
    Builds the Thrennix Windows installer (dist\Thrennix-Setup-<version>.exe) with Inno Setup 6.

.DESCRIPTION
    1. Reads the version from [workspace.package] in Cargo.toml (or -Version).
    2. Unless -SkipBuild is given, runs: cargo build --release -p thrennix-app -p thrennix-cli
    3. Compiles packaging\installer\thrennix.iss with ISCC.exe.
    4. Prints the installer's SHA-256 and, when dist\SHA256SUMS.txt exists, adds or
       updates the installer's line in it.

    Compatible with Windows PowerShell 5.1 and PowerShell 7+.

.PARAMETER Version
    Override the version. Default: [workspace.package] version from Cargo.toml.

.PARAMETER BinDir
    Folder holding thrennix.exe and thrennix-cli.exe. Default: target\release, or
    target\x86_64-pc-windows-msvc\release when the former has no thrennix.exe.

.PARAMETER OutDir
    Output folder. Default: dist (under the repository root).

.PARAMETER SkipBuild
    Do not run cargo; use the binaries already in BinDir.

.PARAMETER Iscc
    Full path to ISCC.exe. Default: searched on PATH and in the usual install folders.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File packaging\build-installer.ps1

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File packaging\build-installer.ps1 -SkipBuild -BinDir target\x86_64-pc-windows-msvc\release
#>
[CmdletBinding()]
param(
    [string]$Version,
    [string]$BinDir,
    [string]$OutDir,
    [switch]$SkipBuild,
    [string]$Iscc
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'

$PackagingDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$Root = Split-Path -Parent $PackagingDir
$IssFile = Join-Path $PackagingDir 'installer\thrennix.iss'

function Write-Step([string]$Message) {
    Write-Host "==> $Message" -ForegroundColor Cyan
}

function Get-WorkspaceVersion([string]$CargoToml) {
    $section = ''
    foreach ($line in Get-Content -LiteralPath $CargoToml) {
        $t = $line.Trim()
        if ($t -match '^\[(.+)\]$') {
            $section = $Matches[1].Trim()
            continue
        }
        if ($section -eq 'workspace.package' -and $t -match '^version\s*=\s*"([^"]+)"') {
            return $Matches[1]
        }
    }
    throw "No version found under [workspace.package] in $CargoToml"
}

function Resolve-FullPath([string]$Path) {
    if ([System.IO.Path]::IsPathRooted($Path)) {
        return [System.IO.Path]::GetFullPath($Path)
    }
    return [System.IO.Path]::GetFullPath((Join-Path $Root $Path))
}

function Find-Iscc {
    $cmd = Get-Command 'ISCC.exe' -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    $candidates = @()
    if ($env:LOCALAPPDATA) { $candidates += (Join-Path $env:LOCALAPPDATA 'Programs\Inno Setup 6\ISCC.exe') }
    if (${env:ProgramFiles(x86)}) { $candidates += (Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe') }
    if ($env:ProgramFiles) { $candidates += (Join-Path $env:ProgramFiles 'Inno Setup 6\ISCC.exe') }
    foreach ($c in $candidates) {
        if (Test-Path -LiteralPath $c) { return $c }
    }
    # Uninstall registry entry written by the Inno Setup installer (per-user or per-machine).
    $keys = @(
        'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Inno Setup 6_is1',
        'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Inno Setup 6_is1',
        'HKLM:\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\Inno Setup 6_is1'
    )
    foreach ($k in $keys) {
        $item = Get-ItemProperty -LiteralPath $k -ErrorAction SilentlyContinue
        if ($item -and ($item.PSObject.Properties.Name -contains 'InstallLocation')) {
            $p = Join-Path $item.InstallLocation 'ISCC.exe'
            if (Test-Path -LiteralPath $p) { return $p }
        }
    }
    throw 'ISCC.exe (Inno Setup 6) not found. Install it with: winget install --id JRSoftware.InnoSetup -e'
}

function Find-Cargo {
    $cmd = Get-Command 'cargo.exe' -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    $p = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
    if (Test-Path -LiteralPath $p) { return $p }
    throw 'cargo.exe not found'
}

function Get-Sha256([string]$Path) {
    $sha = [System.Security.Cryptography.SHA256]::Create()
    $stream = [System.IO.File]::OpenRead($Path)
    try {
        $bytes = $sha.ComputeHash($stream)
    }
    finally {
        $stream.Dispose()
        $sha.Dispose()
    }
    return (($bytes | ForEach-Object { $_.ToString('x2') }) -join '')
}

# ---------------------------------------------------------------- inputs

if (-not $Version) {
    $Version = Get-WorkspaceVersion (Join-Path $Root 'Cargo.toml')
}
if ($Version -notmatch '^[0-9A-Za-z.+-]+$') {
    throw "Suspicious version string: '$Version'"
}
$Numeric = ($Version -replace '[-+].*$', '')
if ($Numeric -notmatch '^\d+(\.\d+){0,3}$') {
    throw "Version '$Version' does not start with a numeric x.y.z"
}

if ($OutDir) { $OutDir = Resolve-FullPath $OutDir } else { $OutDir = Join-Path $Root 'dist' }

if (-not $SkipBuild) {
    Write-Step 'cargo build --release -p thrennix-app -p thrennix-cli'
    $cargo = Find-Cargo
    Push-Location $Root
    try {
        & $cargo build --release -p thrennix-app -p thrennix-cli
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit $LASTEXITCODE)" }
    }
    finally {
        Pop-Location
    }
}

if ($BinDir) {
    $BinDir = Resolve-FullPath $BinDir
}
else {
    $BinDir = Join-Path $Root 'target\release'
    $alt = Join-Path $Root 'target\x86_64-pc-windows-msvc\release'
    if (-not (Test-Path -LiteralPath (Join-Path $BinDir 'thrennix.exe')) -and (Test-Path -LiteralPath (Join-Path $alt 'thrennix.exe'))) {
        $BinDir = $alt
    }
}
foreach ($exe in @('thrennix.exe', 'thrennix-cli.exe')) {
    if (-not (Test-Path -LiteralPath (Join-Path $BinDir $exe))) {
        throw "$exe not found in $BinDir. Build first, or drop -SkipBuild."
    }
}

# Defence in depth: the .iss excludes extension\.keys and key-like files, but refuse to
# run at all if a key-like file sits anywhere in the extension outside .keys\.
$extDir = Join-Path $Root 'extension'
$extBase = (Resolve-Path -LiteralPath $extDir).Path.TrimEnd('\')
foreach ($f in Get-ChildItem -LiteralPath $extBase -Recurse -Force -File) {
    $rel = $f.FullName.Substring($extBase.Length + 1)
    $top = ($rel -split '\\')[0]
    if ($top -eq '.keys' -or $top -eq 'tools') { continue }
    foreach ($pat in @('*.pem', '*.key', '*.p12', '*.pfx', '*.crx')) {
        if ($f.Name -like $pat) { throw "Refusing to build: key-like file extension\$rel" }
    }
}

$IsccExe = $Iscc
if (-not $IsccExe) { $IsccExe = Find-Iscc }

Write-Step "Thrennix $Version installer"
Write-Host "  ISCC:    $IsccExe"
Write-Host "  bin dir: $BinDir"
Write-Host "  out dir: $OutDir"

if (-not (Test-Path -LiteralPath $OutDir)) { New-Item -ItemType Directory -Path $OutDir -Force | Out-Null }
$Setup = Join-Path $OutDir "Thrennix-Setup-$Version.exe"
if (Test-Path -LiteralPath $Setup) { Remove-Item -LiteralPath $Setup -Force }

# ---------------------------------------------------------------- compile

$isccArgs = @(
    '/Qp',
    "/DAppVersion=$Version",
    "/DAppVersionNumeric=$Numeric",
    "/DBinDir=$BinDir",
    "/O$OutDir",
    $IssFile
)
& $IsccExe @isccArgs
if ($LASTEXITCODE -ne 0) { throw "ISCC failed (exit $LASTEXITCODE)" }
if (-not (Test-Path -LiteralPath $Setup)) { throw "ISCC finished but $Setup was not created" }

# ---------------------------------------------------------------- hash

$hash = Get-Sha256 $Setup
$size = (Get-Item -LiteralPath $Setup).Length
Write-Step 'Done'
Write-Host ("  {0}  ({1:N1} MB)" -f $Setup, ($size / 1MB))
Write-Host "  SHA-256: $hash"

$SumsFile = Join-Path $OutDir 'SHA256SUMS.txt'
if (Test-Path -LiteralPath $SumsFile) {
    $leaf = Split-Path -Leaf $Setup
    $lines = @(Get-Content -LiteralPath $SumsFile | Where-Object { $_ -and ($_ -notmatch ('\s' + [regex]::Escape($leaf) + '$')) })
    # Installer first: it is the primary download.
    $lines = @("$hash  $leaf") + $lines
    $utf8 = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($SumsFile, (($lines -join "`n") + "`n"), $utf8)
    Write-Host "  updated $SumsFile"
}
