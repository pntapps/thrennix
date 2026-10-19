<#
.SYNOPSIS
    Builds the Thrennix release archives from already-compiled binaries.

.DESCRIPTION
    Collects thrennix.exe, thrennix-cli.exe, the licenses, README.md, the browser extension
    (without .keys/ and tools/) and the install/uninstall scripts into
    dist\thrennix-<version>-windows-x64\, zips it, builds a separate
    thrennix-extension-<version>.zip for store upload, and writes dist\SHA256SUMS.txt.

    Compatible with Windows PowerShell 5.1 and PowerShell 7+.

.PARAMETER BinDir
    Folder that holds thrennix.exe and thrennix-cli.exe. Default: target\release, or
    target\x86_64-pc-windows-msvc\release when the former has no thrennix.exe.

.PARAMETER OutDir
    Output folder. Default: dist (under the repository root).

.PARAMETER Version
    Override the version. Default: [workspace.package] version from Cargo.toml.

.PARAMETER DryRun
    Only print what would be copied and created. Nothing is written.

.EXAMPLE
    cargo build --release
    powershell -ExecutionPolicy Bypass -File packaging\package.ps1
#>
[CmdletBinding()]
param(
    [string]$BinDir,
    [string]$OutDir,
    [string]$Version,
    [switch]$DryRun
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'

$Root = Split-Path -Parent $PSScriptRoot
$PackagingDir = $PSScriptRoot
$ExtensionDir = Join-Path $Root 'extension'

# Top-level names inside extension/ that never ship.
$ExtensionExcludeDirs = @('.keys', 'tools')
$ExtensionExcludeFiles = @('.gitignore')
# Defence in depth: refuse to package anything that looks like a private key.
$ForbiddenPatterns = @('*.pem', '*.key', '*.p12', '*.pfx')

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

# Returns the extension files to ship as objects with Source (full path) and Rel
# (path relative to extension/, using '\').
function Get-ExtensionFiles {
    $base = (Resolve-Path -LiteralPath $ExtensionDir).Path.TrimEnd('\')
    $result = @()
    foreach ($item in Get-ChildItem -LiteralPath $base -Recurse -Force -File) {
        $rel = $item.FullName.Substring($base.Length + 1)
        $top = ($rel -split '\\')[0]
        if ($ExtensionExcludeDirs -contains $top) { continue }
        if ($ExtensionExcludeFiles -contains $rel) { continue }
        foreach ($pat in $ForbiddenPatterns) {
            if ($item.Name -like $pat) {
                throw "Refusing to package a key-like file: extension\$rel"
            }
        }
        $result += New-Object PSObject -Property @{ Source = $item.FullName; Rel = $rel }
    }
    return $result
}

function Copy-OneFile([string]$Source, [string]$Dest) {
    if ($DryRun) {
        Write-Host "  copy $Source"
        Write-Host "    -> $Dest"
        return
    }
    $parent = Split-Path -Parent $Dest
    if (-not (Test-Path -LiteralPath $parent)) {
        New-Item -ItemType Directory -Path $parent -Force | Out-Null
    }
    Copy-Item -LiteralPath $Source -Destination $Dest -Force
}

# Zips the contents of $SourceDir. Entry names use '/' (Compress-Archive on 5.1
# writes '\', which breaks some unzip tools and store validators). When $Prefix
# is set, every entry is placed under that top-level folder.
function New-ZipFromDir([string]$SourceDir, [string]$ZipPath, [string]$Prefix) {
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    if (Test-Path -LiteralPath $ZipPath) { Remove-Item -LiteralPath $ZipPath -Force }
    $base = (Resolve-Path -LiteralPath $SourceDir).Path.TrimEnd('\')
    $zip = [System.IO.Compression.ZipFile]::Open($ZipPath, [System.IO.Compression.ZipArchiveMode]::Create)
    try {
        foreach ($f in Get-ChildItem -LiteralPath $base -Recurse -Force -File | Sort-Object FullName) {
            $name = $f.FullName.Substring($base.Length + 1).Replace('\', '/')
            if ($Prefix) { $name = "$Prefix/$name" }
            [System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
                $zip, $f.FullName, $name, [System.IO.Compression.CompressionLevel]::Optimal) | Out-Null
        }
    }
    finally {
        $zip.Dispose()
    }
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

if ($OutDir) {
    $OutDir = Resolve-FullPath $OutDir
}
else {
    $OutDir = Join-Path $Root 'dist'
}

$PackageName = "thrennix-$Version-windows-x64"
$StageDir = Join-Path $OutDir $PackageName
$MainZip = Join-Path $OutDir "$PackageName.zip"
$ExtName = "thrennix-extension-$Version"
$ExtStage = Join-Path $OutDir $ExtName
$ExtZip = Join-Path $OutDir "$ExtName.zip"
$SumsFile = Join-Path $OutDir 'SHA256SUMS.txt'

Write-Step "Thrennix $Version"
Write-Host "  repo:    $Root"
Write-Host "  bin dir: $BinDir"
Write-Host "  out dir: $OutDir"
if ($DryRun) { Write-Host '  DRY RUN: nothing will be written' -ForegroundColor Yellow }

# Top-level files: source -> name inside the package.
$TopFiles = @(
    @{ Src = (Join-Path $BinDir 'thrennix.exe'); Name = 'thrennix.exe' },
    @{ Src = (Join-Path $BinDir 'thrennix-cli.exe'); Name = 'thrennix-cli.exe' },
    @{ Src = (Join-Path $Root 'LICENSE-MIT'); Name = 'LICENSE-MIT' },
    @{ Src = (Join-Path $Root 'LICENSE-APACHE'); Name = 'LICENSE-APACHE' },
    @{ Src = (Join-Path $Root 'README.md'); Name = 'README.md' },
    @{ Src = (Join-Path $PackagingDir 'install.ps1'); Name = 'install.ps1' },
    @{ Src = (Join-Path $PackagingDir 'uninstall.ps1'); Name = 'uninstall.ps1' }
)

$missing = @()
foreach ($f in $TopFiles) {
    if (-not (Test-Path -LiteralPath $f.Src)) { $missing += $f.Src }
}
if ($missing.Count -gt 0) {
    if ($DryRun) {
        foreach ($m in $missing) { Write-Warning "missing (would fail for real): $m" }
    }
    else {
        throw ("Missing input file(s):`n  " + ($missing -join "`n  ") + "`nBuild first: cargo build --release")
    }
}

$ExtFiles = Get-ExtensionFiles
if (-not ($ExtFiles | Where-Object { $_.Rel -eq 'manifest.json' })) {
    throw 'extension\manifest.json not found'
}

# Warn if the extension version drifts from the workspace version.
$manifestText = Get-Content -LiteralPath (Join-Path $ExtensionDir 'manifest.json') -Raw
$manifest = $manifestText | ConvertFrom-Json
if ($manifest.version -ne $Version) {
    Write-Warning "extension/manifest.json version ($($manifest.version)) differs from the release version ($Version)"
}

# ---------------------------------------------------------------- stage

if (-not $DryRun) {
    if (-not (Test-Path -LiteralPath $OutDir)) { New-Item -ItemType Directory -Path $OutDir -Force | Out-Null }
    foreach ($d in @($StageDir, $ExtStage)) {
        if (Test-Path -LiteralPath $d) { Remove-Item -LiteralPath $d -Recurse -Force }
    }
    foreach ($z in @($MainZip, $ExtZip, $SumsFile)) {
        if (Test-Path -LiteralPath $z) { Remove-Item -LiteralPath $z -Force }
    }
}

Write-Step "Staging $StageDir"
foreach ($f in $TopFiles) {
    Copy-OneFile $f.Src (Join-Path $StageDir $f.Name)
}
foreach ($f in $ExtFiles) {
    Copy-OneFile $f.Source (Join-Path (Join-Path $StageDir 'extension') $f.Rel)
}

Write-Step "Staging $ExtStage (store upload)"
foreach ($f in $ExtFiles) {
    Copy-OneFile $f.Source (Join-Path $ExtStage $f.Rel)
}
# The Chrome Web Store rejects manifests with a "key" field (the store assigns its
# own key). Strip that one line from the store copy, keeping the formatting intact.
$storeManifest = Join-Path $ExtStage 'manifest.json'
if ($DryRun) {
    Write-Host '  strip "key" from the store copy of manifest.json'
}
else {
    $lines = Get-Content -LiteralPath $storeManifest -Encoding UTF8
    $kept = @($lines | Where-Object { $_ -notmatch '^\s*"key"\s*:\s*"[^"]*"\s*,?\s*$' })
    if ($kept.Count -ne $lines.Count) {
        $utf8 = New-Object System.Text.UTF8Encoding($false)
        [System.IO.File]::WriteAllText($storeManifest, (($kept -join "`n") + "`n"), $utf8)
        # Make sure the result is still valid JSON.
        Get-Content -LiteralPath $storeManifest -Raw -Encoding UTF8 | ConvertFrom-Json | Out-Null
    }
}

# ---------------------------------------------------------------- zip + sums

Write-Step "Creating $MainZip"
if (-not $DryRun) { New-ZipFromDir $StageDir $MainZip $PackageName }

Write-Step "Creating $ExtZip"
if (-not $DryRun) { New-ZipFromDir $ExtStage $ExtZip '' }

Write-Step "Writing $SumsFile"
if (-not $DryRun) {
    $sumLines = @()
    foreach ($z in @($MainZip, $ExtZip)) {
        $sumLines += ('{0}  {1}' -f (Get-Sha256 $z), (Split-Path -Leaf $z))
    }
    $utf8 = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($SumsFile, (($sumLines -join "`n") + "`n"), $utf8)
    Get-Content -LiteralPath $SumsFile | ForEach-Object { Write-Host "  $_" }
}

Write-Step 'Done'
if ($DryRun) {
    Write-Host "  would create: $MainZip"
    Write-Host "  would create: $ExtZip"
    Write-Host "  would create: $SumsFile"
}
