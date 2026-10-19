# Thrennix benchmarks

Scripts for comparing **thrennix** with **aria2** and **Internet Download Manager (IDM)**. They work in
Windows PowerShell 5.1 and in PowerShell 7.

| Script | What it does |
|---|---|
| `install-tools.ps1` | Installs aria2 with winget (`aria2.aria2`) if `aria2c` isn't found, and reports whether IDM is installed. |
| `bench.ps1` | The benchmark itself: URL × run × tool, with timing, size and SHA-256 checks, a table, CSV and a Markdown summary. |
| `local-bench.ps1` | Builds the release binaries, starts `thrennix-testserver`, runs `bench.ps1` on four reproducible local scenarios, then stops the server. |

## Quick start

```powershell
cd <repo>
powershell -ExecutionPolicy Bypass -File bench\install-tools.ps1
cargo build --release -p thrennix-cli -p thrennix-testserver

# Local, reproducible scenarios (about 10 to 15 min for 3 runs × 3 tools):
powershell -ExecutionPolicy Bypass -File bench\local-bench.ps1 -Tools thrennix,aria2,idm -Runs 3
# Fast smoke run with smaller files:
powershell -ExecutionPolicy Bypass -File bench\local-bench.ps1 -Tools thrennix,aria2 -Runs 1 -Quick

# Real internet:
powershell -ExecutionPolicy Bypass -File bench\bench.ps1 -Runs 3 -Tools thrennix,aria2,idm `
    -Urls https://proof.ovh.net/files/1Gb.dat
```

To pass several URLs, call the script from a PowerShell prompt, because `-File` cannot pass arrays:
`.\bench\bench.ps1 -Urls 'https://a/1.bin','https://b/2.bin' -Runs 3`.

Output goes to `bench\results\` (change it with `-OutDir`):
- `bench-<timestamp>.csv`: one row per run (Tool, Url, Run, Seconds, MBps, Bytes, Status, Sha256).
- `bench-<timestamp>.md`: the median per tool per URL, the result relative to the best tool, and the failed runs.
- `logs-<timestamp>\`: stdout and stderr of each thrennix and aria2 run.

`MB/s` means 10^6 bytes per second, calculated from the expected file size and the wall time.

## How each tool is run and timed

| Tool | Command | Timing |
|---|---|---|
| thrennix | `target\release\thrennix-cli.exe <url> -o <dir> -f <name> --json -q` | Stopwatch from process start to process exit |
| aria2 | `aria2c -x16 -s16 -k1M --file-allocation=none --allow-overwrite=true -d <dir> -o <name> <url>` | Stopwatch from process start to process exit |
| IDM | `IDMan.exe /d <url> /p <dir> /f <name> /n /q` | Polling (see below) |

- Each run downloads into a new, empty temp folder. The file is deleted afterwards.
- Before the runs, a `HEAD` request gets the expected size (`Content-Length`) and the file name. A
  run only counts as `ok` if the final file has exactly that size. Its SHA-256 must also match the
  first successful download of the same URL by any tool, so all tools are checked against each
  other. `-SkipHash` turns off the hash check for very large files.
- Tools take turns within each run (run 1: thrennix, aria2, idm; run 2: thrennix, aria2, idm; …). A
  temporary network slowdown therefore hits all tools, not just one.

### IDM measurement method (a bit less precise)

IDM normally keeps running in the system tray. In that case `IDMan.exe /d …` only hands the job to
the running instance and returns straight away, and `/q` (exit after download) has no effect.
`bench.ps1` therefore doesn't wait for a process. It works like this:

1. It starts the Stopwatch and launches `IDMan.exe /d <url> /p <dir> /f <name> /n /q` (`/n` turns off
   IDM's questions).
2. Every 50 ms it checks whether `<dir>\<name>` exists. IDM builds the file in its own temp folder
   and moves it into place when the download is done.
3. The first time the file is seen with the expected size, that moment is recorded as the finish
   time.
4. The run is accepted once the size has stayed the same for 1 s and the file can be opened
   exclusively, which shows IDM has released it. These 1 s are **not** counted in the result.

So the IDM time includes IDM assembling and moving the file, plus up to about 50 ms of polling
delay. The thrennix and aria2 times include process startup and shutdown instead. On a 1 GB file these
differences are below 1%. On small files they matter, so benchmark files of 500 MB or more.

Check before you benchmark IDM:
- Set the connection count in *Options → Connection → Max. connections number* to the value you
  want to compare (the research plan uses **16**; IDM's default is 8), and write it down with your
  results.
- Turn off the "Download complete" dialog and the "Start download" confirmation dialog in IDM's
  options. Otherwise the benchmark hangs until someone clicks.
- IDM's speed limiter must be off.

## Fair real-internet benchmarks

1. **Same file for all tools**, 1 GB or larger. Use several servers or CDNs, because results differ
   a lot between servers.
2. **Same time window.** `bench.ps1` alternates the tools inside every run. Do all runs in one
   session.
3. **Nothing else downloading.** Close browsers, Steam, OneDrive, Windows Update and so on. Use a
   wired connection if you can.
4. **At least 3 runs**, and report the **median** (the `.md` file does this). Report failed runs
   too; don't drop them.
5. **Same settings**: give every tool the same connection count, or document the difference
   (thrennix: `-m`, aria2: `-x/-s`, IDM: options).
6. Turn off the speed limits of all tools. If antivirus slows writes noticeably, add the same
   exclusion for every tool.
7. Report your line speed. If every tool reaches your line limit, the test tells you little. Use a
   throttled or far-away server, or the local scenarios.
8. Beyond the average speed, the plan in `IDM_Research_and_Faster_Design.md` §6 also looks at
   time-to-full-speed and the length of the end phase. For thrennix, `--json` reports `peak_speed_bps`
   and `max_connections`.

### Public test files (each answered a HEAD request with 200, the full Content-Length and `Accept-Ranges: bytes` on 2026-10-19)

| URL | Size | Location |
|---|---|---|
| `https://proof.ovh.net/files/1Gb.dat` | 1 GiB | OVH, France |
| `https://proof.ovh.net/files/10Gb.dat` | 10 GiB | OVH, France |
| `http://speedtest.tele2.net/1GB.zip` | 1 GiB | Tele2, Sweden (plain HTTP) |
| `http://speedtest.tele2.net/10GB.zip` | 10 GiB | Tele2, Sweden (plain HTTP) |
| `https://lon.speedtest.clouvider.net/1g.bin` | 1000 MiB | Clouvider, London |
| `https://fra.speedtest.clouvider.net/1g.bin` | 1000 MiB | Clouvider, Frankfurt |
| `https://ams.speedtest.clouvider.net/1g.bin` | 1000 MiB | Clouvider, Amsterdam |
| `https://nyc.speedtest.clouvider.net/1g.bin` | 1000 MiB | Clouvider, New York |
| `https://la.speedtest.clouvider.net/1g.bin` | 1000 MiB | Clouvider, Los Angeles |
| `https://speedtest.selectel.ru/1GB` | 1 GiB | Selectel, Russia |

These did **not** respond from the test machine on that date, so they are left out:
`speed.hetzner.de` and `*-speed.hetzner.com` (connection made, but no response), thinkbroadband
(403), Vultr `*-ping.vultr.com` (the file is gone), Leaseweb mirrors, and Cloudflare
`speed.cloudflare.com/__down` (403 for large sizes). Run a quick
`curl -sI <url>` check before you rely on any of them.

## Local scenarios (`local-bench.ps1`)

`thrennix-testserver` serves virtual files (`GET /file/{size}`, where byte i = `(i*31 + i/7) as u8`) with
full Range, ETag and If-Range support. Query parameters simulate real servers:

| Parameter | Effect |
|---|---|
| `rate=<KiB/s>` | Throttle each connection (token bucket per response), like many CDNs |
| `total_rate=<KiB/s>` | One shared limit across all connections (simulates the user's line) |
| `max_conns=<n>` | 503 when more than n responses are active |
| `norange=1` | Ignore Range and always send 200 with the full body |
| `latency=<ms>` | Delay before the first byte |
| `fail=<p>` | Probability that a response is cut off mid-body |
| `etag=<v>` | Override the ETag (simulates a changed file) |

The four scenarios:

| | URL | What it shows |
|---|---|---|
| a | `/file/1G?rate=2048` | 2 MiB/s per connection: the benefit of many connections and of adding them adaptively |
| b | `/file/1G?total_rate=40960&rate=4096` | 40 MiB/s line, 4 MiB/s per connection: ≥10 connections fill the line, and more give nothing |
| c | `/file/500M?latency=150&fail=0.02` | Unstable, high-latency server: retry and recovery cost |
| d | `/file/500M?max_conns=6&rate=4096` | Server allows only 6 connections: handling of 503s |

Other server endpoints: `GET /stats` (total requests, peak concurrency, bytes served, 503s, drops),
`POST /stats/reset`. `thrennix-testserver --tls` serves HTTPS with a self-signed certificate that it
generates at startup and prints the path of, so you can compare TLS handshake and resumption cost.
`--quiet` turns off the log line written for each request.

IDM may handle `127.0.0.1` differently from real hosts. Local IDM numbers are an indication, not
proof.
