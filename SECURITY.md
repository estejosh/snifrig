# Security Policy

## Reporting a vulnerability

Email hello@fungibility.group with a description of the issue, steps to reproduce, and the snifrig version.

## What snifrig reads

- Kernel tables through `NtQuerySystemInformation`: pool tags, commit charge, RAM, paged and non-paged pool, and process and token allocation counters.
- Per-process handle and private memory counts.
- Free disk space.
- Windows per-process GPU counters, only when the tray flyout opens.
- Its own files in the data folder, including the fixer license key file `snifrig-fix.key`, which the fixer reads for its offline key check.

## What snifrig writes

- Data folder `%LOCALAPPDATA%\snifrig`: `snifrig.jsonl` (history), `alerts.jsonl`, `status.json`, `snapshot.json`, and `webhook.txt` if a webhook is set. Both JSONL logs are size-capped.
- Fixer records in the same folder: `fixes.jsonl`, `pending.json` and `fixer-state.json`.
- Local license acceptance and usage records, kept in the data folder.
- Login startup: `snifrig install` writes two values under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` (`Snifrig` and `SnifrigTray`). `snifrig uninstall` removes them.

snifrig runs without admin rights. It does not install a Windows service or a driver.

## Network

snifrig makes no network connection unless you set a webhook with `snifrig webhook URL`. When a webhook is set, alerts are sent as JSON to that URL. `snifrig webhook off` removes it. Usage data is never sent anywhere.

The Notice Screen makes no network calls.

The fixer key check is offline. `snifrig-fix` checks the key's signature and dates on your computer and makes no network call.

## Snapshot

The CPU, RAM, GPU and VRAM snapshot runs only when the tray flyout opens. Nothing is sampled while the flyout is closed.
