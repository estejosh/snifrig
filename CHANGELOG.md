# Changelog

All notable changes to snifrig are listed here.

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
