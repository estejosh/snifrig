<p align="center"><img src="assets/snifrig-badge.svg" width="300" alt="Snifrig: a bloodhound in a Viking helmet resting its chin on a PC tower"></p>

# Snifrig

Low-footprint Windows leak monitor. It sniffs out what is slowly eating your
rig: kernel pool leaks, handle leaks, runaway memory, and process-spawn churn.
One resident process at idle priority (the GPU readout runs only while the tray flyout is open), no network connection unless a webhook is configured.

It reads the kernel tables directly (`NtQuerySystemInformation`) instead of
running other tools, so the monitor does not add to the problem it is watching.

## What it watches

- Kernel pool by tag (the `Toke`, `SeAt`, `FMfn`, `WCsc` family) and growth per hour
- Handle counts and private memory per process, top 25 of each
- Commit charge, free RAM, paged and non-paged pool
- Process and token creation rate, from the kernel's own allocation counters, so
  even processes too short-lived to see in a snapshot are counted
- Free disk space
- Windows slowdown evidence (`snifrig slowdown`)

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

## Commands

Output of `snifrig --help`:

| Command | What it does |
|---|---|
| `snifrig --once [--sample SECS]` | print a report now (2 samples, default 10 s apart) |
| `snifrig [--interval SECS]` | watch loop (default 60), alerts to alerts.jsonl |
| `snifrig install` | start at login (hidden monitor + tray icon), start now |
| `snifrig pause [MIN]` | pause fixing (default 60 min); monitoring continues |
| `snifrig resume` | resume fixing |
| `snifrig webhook URL\|off` | POST alerts as JSON to URL (opt-in, e.g. n8n) |
| `snifrig slowdown [--dir PATH]` | summarize evidence of Windows slowing this PC (last 24 h) |
| `snifrig uninstall` | remove login startup and stop the monitor |
| `snifrig license accept` | record acceptance of UFL-3.7 |
| `snifrig license statement` | print a usage statement from local records |

Options: `--dir PATH`, `--cycles N`, `--accept-license UFL-3.7` (or env `SNIFRIG_ACCEPT_LICENSE`).

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

## Privacy

snifrig makes no network connection unless you set a webhook with `snifrig webhook URL`, and then it only posts alerts to that URL.
Everything else stays on your computer in `%LOCALAPPDATA%\snifrig`.

## Fixer

`snifrig-fix` is the paid Component of snifrig. It reads the monitor's alerts and, when it is safe, trims, lowers the priority of, terminates or restarts the process behind a leak. It never runs without a valid key. See [fixer/README.md](fixer/README.md) for modes, safety rules, files and commands, and [PRICING.md](PRICING.md) for the price.

The tray right-click menu lists pending fixes (Approve or Dismiss) and shows the fixer mode, which can be changed from its submenu.

## Notice

The monitor shows a Notice Screen as required by UFL Section 2D:

- The tray shows one short Notice at startup. It stays up for at most 8 seconds, shows once per login, closes on click or Esc, and never takes focus.
- Interactive command-line reports end with one Notice line.
- The Notice cannot be turned off.
- Nothing is sent anywhere.

## Licensing

Release statement: UFL 3.7, Operational Scope: Unconditional; Component snifrig-fix: Paid (`LicenseRef-UFL-3.7-U.P-snifrig-fix`).

| Who | What you owe |
|---|---|
| Anyone using the monitor/detector (everything outside `fixer/`), companies included | Nothing. Operational Scope Unconditional. |
| Anyone running the fixer (`snifrig-fix`), even to try it | Paid Use at the Published Price. See [PRICING.md](PRICING.md). |
| Having the fixer on disk without running it | Nothing |

Copyright holder: estejosh. Earlier releases and commits keep the license they
were published under (UFL 3.4 Noncommercial).

UFL 3.7 governs this software; see LICENSE.

UFL is not on the SPDX list and is not OSI open source. Full terms in
[LICENSE](LICENSE); canonical text at
https://github.com/estejosh/UFL-Usufruct-License.

## Buying the fixer

`snifrig-fix` needs a key file. Keys are offline, Ed25519-signed files, valid for 30 days and tied to the paid period. There is no trial and no free dry-run.

1. Email hello@fungibility.group with the number of computers. Prices are in [PRICING.md](PRICING.md).
2. A key is emailed for each 30-day period while you are paid. If you want the key bound to one machine, run `snifrig-fix machine-id` and send the hash with your request.
3. Install the key: `snifrig-fix license install PATH`.
4. Accept the Component: `snifrig-fix license accept`.
5. Check the state: `snifrig-fix license status`.

The key is checked entirely on your computer, with no network call. A clock set backwards is refused. This keeps honest users honest; it is not uncrackable.
