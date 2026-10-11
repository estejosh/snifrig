# snifrig-fix

The paid Component of snifrig. `snifrig-fix` reads the monitor's alerts (`alerts.jsonl`) and, when it is safe, acts on the process behind a leak. The monitor (`snifrig`) is free for everyone.

It can trim a process's memory, lower its priority, terminate it, or restart the service it belongs to. It also runs a governor that keeps the PC responsive when the CPU is pegged, applies your per-app rules, switches to a fast power plan while you play, and learns which fixes help on your machine.

It never runs without a valid key. There is no trial and no free dry-run. See [PRICING.md](../PRICING.md) for the price and how to buy.

## Modes

| Mode | What it does | Needs a key |
|---|---|---|
| `off` | Ignores alerts. | Yes |
| `dry-run` | Logs what it would do. Changes nothing. | Yes |
| `ask` | Queues each fix in `pending.json`. Approve it in the tray or with `snifrig-fix approve ID`. | Yes |
| `auto` | Runs `trim` and `lower-priority` by itself. `terminate` and `restart-service` run by themselves only for entries in `fix-allow.txt`. Other fixes are queued. | Yes |

Auto mode runs at most 3 automatic actions per hour. Past that, fixes are queued for approval.

Set the mode with `snifrig-fix mode M`, or from the tray's Fixer mode menu.

## Governor

Every second the governor measures CPU per process. When the PC is busy (total CPU at or over 75% for 3 seconds), it eases the biggest background users, two per second at most, one step at a time:

| Level | What changes |
|---|---|
| 1 | Efficiency mode (EcoQoS) and low I/O priority |
| 2 | Below-normal priority |
| 3 | Idle priority and low memory priority |

It never touches the app you are using (or other copies of it), the never-touch list, anything a pinning rule covers, or an app you undid by hand. When the PC is calm (under 50% for 10 seconds), it puts apps back one step every 5 seconds. An eased app that comes to the front is put back at once. Every change is logged in plain words in `fixes.jsonl` and shown in the tray's Governor menu, where each one can be undone.

All the numbers can be changed in `fix-governor.txt` (created with every line commented out on first run, re-read within 30 seconds of a save):

| Setting | Default | Meaning |
|---|---|---|
| `busy_pct` / `busy_secs` | 75 / 3 | total CPU that counts as busy, and for how long |
| `calm_pct` / `calm_secs` | 50 / 10 | total CPU that counts as calm, and for how long |
| `restore_every_secs` | 5 | one step back this often once calm |
| `min_core_pct` | 25 | only ease apps using at least this share of one core |
| `max_per_tick` | 2 | apps eased per second at most |
| `max_level` | 3 | deepest level for any app |
| `game_mode` | on | performance mode (below) |
| `game_plan` | auto | power plan in performance mode: `auto` (Ultimate, else High performance), `none`, or a plan name or GUID |
| `game_busy_pct` | 50 | busy threshold while a game runs |
| `reapply_secs` | 30 | how often rules are re-checked (0 = never) |

## Rules

`fix-rules.txt` holds per-app rules, one per line. Add them with `snifrig-fix rule add "PATTERN key=value ..."` or edit the file; it is re-read within 30 seconds.

```
# pattern (image name, * and ? wildcards)   settings
ffmpeg*.exe          priority=below_normal io=low cap=60
handbrake*.exe       background=yes max_instances=1
obs64.exe            priority=normal efficiency=off affinity=0-7
legendsofidleon.exe  background=yes affinity=0x8000
build*.exe           max_level=1
steam.exe            governor=exempt
mygame.exe           game=yes
```

| Key | Values |
|---|---|
| `priority` | `idle`, `below_normal`, `normal` (never higher) |
| `efficiency` | `on`, `off` |
| `io`, `memory` | `very_low`, `low`, `normal` |
| `affinity` | hex mask (`0xff`) or core list (`0-7`, `0,2,4-5`, `15`) |
| `cap` | 1-100, a hard CPU cap in percent (job object) |
| `max_instances` | n: extra, newest copies are reported and queued for your approval, never killed by themselves |
| `background` | `yes`: efficiency on, I/O low, below normal |
| `governor` | `exempt`: the governor never touches it. `on`: the governor manages it like any other app |
| `max_level` | 0-3: the governor manages it, but no deeper than this (0 = exempt) |
| `game` | `yes` / `no`: force performance mode on or off for this app |

A rule that changes anything also pins the app: the governor leaves it alone unless the rule says `governor=on` or `max_level=`. Priority and affinity are re-checked every 30 seconds and put back if the app resets them.

## Performance mode

When a game is the app in front for 3 seconds, the fixer switches to a fast power plan, treats 50% CPU as busy so background apps are eased sooner, and never eases the game, even when you alt-tab. When the game exits, the plan you had goes back, unless you changed it meanwhile. The replaced plan is saved to `perf-plan.txt` first, so a crash or reboot cannot leave it changed.

An app counts as a game when a rule says `game=yes`, or when the local brain classified it as a game, or when it runs Direct3D full screen. A rule that deliberately slows an app (for example `background=yes`) or says `game=no` keeps it out. Turn the feature off with `game_mode = off`.

## Learning

- **Outcomes.** Each fix and each first easing is scored 3 minutes later: did CPU, memory or the app in front get better? Names where easing rarely helps drop down the list, and names that never help are left alone.
- **Your clicks.** Undo, approve and dismiss are recorded in `learn-feedback.jsonl`. An app you keep undoing is protected from then on.
- **Your week.** `baseline.json` keeps what is normal for each hour of the week. When a busy spell is unusual for that hour, the ledger says so and names the apps that are not usually busy then.
- **The local brain (optional).** If an OpenJev server runs on this PC (`http://127.0.0.1:8791`, or the URL in `jev.txt`), the fixer asks it what kind of program each busy app is: game, interactive, batch job, AI model, sync or backup, system, or background. Games and apps you are using are eased later and less. Batch jobs are eased first. Local AI model servers are never pushed to idle. It asks at most once every 20 seconds, caches answers for 7 days in `brain-cache.json`, ignores low-confidence answers, and only ever talks to this machine. `snifrig-fix brain NAME` asks it about one program.

## Safety rules

- Processes younger than 2 minutes are ignored. A short burst is not a leak.
- The fixer never acts on these image names (the never-touch list):

```
system, registry, memory compression, secure system, idle,
smss.exe, csrss.exe, wininit.exe, services.exe, lsass.exe, lsaiso.exe,
winlogon.exe, dwm.exe, explorer.exe, fontdrvhost.exe, sihost.exe,
ctfmon.exe, audiodg.exe, spoolsv.exe, taskhostw.exe, conhost.exe,
msmpeng.exe, nissrv.exe, securityhealthservice.exe, mpdefendercoreservice.exe,
vmmem, vmmemwsl, vmcompute.exe, wslservice.exe, wsl.exe, wslhost.exe
```

- Also never: any image name starting with `snifrig`, any command line containing `gpu_arbiter` or `ferryman`, pid 0, pid 4, and the fixer itself. Entries in `fix-deny.txt` are added to this list.
- `svchost.exe` is never terminated, because one host runs many services. If a single service is the cause, the fixer restarts that service instead.
- Before acting, the fixer re-checks the process name and identity, so a reused pid is not hit.
- `snifrig pause [MIN]` stops all fixing for MIN minutes (default 60). Monitoring continues. `snifrig resume` ends the pause. Approvals respect the pause too.
- Every decision is logged to `fixes.jsonl`, including refusals and dry-run entries.

## Files

All in the data folder, `%LOCALAPPDATA%\snifrig`, unless `--dir` is given.

| File | Purpose |
|---|---|
| `alerts.jsonl` | Written by the monitor. Read by the fixer. |
| `fixes.jsonl` | Log of every decision. Rotates to `fixes.jsonl.old` at 512 KB. |
| `pending.json` | Fixes waiting for approval. Keeps the newest 20. |
| `fixer-state.json` | How far into `alerts.jsonl` the fixer has read. |
| `fix-mode.txt` | One word: `off`, `dry-run`, `ask` or `auto`. |
| `fix-allow.txt` | Process names that auto mode may terminate or restart. One per line, `#` for comments. |
| `fix-deny.txt` | Extra never-touch entries. One per line. |
| `fix-rules.txt` | Per-app rules (see Rules). |
| `fix-governor.txt` | Governor and performance mode settings. |
| `governor.json` | What the governor holds back right now, and the game in performance mode. Read by the tray. |
| `perf-plan.txt` | The power plan to put back after performance mode. Exists only while it is on. |
| `learn.jsonl`, `learn-stats.json`, `learn-feedback.jsonl` | Fix outcomes and your feedback. |
| `baseline.json` | What is normal for each hour of the week. |
| `brain-cache.json`, `brain-error.txt`, `jev.txt` | Local brain answers, last error, and optional URL. |
| `snifrig-fix.key` | License key, written by `license install`. |
| `mode.json` | Pause end time, written by `snifrig pause`. |
| `stop-fix` | Create this file to stop the watch loop. The loop deletes it and exits. |

## Commands

```
snifrig-fix                       watch loop: governor every second, alerts every 30 s
snifrig-fix once                  process new alerts once and exit
snifrig-fix status                mode, pause, license, pending count
snifrig-fix mode off|dry-run|ask|auto
snifrig-fix pending               list fixes waiting for approval
snifrig-fix approve ID            run a pending fix
snifrig-fix dismiss ID            drop a pending fix
snifrig-fix governor status       what the governor holds back now
snifrig-fix governor undo PID     put an app back and leave it alone
snifrig-fix rule add "PATTERN key=value ..."
snifrig-fix rule list             numbered rules, with errors
snifrig-fix rule remove N
snifrig-fix brain NAME            ask the local brain what NAME is
snifrig-fix machine-id            print this machine's hash (for a machine-bound key)
snifrig-fix license install PATH  install a license key file
snifrig-fix license accept        accept the Component license (UFL 3.7)
snifrig-fix license status        show license state
```

Option: `--dir PATH`.

Approve ignores the mode and the hourly limit. It does not ignore the never-touch list, the 2-minute age rule, the pause, or the license.

## License

`snifrig-fix` is a paid Component under UFL 3.7 (`LicenseRef-UFL-3.7-U.P-snifrig-fix`). Every mode needs a valid key, including `off` and `dry-run`. Keys are offline Ed25519-signed files in `snifrig-fix.key`. They last 30 days, are tied to the paid period, and are re-issued during it. A key may be bound to one machine (`snifrig-fix machine-id` prints the hash). A clock set backwards is refused.

To get a key, install it, and accept the Component:

1. Buy a license and receive your key by email. See [PRICING.md](../PRICING.md).
2. `snifrig-fix license install PATH`
3. `snifrig-fix license accept`
4. `snifrig-fix license status` to confirm.

The key check is made entirely on your computer and makes no network connection. This keeps honest users honest. It is not uncrackable.

The monitor (everything outside `fixer/`) is licensed under UFL 3.7, Operational Scope: Unconditional, and is free. See [PRICING.md](../PRICING.md) and [LICENSE](../LICENSE).
