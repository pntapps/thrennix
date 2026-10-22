<#
.SYNOPSIS
    Installs Thrennix for the current user (no administrator rights needed).

.DESCRIPTION
    Copies Thrennix to %LOCALAPPDATA%\Programs\Thrennix, registers the browser integration
    (native messaging host for Chrome, Edge, Brave, Chromium and Firefox), and adds a
    Start Menu shortcut. Run it from the extracted release folder:

        powershell -ExecutionPolicy Bypass -File .\install.ps1

    Running it again upgrades in place. Settings and download state in %APPDATA%\Thrennix
    are not touched.

.PARAMETER InstallDir
    Install location. Default: %LOCALAPPDATA%\Programs\Thrennix.
#>
[CmdletBinding()]
param(
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\Thrennix')
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'

$Source = $PSScriptRoot
$Payload = @('thrennix.exe', 'thrennix-cli.exe', 'LICENSE', 'NOTICE', 'README.md', 'uninstall.ps1')

foreach ($name in @('thrennix.exe', 'extension')) {
    if (-not (Test-Path -LiteralPath (Join-Path $Source $name))) {
        throw "'$name' not found next to install.ps1. Run this script from the extracted Thrennix release folder."
    }
}

$InstallDir = [System.IO.Path]::GetFullPath($InstallDir)
if ([System.IO.Path]::GetFullPath($Source).TrimEnd('\') -ieq $InstallDir.TrimEnd('\')) {
    throw "install.ps1 is already running from the install folder ($InstallDir). Run it from the extracted release folder instead."
}

Write-Host "Installing Thrennix to $InstallDir"

# Stop any running copy from the install folder (the app, or a native host the
# browser started), otherwise the .exe files are locked.
$running = @(Get-Process -Name 'thrennix', 'thrennix-cli' -ErrorAction SilentlyContinue | Where-Object {
        $p = $null
        try { $p = $_.Path } catch { }
        $p -and $p.StartsWith($InstallDir.TrimEnd('\') + '\', [System.StringComparison]::OrdinalIgnoreCase)
    })
if ($running.Count -gt 0) {
    Write-Host "Stopping $($running.Count) running Thrennix process(es)"
    $running | Stop-Process -Force
    Start-Sleep -Milliseconds 500
}

if (-not (Test-Path -LiteralPath $InstallDir)) {
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
}

foreach ($name in $Payload) {
    $src = Join-Path $Source $name
    if (Test-Path -LiteralPath $src) {
        Copy-Item -LiteralPath $src -Destination (Join-Path $InstallDir $name) -Force
    }
    else {
        Write-Warning "$name not found in the release folder; skipped"
    }
}

# Replace the extension folder wholesale so files removed upstream don't linger.
$extDest = Join-Path $InstallDir 'extension'
if (Test-Path -LiteralPath $extDest) {
    Remove-Item -LiteralPath $extDest -Recurse -Force
}
Copy-Item -LiteralPath (Join-Path $Source 'extension') -Destination $extDest -Recurse -Force

# Register the native messaging host. thrennix.exe records its own path, so it must be
# the installed copy that runs this.
$exe = Join-Path $InstallDir 'thrennix.exe'
Write-Host 'Registering the browser integration'
$proc = Start-Process -FilePath $exe -ArgumentList '--register' -Wait -PassThru -WindowStyle Hidden
if ($proc.ExitCode -ne 0) {
    throw "thrennix.exe --register failed with exit code $($proc.ExitCode)"
}

# Start Menu shortcut (per user).
$programs = [Environment]::GetFolderPath('Programs')
$lnkPath = Join-Path $programs 'Thrennix.lnk'
$shell = New-Object -ComObject WScript.Shell
try {
    $lnk = $shell.CreateShortcut($lnkPath)
    $lnk.TargetPath = $exe
    $lnk.WorkingDirectory = $InstallDir
    $lnk.IconLocation = "$exe,0"
    $lnk.Description = 'Thrennix download manager'
    $lnk.Save()
}
finally {
    [System.Runtime.InteropServices.Marshal]::ReleaseComObject($shell) | Out-Null
}
Write-Host "Created Start Menu shortcut: $lnkPath"

Write-Host ''
Write-Host 'Thrennix is installed.' -ForegroundColor Green
Write-Host ''
Write-Host 'Next steps: load the browser extension'
Write-Host "  Extension folder: $extDest"
Write-Host ''
Write-Host '  Chrome / Edge / Brave'
Write-Host '    1. Open chrome://extensions (or edge://extensions)'
Write-Host '    2. Turn on Developer mode'
Write-Host "    3. Click 'Load unpacked' and select the extension folder above"
Write-Host ''
Write-Host '  Firefox (128+)'
Write-Host '    1. Open about:debugging#/runtime/this-firefox'
Write-Host "    2. Click 'Load Temporary Add-on...' and pick manifest.json in the extension folder"
Write-Host '    3. Allow access to all websites under about:addons > Thrennix > Permissions'
Write-Host ''
Write-Host "Start Thrennix from the Start Menu, or run: `"$exe`""
Write-Host "Command line downloader: `"$(Join-Path $InstallDir 'thrennix-cli.exe')`""
Write-Host "To uninstall: powershell -ExecutionPolicy Bypass -File `"$(Join-Path $InstallDir 'uninstall.ps1')`""
