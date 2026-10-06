<p align="center"><img src="assets/snifrig-badge.svg" width="300" alt="Snifrig: a bloodhound in a Viking helmet resting its chin on a PC tower"></p>

# Snifrig

Low-footprint Windows leak monitor. It sniffs out what is slowly eating your
rig: kernel pool leaks, handle leaks, runaway memory, and process-spawn churn.
One process, no child processes, no GPU use, idle priority, no network.

It reads the kernel tables directly (`NtQuerySystemInformation`) instead of
running other tools, so the monitor does not add to the problem it is watching.

## What it watches

- Kernel pool by tag (the `Toke`, `SeAt`, `FMfn`, `WCsc` family) and growth per hour
- Handle counts and private memory per process, top 25 of each
- Commit charge, free RAM, paged and non-paged pool
- Process and token creation rate, from the kernel's own allocation counters, so
  even processes too short-lived to see in a snapshot are counted
- Free disk space

Alerts say what is growing, how fast, and what usually causes it. Growth alerts
need at least 5 minutes of history, so a short burst is not reported as a leak.

## Use

```
snifrig --once                 report now (two samples 10 s apart)
snifrig install                start at login: hidden monitor + tray icon
snifrig uninstall              remove login startup, stop the monitor
snifrig license statement      usage statement from local records
```

The tray icon shows green (ok), red (alert) or gray (monitor not running).
Right-click for the report, the alerts, the data folder, or to quit.
Data lives in `%LOCALAPPDATA%\snifrig`: `snifrig.jsonl` (history),
`alerts.jsonl`, `status.json`. Both logs are size-capped.

## Tray flyout

Left-click the tray icon to see CPU, RAM, GPU and VRAM with the top three consumers of each (for example which process holds your VRAM). Data is collected only while the flyout opens (snifrig --snapshot), using Windows per-process GPU counters, so it adds nothing to the background monitor.

## Footprint

It checks itself every cycle and exits if it breaks its own budget: working set
over 48 MB, thread or handle growth, private memory growth over 8 MB, or CPU
over 1%. Measured on a 64 GB workstation: about 6 MB working set, 74 handles,
about 0.01% CPU at the default 60 second interval. The tray is a separate
process that only reads `status.json`.

## Build

```
cargo build --release
```

Produces `snifrig.exe`, `snifrigd.exe` (windowless monitor) and
`snifrig-tray.exe`. Run `snifrig install` from the folder that holds them.
Windows 10/11 x64. macOS and Linux versions are planned and need their own probes.

## License

Licensed under UFL 3.4, Operational Scope: **Noncommercial**.
Free for non-commercial and home use. Commercial use is paid: see
[COMMERCIAL.md](COMMERCIAL.md). Cite as `LicenseRef-UFL-3.4-N`.

The first run asks you to accept the license by version and scope (Section 9).
The acceptance is recorded only on your own computer. Snifrig never sends usage
data anywhere.

UFL is not on the SPDX list and is not OSI open source. Full terms in
[LICENSE](LICENSE); canonical text at
https://github.com/estejosh/UFL-Usufruct-License.
