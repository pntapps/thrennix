<#
.SYNOPSIS
    Uninstalls Thrennix for the current user.

.DESCRIPTION
    Unregisters the browser integration (thrennix.exe --unregister), removes the Start Menu
    shortcut and the install folder. Settings and download state in %APPDATA%\Thrennix are
    kept unless -RemoveData is given.

        powershell -ExecutionPolicy Bypass -File "$env:LOCALAPPDATA\Programs\Thrennix\uninstall.ps1"

    The browser extension itself is not removed; remove it from the browser's
    extensions page.

.PARAMETER InstallDir
    Install location. Default: %LOCALAPPDATA%\Programs\Thrennix.

.PARAMETER Force
    Do not ask for confirmation.

.PARAMETER RemoveData
    Also delete %APPDATA%\Thrennix (settings, download list, resume state).
#>
[CmdletBinding()]
param(
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\Thrennix'),
    [switch]$Force,
    [switch]$RemoveData
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'

$InstallDir = [System.IO.Path]::GetFullPath($InstallDir).TrimEnd('\')
$DataDir = Join-Path $env:APPDATA 'Thrennix'
$HostDir = Join-Path $env:LOCALAPPDATA 'Thrennix'
$LnkPath = Join-Path ([Environment]::GetFolderPath('Programs')) 'Thrennix.lnk'
$exe = Join-Path $InstallDir 'thrennix.exe'

# Guard against a mistyped -InstallDir wiping something unrelated.
if (-not (Test-Path -LiteralPath $exe)) {
    if (Test-Path -LiteralPath $InstallDir) {
        throw "$InstallDir does not contain thrennix.exe; refusing to delete it."
    }
    Write-Warning "Thrennix does not appear to be installed in $InstallDir"
}

Write-Host 'This will remove:'
Write-Host '  - the browser integration (native messaging host registration)'
Write-Host "  - $LnkPath"
Write-Host "  - $InstallDir"
if ($RemoveData) {
    Write-Host "  - $DataDir (settings and download state)"
}
else {
    Write-Host "Your settings and download state in $DataDir are kept (use -RemoveData to delete them)."
}

if (-not $Force) {
    $answer = Read-Host 'Continue? [y/N]'
    if ($answer -notmatch '^(y|yes)$') {
        Write-Host 'Cancelled.'
        return
    }
}

# Stop running copies (the app, or a native host the browser started).
$running = @(Get-Process -Name 'thrennix', 'thrennix-cli' -ErrorAction SilentlyContinue | Where-Object {
        $p = $null
        try { $p = $_.Path } catch { }
        $p -and $p.StartsWith($InstallDir + '\', [System.StringComparison]::OrdinalIgnoreCase)
    })
if ($running.Count -gt 0) {
    Write-Host "Stopping $($running.Count) running Thrennix process(es)"
    $running | Stop-Process -Force
    Start-Sleep -Milliseconds 500
}

$unregistered = $false
if (Test-Path -LiteralPath $exe) {
    Write-Host 'Unregistering the browser integration'
    $proc = Start-Process -FilePath $exe -ArgumentList '--unregister' -Wait -PassThru -WindowStyle Hidden
    if ($proc.ExitCode -eq 0) {
        $unregistered = $true
    }
    else {
        Write-Warning "thrennix.exe --unregister failed with exit code $($proc.ExitCode); cleaning up manually"
    }
}
if (-not $unregistered) {
    # Same keys and files that thrennix.exe --register creates.
    $keys = @(
        'HKCU:\Software\Google\Chrome\NativeMessagingHosts\com.thrennix.host',
        'HKCU:\Software\Microsoft\Edge\NativeMessagingHosts\com.thrennix.host',
        'HKCU:\Software\Chromium\NativeMessagingHosts\com.thrennix.host',
        'HKCU:\Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\com.thrennix.host',
        'HKCU:\Software\Mozilla\NativeMessagingHosts\com.thrennix.host'
    )
    foreach ($k in $keys) {
        if (Test-Path -LiteralPath $k) { Remove-Item -LiteralPath $k -Recurse -Force }
    }
    foreach ($f in @('com.thrennix.host.chrome.json', 'com.thrennix.host.firefox.json')) {
        $path = Join-Path $HostDir $f
        if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path -Force }
    }
}
# Remove the (now empty) native host manifest folder.
if ((Test-Path -LiteralPath $HostDir) -and -not (Get-ChildItem -LiteralPath $HostDir -Force)) {
    Remove-Item -LiteralPath $HostDir -Force
}

if (Test-Path -LiteralPath $LnkPath) {
    Remove-Item -LiteralPath $LnkPath -Force
    Write-Host "Removed $LnkPath"
}

if (Test-Path -LiteralPath $InstallDir) {
    # This script may live inside the install folder; step out of it first.
    $here = (Get-Location).ProviderPath
    if ($here.StartsWith($InstallDir, [System.StringComparison]::OrdinalIgnoreCase)) {
        Set-Location -LiteralPath $env:USERPROFILE
    }
    Remove-Item -LiteralPath $InstallDir -Recurse -Force
    Write-Host "Removed $InstallDir"
}

if ($RemoveData -and (Test-Path -LiteralPath $DataDir)) {
    Remove-Item -LiteralPath $DataDir -Recurse -Force
    Write-Host "Removed $DataDir"
}

Write-Host ''
Write-Host 'Thrennix has been uninstalled.' -ForegroundColor Green
Write-Host 'Remove the Thrennix extension from your browser (chrome://extensions, edge://extensions or about:addons).'
