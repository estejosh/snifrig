# snifrig-fix

The paid Component of snifrig. `snifrig-fix` reads the monitor's alerts (`alerts.jsonl`) and, when it is safe, acts on the process behind a leak. The monitor (`snifrig`) is free for everyone.

It can trim a process's memory, lower its priority, terminate it, or restart the service it belongs to.

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
| `snifrig-fix.key` | License key, written by `license install`. |
| `mode.json` | Pause end time, written by `snifrig pause`. |
| `stop-fix` | Create this file to stop the watch loop. The loop deletes it and exits. |

## Commands

```
snifrig-fix                       watch loop, checks every 30 s
snifrig-fix once                  process new alerts once and exit
snifrig-fix status                mode, pause, license, pending count
snifrig-fix mode off|dry-run|ask|auto
snifrig-fix pending               list fixes waiting for approval
snifrig-fix approve ID            run a pending fix
snifrig-fix dismiss ID            drop a pending fix
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
