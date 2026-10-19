<#
.SYNOPSIS
  Download benchmark: thrennix vs aria2 vs IDM.

.DESCRIPTION
  For every URL x run x tool, downloads into a fresh temp folder, measures wall time with a
  Stopwatch, verifies the size (HEAD Content-Length) and SHA-256 equality across tools, then
  deletes the file. Tools are interleaved inside each run (run 1: thrennix, aria2, idm; run 2: ...)
  so that network fluctuations affect all tools similarly.

  Writes <OutDir>\bench-<timestamp>.csv and <OutDir>\bench-<timestamp>.md (medians).
  Windows PowerShell 5.1 compatible.

.EXAMPLE
  .\bench\bench.ps1 -Urls 'https://ash-speed.hetzner.com/1GB.bin' -Runs 3 -Tools thrennix,aria2,idm
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string[]]$Urls,
    [int]$Runs = 3,
    [string[]]$Tools = @('thrennix', 'aria2', 'idm'),
    [string]$OutDir = '',
    [string]$ThrennixExe = '',
    [string]$IdmExe = 'C:\Program Files (x86)\Internet Download Manager\IDMan.exe',
    [int]$TimeoutSec = 1800,
    [string[]]$Labels = @(),
    [switch]$SkipHash
)

$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
if (-not $OutDir) { $OutDir = Join-Path $here 'results' }
if (-not $ThrennixExe) { $ThrennixExe = Join-Path $here '..\target\release\thrennix-cli.exe' }

# "-Tools thrennix,aria2" arrives as one string when the script is run with -File.
$Tools = @($Tools | ForEach-Object { $_ -split ',' } | ForEach-Object { $_.Trim().ToLower() } | Where-Object { $_ })

function Find-Aria2 {
    $cmd = Get-Command aria2c -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    $links = Join-Path $env:LOCALAPPDATA 'Microsoft\WinGet\Links\aria2c.exe'
    if (Test-Path $links) { return $links }
    $pkgRoot = Join-Path $env:LOCALAPPDATA 'Microsoft\WinGet\Packages'
    if (Test-Path $pkgRoot) {
        $found = Get-ChildItem -Path $pkgRoot -Filter aria2c.exe -Recurse -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($found) { return $found.FullName }
    }
    return $null
}

function Quote([string]$s) {
    if ($s -match '[\s"&|<>^]') { return '"' + ($s -replace '"', '\"') + '"' }
    return $s
}

# HEAD request -> expected size and file name.
function Get-RemoteInfo([string]$url) {
    $req = [System.Net.HttpWebRequest]::Create($url)
    $req.Method = 'HEAD'
    $req.AllowAutoRedirect = $true
    $req.Timeout = 30000
    $req.UserAgent = 'thrennix-bench/1.0'
    $resp = $req.GetResponse()
    try {
        $size = [int64]$resp.ContentLength
        $name = $null
        $cd = $resp.Headers['Content-Disposition']
        if ($cd -and ($cd -match 'filename\*?=(?:UTF-8'''')?"?([^";]+)"?')) {
            $name = [System.Uri]::UnescapeDataString($Matches[1])
        }
        if (-not $name) {
            $name = [System.IO.Path]::GetFileName($resp.ResponseUri.AbsolutePath)
            $name = [System.Uri]::UnescapeDataString($name)
        }
        if (-not $name) { $name = 'download.bin' }
        return [pscustomobject]@{ Size = $size; Name = $name; FinalUrl = $resp.ResponseUri.AbsoluteUri }
    } finally {
        $resp.Close()
    }
}

function Start-Timed([string]$exe, [string[]]$argList, [string]$logBase) {
    $argString = ($argList | ForEach-Object { Quote $_ }) -join ' '
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $p = Start-Process -FilePath $exe -ArgumentList $argString -NoNewWindow -PassThru `
        -RedirectStandardOutput "$logBase.out.txt" -RedirectStandardError "$logBase.err.txt"
    $null = $p.Handle  # keep the handle so ExitCode is available after exit
    $finished = $p.WaitForExit($TimeoutSec * 1000)
    $sw.Stop()
    if (-not $finished) {
        try { $p.Kill() } catch { }
        return [pscustomobject]@{ Seconds = $sw.Elapsed.TotalSeconds; ExitCode = -1; TimedOut = $true }
    }
    $p.WaitForExit()
    return [pscustomobject]@{ Seconds = $sw.Elapsed.TotalSeconds; ExitCode = $p.ExitCode; TimedOut = $false }
}

# IDM: launch, then poll until <dir>\<name> has the expected size and is unchanged for 1 s.
# Elapsed time = launch -> first moment the complete file was seen (the 1 s stability window
# is not counted). IDM assembles the file elsewhere and moves it into place when done.
function Start-IdmTimed([string]$url, [string]$dir, [string]$name, [int64]$expected) {
    $target = Join-Path $dir $name
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    Start-Process -FilePath $IdmExe -ArgumentList @('/d', (Quote $url), '/p', (Quote $dir), '/f', (Quote $name), '/n', '/q') | Out-Null
    $completeAt = $null
    $lastSize = -1
    $stableSince = $null
    while ($sw.Elapsed.TotalSeconds -lt $TimeoutSec) {
        Start-Sleep -Milliseconds 50
        if (-not (Test-Path -LiteralPath $target)) { continue }
        try { $len = (Get-Item -LiteralPath $target).Length } catch { continue }
        if ($len -ne $lastSize) {
            $lastSize = $len
            $stableSince = $sw.Elapsed.TotalSeconds
            if (($expected -le 0 -or $len -eq $expected) -and -not $completeAt) { $completeAt = $sw.Elapsed.TotalSeconds }
            if ($expected -gt 0 -and $len -ne $expected) { $completeAt = $null }
            continue
        }
        if ($completeAt -and ($sw.Elapsed.TotalSeconds - $stableSince) -ge 1.0) {
            # Make sure IDM has released the file.
            try { $fs = [System.IO.File]::Open($target, 'Open', 'Read', 'None'); $fs.Close() } catch { continue }
            return [pscustomobject]@{ Seconds = $completeAt; ExitCode = 0; TimedOut = $false }
        }
    }
    return [pscustomobject]@{ Seconds = $sw.Elapsed.TotalSeconds; ExitCode = -1; TimedOut = $true }
}

function Get-Median([double[]]$values) {
    if (-not $values -or $values.Count -eq 0) { return [double]::NaN }
    $s = $values | Sort-Object
    $n = $s.Count
    if ($n % 2 -eq 1) { return [double]$s[[int][math]::Floor($n / 2)] }
    return ([double]$s[$n / 2 - 1] + [double]$s[$n / 2]) / 2.0
}

# ---------------------------------------------------------------- tool availability
$aria2 = $null
$available = @()
foreach ($t in $Tools) {
    switch ($t) {
        'thrennix' {
            if (Test-Path $ThrennixExe) { $ThrennixExe = (Resolve-Path $ThrennixExe).Path; $available += 'thrennix' }
            else { Write-Warning "thrennix-cli not found at $ThrennixExe (run: cargo build --release -p thrennix-cli). Skipping thrennix." }
        }
        'aria2' {
            $aria2 = Find-Aria2
            if ($aria2) { $available += 'aria2' } else { Write-Warning 'aria2c not found (run bench\install-tools.ps1). Skipping aria2.' }
        }
        'idm' {
            if (Test-Path $IdmExe) { $available += 'idm' } else { Write-Warning "IDM not found at $IdmExe. Skipping idm." }
        }
        default { Write-Warning "Unknown tool '$t' (expected thrennix, aria2, idm)." }
    }
}
if ($available.Count -eq 0) { throw 'No benchmark tools available.' }

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$OutDir = (Resolve-Path $OutDir).Path
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$tempRoot = Join-Path ([System.IO.Path]::GetTempPath()) "thrennix-bench-$stamp"
New-Item -ItemType Directory -Force -Path $tempRoot | Out-Null
$logDir = Join-Path $OutDir "logs-$stamp"
New-Item -ItemType Directory -Force -Path $logDir | Out-Null

Write-Host "Tools: $($available -join ', ')   Runs: $Runs   Output: $OutDir"

$rows = New-Object System.Collections.Generic.List[object]
$refHash = @{}

for ($ui = 0; $ui -lt $Urls.Count; $ui++) {
    $url = $Urls[$ui]
    $label = if ($ui -lt $Labels.Count -and $Labels[$ui]) { $Labels[$ui] } else { $url }
    try { $info = Get-RemoteInfo $url } catch { Write-Warning "HEAD failed for ${url}: $($_.Exception.Message)"; continue }
    Write-Host ""
    Write-Host ("== {0}  ({1:N0} bytes, {2})" -f $label, $info.Size, $info.Name)

    for ($run = 1; $run -le $Runs; $run++) {
        foreach ($tool in $available) {
            $dir = Join-Path $tempRoot ("{0}-u{1}-r{2}" -f $tool, $ui, $run)
            New-Item -ItemType Directory -Force -Path $dir | Out-Null
            $logBase = Join-Path $logDir ("{0}-u{1}-r{2}" -f $tool, $ui, $run)
            $status = 'ok'
            $res = $null
            try {
                switch ($tool) {
                    'thrennix' {
                        $res = Start-Timed $ThrennixExe @($url, '-o', $dir, '-f', $info.Name, '--json', '-q') $logBase
                    }
                    'aria2' {
                        $res = Start-Timed $aria2 @('-x16', '-s16', '-k1M', '--file-allocation=none', '--allow-overwrite=true',
                            '--console-log-level=warn', '--summary-interval=0', '-d', $dir, '-o', $info.Name, $url) $logBase
                    }
                    'idm' {
                        $res = Start-IdmTimed $url $dir $info.Name $info.Size
                    }
                }
            } catch {
                $status = "error: $($_.Exception.Message)"
            }

            $file = Join-Path $dir $info.Name
            $bytes = 0
            $hash = ''
            if ($res) {
                if ($res.TimedOut) { $status = 'timeout' }
                elseif ($res.ExitCode -ne 0) { $status = "exit $($res.ExitCode)" }
            }
            if (Test-Path -LiteralPath $file) {
                $bytes = (Get-Item -LiteralPath $file).Length
                if ($status -eq 'ok' -and $info.Size -gt 0 -and $bytes -ne $info.Size) { $status = "size $bytes != $($info.Size)" }
                if ($status -eq 'ok' -and -not $SkipHash) {
                    $hash = (Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash
                    if ($refHash.ContainsKey($url)) {
                        if ($refHash[$url] -ne $hash) { $status = 'sha256 mismatch' }
                    } else {
                        $refHash[$url] = $hash
                    }
                }
            } elseif ($status -eq 'ok') {
                $status = 'file missing'
            }

            $secs = if ($res) { [math]::Round($res.Seconds, 3) } else { [double]::NaN }
            $mbps = if ($status -eq 'ok' -and $secs -gt 0) { [math]::Round($info.Size / 1e6 / $secs, 2) } else { [double]::NaN }
            $row = [pscustomobject]@{
                Tool    = $tool
                Url     = $label
                Run     = $run
                Seconds = $secs
                MBps    = $mbps
                Bytes   = $bytes
                Status  = $status
                Sha256  = $hash
            }
            $rows.Add($row)
            Write-Host ("  run {0}  {1,-6} {2,9:N2} s  {3,9:N2} MB/s  {4}" -f $run, $tool, $secs, $mbps, $status)

            # Clean up (retry: IDM / AV scanners may hold the file briefly).
            for ($k = 0; $k -lt 20; $k++) {
                try { Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction Stop; break } catch { Start-Sleep -Milliseconds 250 }
            }
        }
    }
}

Remove-Item -LiteralPath $tempRoot -Recurse -Force -ErrorAction SilentlyContinue

# ---------------------------------------------------------------- output
Write-Host ""
$rows | Format-Table Tool, Url, Run, Seconds, MBps, Status -AutoSize | Out-String -Width 250 | Write-Host

$csv = Join-Path $OutDir "bench-$stamp.csv"
$rows | Export-Csv -Path $csv -NoTypeInformation -Encoding UTF8

$md = New-Object System.Text.StringBuilder
[void]$md.AppendLine("# Benchmark results ($stamp)")
[void]$md.AppendLine('')
[void]$md.AppendLine("Runs per tool per URL: $Runs. Machine: $env:COMPUTERNAME. MB/s = 10^6 bytes per second. Medians over successful runs.")
[void]$md.AppendLine('')
[void]$md.AppendLine('| URL | Tool | Median s | Median MB/s | OK runs | vs best |')
[void]$md.AppendLine('|---|---|---:|---:|---:|---:|')
$summary = New-Object System.Collections.Generic.List[object]
foreach ($g in ($rows | Group-Object Url)) {
    $perTool = @()
    foreach ($tg in ($g.Group | Group-Object Tool)) {
        $ok = @($tg.Group | Where-Object { $_.Status -eq 'ok' })
        $medS = Get-Median ([double[]]@($ok | ForEach-Object { $_.Seconds }))
        $medM = Get-Median ([double[]]@($ok | ForEach-Object { $_.MBps }))
        $perTool += [pscustomobject]@{ Url = $g.Name; Tool = $tg.Name; MedianSeconds = $medS; MedianMBps = $medM; Ok = $ok.Count; Total = $tg.Count }
    }
    $best = ($perTool | Where-Object { -not [double]::IsNaN($_.MedianMBps) } | Measure-Object MedianMBps -Maximum).Maximum
    foreach ($p in $perTool) {
        $rel = if ($best -and -not [double]::IsNaN($p.MedianMBps)) { '{0:P0}' -f ($p.MedianMBps / $best) } else { '-' }
        $s = if ([double]::IsNaN($p.MedianSeconds)) { '-' } else { '{0:N2}' -f $p.MedianSeconds }
        $m = if ([double]::IsNaN($p.MedianMBps)) { '-' } else { '{0:N2}' -f $p.MedianMBps }
        [void]$md.AppendLine("| ``$($p.Url)`` | $($p.Tool) | $s | $m | $($p.Ok)/$($p.Total) | $rel |")
        $summary.Add($p)
    }
}
$failed = @($rows | Where-Object { $_.Status -ne 'ok' })
if ($failed.Count -gt 0) {
    [void]$md.AppendLine('')
    [void]$md.AppendLine('## Failed runs')
    [void]$md.AppendLine('')
    foreach ($f in $failed) { [void]$md.AppendLine("- $($f.Tool) run $($f.Run) ``$($f.Url)``: $($f.Status)") }
}
$mdPath = Join-Path $OutDir "bench-$stamp.md"
[System.IO.File]::WriteAllText($mdPath, $md.ToString(), (New-Object System.Text.UTF8Encoding($false)))

Write-Host "Median summary:"
$summary | Format-Table Url, Tool, MedianSeconds, MedianMBps, Ok, Total -AutoSize | Out-String -Width 250 | Write-Host
Write-Host "CSV:      $csv"
Write-Host "Markdown: $mdPath"
Write-Host "Logs:     $logDir"
