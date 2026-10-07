# Changelog

All notable changes to snifrig are listed here.

## License change

- Starting with v0.2.0, snifrig moved from UFL 3.4 Noncommercial to UFL 3.7.
- The monitor (everything outside `fixer/`) is licensed under UFL 3.7, Operational Scope: Unconditional. It is free for everyone, companies included.
- The fixer `snifrig-fix` is a paid Component (`LicenseRef-UFL-3.7-U.P-snifrig-fix`). It needs a key in every mode. See `PRICING.md`.
- Earlier releases and commits keep the license they were published under (UFL 3.4 Noncommercial).
- UFL 1C: projects move forward only.

## 0.2.0 — 2026-10-07

- Windows slowdown evidence: `snifrig slowdown` summarizes evidence of Windows slowing this PC over the last 24 hours.
- Notice Screen (UFL Section 2D): the tray shows one short Notice at startup (max 8 s, once per login, closes on click or Esc, never takes focus). Interactive command-line reports end with one Notice line. It cannot be turned off, and nothing is sent anywhere.
- Fixer (`snifrig-fix`, paid Component): reads the monitor's alerts and acts on the process behind them. Modes off, dry-run, ask and auto. Every mode needs a valid key; there is no trial and no free dry-run. See `fixer/README.md`.
- Fixer keys: offline Ed25519-signed files, valid for 30 days, tied to the paid period, optionally bound to one machine (`snifrig-fix machine-id`). Install with `snifrig-fix license install PATH`, accept with `snifrig-fix license accept`.
- Tray approvals: pending fixes appear in the right-click menu with Approve and Dismiss, plus a Fixer mode submenu.
- Install wiring: `snifrig install` also installs and starts `snifrig-fix.exe` when it is built next to the monitor, with its own login entry (`SnifrigFix`).
- Flyout: VRAM is shown as a bar, and the flyout scales for display DPI.

## 0.1.0

- Monitor: reads kernel tables directly (`NtQuerySystemInformation`). Tracks kernel pool by tag and growth per hour, handle counts and private memory per process (top 25), commit charge, free RAM, paged and non-paged pool, process and token creation rate, and free disk space.
- Alert rules: alerts say what is growing, how fast, and what usually causes it. Growth alerts need at least 5 minutes of history.
- Tray icon: green (ok), red (alert), gray (monitor not running). Right-click menu for the report, the alerts, the data folder, pause/resume, and quit.
- Login install: `snifrig install` and `snifrig uninstall` add and remove per-user login startup (HKCU Run entries). No admin rights, no service.
- Spawner attribution: when the process spawn rate spikes, a burst watch of about 5 seconds names the parent processes creating them.
- Pause and resume: `snifrig pause [MIN]` (default 60 minutes) and `snifrig resume` stop fixing while monitoring continues. Also in the tray menu.
- Webhook: `snifrig webhook URL|off` posts alerts as JSON to a URL you set. Opt-in.
- Restart watchdog: the tray restarts the hidden monitor if it died unexpectedly. Maximum 3 restarts per hour, and never after you stopped it.
- Tray flyout: left-click shows CPU, RAM, GPU and VRAM with the top three consumers of each. Collected only while the flyout opens, using Windows per-process GPU counters.
- Footprint self-check: the monitor exits if it breaks its own budget (working set over 48 MB, handle or thread growth, private memory growth over 8 MB, or CPU over 1%).
- License: UFL 3.4, Operational Scope Noncommercial. The first run asks for acceptance. `snifrig license accept` and `snifrig license statement` are available.
- Windows CI: GitHub Actions workflow builds the project on Windows.
