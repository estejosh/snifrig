---
name: snifrig
description: Diagnose why a Windows PC is slow, laggy or running out of memory using snifrig's tools (snifrig_status, snifrig_why_slow, snifrig_top_processes, snifrig_recent_activity, snifrig_suggest). Use when the user says the PC, a game or an app is slow, stuttering, freezing, or asks what is eating CPU, RAM or GPU memory.
---

# Diagnosing a slow PC with snifrig

snifrig measures this PC continuously. You can read what it sees and suggest changes. You cannot change anything: the user approves every suggestion in the snifrig window. There is no tool to approve pending fixes or edit rules, so do not look for one.

## Steps

1. `snifrig_status`: headline, likely causes, live CPU and RAM, what the governor is already easing. Note whether the fixer is running and licensed.
2. `snifrig_why_slow`: snifrig's own diagnosis. Trust its evidence (paging, commit, pool leaks) over guesses.
3. `snifrig_top_processes`: live view. Try `sort: "memory"` too when paging or commit is the problem. Parent pids show which app spawned a swarm of children.
4. `snifrig_recent_activity` (and `snifrig_alerts`): what the fixer already did and what snifrig flagged lately. Do not suggest what is already in place. `snifrig_rules` shows current rules and settings, `snifrig_pending` shows fixes already waiting.
5. Use `snifrig_report` for "it has been slow all week" questions.

## Giving advice

- Explain in plain words: what is slow, why, what it costs, and the least disruptive fix.
- Never suggest killing or closing an app the user is using. If memory is the issue, say which app holds it and let the user decide (for example "WSL holds 22 GB; `wsl --shutdown` gives it back when you are done").
- Prefer rein-in rules over stopping things: `efficiency=on`, `background=yes`, `priority=below_normal`, `cap=<percent>`, `io=low`, `memory=low`. Avoid `priority=idle` or tight caps on anything interactive.
- Use `snifrig_suggest` with a plain `reason` the user will understand, for example:
  - kind `rule`, rule `comet.exe background=yes efficiency=on`, reason "comet.exe used a full core while you were in other windows 12 times today".
  - kind `ease` with `app` to let the governor ease a repeat offender when the PC is busy.
  - kind `setting` such as `busy_pct=80` only if the governor is acting too eagerly or too late.
  - kind `note` for advice that is not a rule.
- If `snifrig_suggest` says snifrig fix is needed, give the advice in your reply instead and mention that suggestions need snifrig fix.
- After suggesting, tell the user to look for it in the snifrig window and Accept or Dismiss it. Do not claim anything was changed.
- One or two well-reasoned suggestions beat a long list.
