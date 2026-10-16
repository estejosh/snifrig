//! snifrig-fix: reads the monitor's alerts and fixes what it safely can.
//!   snifrig-fix [--dir D]                 watch loop (every 30 s); stops when D\stop-fix exists
//!   snifrig-fix once                      process new alerts once and exit
//!   snifrig-fix mode off|dry-run|ask|auto set the mode (default dry-run; every mode needs a key)
//!   snifrig-fix status                    mode, pause, license, pending count
//!   snifrig-fix pending                   list fixes waiting for approval
//!   snifrig-fix approve ID | dismiss ID   act on a pending fix
//!   snifrig-fix governor status | governor undo PID   what the governor demoted; put one process back
//!   snifrig-fix rule add "PATTERN key=value ..." | rule list | rule remove N   per-app rules (see rules.rs)
//!   snifrig-fix license install PATH      install a license key
//!   snifrig-fix license accept            accept UFL-3.7 for this component (or --accept-license "UFL-3.7 snifrig-fix")
//!   snifrig-fix license status | machine-id
//! PAID component: every command except license install/status/accept, machine-id and help needs a valid
//! key and a recorded acceptance, otherwise it prints why on stderr and exits with code 2. No trial.

use snifrig_fix::{actions, baseline::Baseline, ledger, learn::{self, Feedback, Learner, Metrics, Trigger}, license, now_unix, plan, policy::Policy, proc, rules, sys, Action, Mode, Subject, Target, Verdict};
use std::path::{Path, PathBuf};

fn default_dir() -> PathBuf {
    std::env::var("LOCALAPPDATA").map(|p| PathBuf::from(p).join("snifrig")).unwrap_or_else(|_| PathBuf::from("data"))
}

fn policy(dir: &Path) -> Policy {
    Policy {
        mode: ledger::read_mode(dir),
        now: now_unix(),
        paused_until: ledger::paused_until(dir),
        licensed: true, // the hard gate in main() already required a valid key
        allow: ledger::read_list(dir, "fix-allow.txt"),
        extra_deny: ledger::read_list(dir, "fix-deny.txt"),
        recent_auto: ledger::recent_auto(dir),
    }
}

/// Turn a planned subject into live targets. A named pid must still carry the same name.
fn resolve(s: &Subject) -> Vec<Target> {
    match s {
        Subject::None => Vec::new(),
        Subject::Pid { name, pid } => proc::target(*pid).filter(|t| t.name.eq_ignore_ascii_case(name)).into_iter().collect(),
        Subject::TopPrivate(n) => proc::top_private(*n),
    }
}

/// A process that is a single-service host is restarted through the service manager, not killed.
fn refine(action: &Action, t: &Target) -> Action {
    match (action, &t.service) {
        (Action::Terminate, Some(svc)) => Action::RestartService(svc.clone()),
        _ => action.clone(),
    }
}

/// "Oct 12 14:05" in local time.
fn local_date(unix: f64) -> String {
    let t = unix as i64 + sys::local_offset_secs();
    let (days, secs) = (t.div_euclid(86400), t.rem_euclid(86400));
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    const M: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    format!("{} {} {:02}:{:02}", M[(mo - 1) as usize], d, secs / 3600, secs % 3600 / 60)
}

/// The name learning is keyed by: what a script host runs ("python script build.py"), else the image name.
fn learn_name(t: &Target) -> String {
    if snifrig_fix::inuse::is_host(&t.name) { snifrig_fix::judge::identity(&t.name, &t.cmdline).to_lowercase() } else { t.name.to_lowercase() }
}

fn ago(secs: f64) -> String {
    let s = secs.max(0.0);
    if s < 7200.0 { format!("{:.0} minutes", (s / 60.0).max(1.0)) } else if s < 172800.0 { format!("{:.0} hours", s / 3600.0) } else { format!("{:.0} days", s / 86400.0) }
}

fn process_once(dir: &Path) -> usize { process_once_ex(dir, None).0 }

/// Like process_once, and also returns what ran: (action kind, lowercase name, metric it was meant to help).
/// With `ctx` (the watch loop's learner and governor, and whether it acts), fixes the user has
/// taught snifrig to trust run without asking; a lower priority is handed to the governor so the
/// app is restored the moment you switch to it.
fn process_once_ex(dir: &Path, mut ctx: Option<(&Learner, &mut snifrig_fix::governor::Governor, bool)>) -> (usize, Vec<(String, String, Trigger)>) {
    let (alerts, off) = ledger::new_alerts(dir, ledger::read_offset(dir));
    let (listen, mem, now) = if alerts.is_empty() { (Default::default(), snifrig_fix::judge::Memory::load(Path::new("")), now_unix()) } else { (snifrig_fix::judge::listening_pids(), snifrig_fix::judge::Memory::load(dir), now_unix()) };
    let mut acted = 0;
    let mut ran = Vec::new();
    for a in &alerts {
        let it = plan::plan(a);
        let targets = resolve(&it.subject);
        if targets.is_empty() {
            let why = if it.subject == Subject::None { it.why.clone() } else { "process gone, pid reused, or not accessible to this user".into() };
            let v = policy(dir).decide(&it.action, None);
            let status = if it.action == Action::Report { "would" } else { "refused" };
            ledger::record(dir, a, &it.action, None, status, &match v { Verdict::LogOnly(r) | Verdict::Refuse(r) => format!("{}: {}", r, why), _ => why });
            continue;
        }
        for t in &targets {
            let mut action = refine(&it.action, t);
            let mut why = it.why.clone();
            let ident = snifrig_fix::judge::identity(&t.name, &t.cmdline);
            let server = listen.get(&t.pid).copied();
            let in_use = match server { Some(port) => Some(format!("it is a server other programs use, port {}", port)), None => snifrig_fix::inuse::why_in_use(&t.name, now).or_else(|| {
                // A script host is as important as the app that started it (your terminal, Claude, an IDE).
                if snifrig_fix::inuse::is_host(&t.name) { snifrig_fix::inuse::started_by(t.pid, now).map(|p| format!("{} started it and you are using {}", p, p)) } else { None }
            }) };
            let memory_alert = a.key.starts_with("pm:") || a.key.starts_with("ph:") || a.key.starts_with("x:");
            // A server other programs use (ComfyUI, a model server): its memory grows as it caches and
            // its CPU use is its work. Never ask about it; the governor eases it only if the PC feels slow.
            if let Some(port) = server {
                ledger::record(dir, a, &Action::Report, Some(t), "would", &format!("left alone: {} is a server other programs use (port {}); snifrig keeps watching and the governor eases it only if the PC feels slow", ident, port));
                continue;
            }
            // Never end or restart something the user is using: rein it in instead.
            if matches!(action, Action::Terminate | Action::RestartService(_)) {
                if let Some(r) = &in_use {
                    action = if memory_alert { Action::Trim } else { Action::LowerPriority };
                    why = format!("you are using it ({}), so snifrig reins it in instead of closing it; {}", r, it.why);
                }
            }
            let label = action.label();
            if let Some(when) = mem.declined(&t.name, &label, now) {
                ledger::record(dir, a, &action, Some(t), "refused", &format!("not asking again: you said no to this {} ago", ago(now - when)));
                continue;
            }
            if !ident.eq_ignore_ascii_case(&t.name) { why = format!("{}: {}", ident, why); }
            // Freeing memory of an app in use is harmless: do it (at most every 6 h) instead of asking.
            if action == Action::Trim && in_use.is_some() {
                let p = policy(dir);
                if matches!(p.mode, Mode::Ask | Mode::Auto) && now >= p.paused_until && snifrig_fix::policy::denied(t, &action, &p.extra_deny).is_none() {
                    if !mem.done_within(&t.name, &label, now, 6.0 * 3600.0) {
                        let (status, note) = match actions::run(&action, t) { Ok(m) => ("done", format!("freed memory without asking (harmless, you are using it): {}; {}", m, why)), Err(e) => ("failed", e) };
                        ledger::record(dir, a, &action, Some(t), status, &note);
                    }
                    continue;
                }
            }
            if mem.asked_recently(&t.name, &label, now) { continue; }
            // Fresh policy per target so the auto rate limit sees actions taken earlier in this pass.
            let p = policy(dir);
            match p.decide(&action, Some(t)) {
                Verdict::Execute => {
                    let (status, note) = match actions::run(&action, t) {
                        Ok(m) => ("done", format!("auto: {}; {}", m, why)),
                        Err(e) => ("failed", format!("auto: {}", e)),
                    };
                    ledger::record(dir, a, &action, Some(t), status, &note);
                    if status == "done" { ran.push((learn::action_kind(&action.label()), learn_name(t), learn::trigger_for(&action.label(), &a.msg))); }
                    acted += 1;
                }
                Verdict::Queue => {
                    let learned = ctx.as_ref().and_then(|(l, _, _)| l.earned(&label, &learn_name(t)));
                    if let (Some(trust), Some((_, gov, act))) = (learned, ctx.as_mut()) {
                        let r = if action == Action::LowerPriority {
                            match gov.adopt(t.pid, 2, &format!("learned from you: {}", trust), *act) {
                                Some(c) => c.result.map(|_| "eased it: efficiency mode and below-normal priority, back to full speed the moment you switch to it".to_string()),
                                None => continue,
                            }
                        } else { actions::run(&action, t) };
                        let (status, note) = match &r { Ok(m) => ("done", format!("did it without asking because {}: {}; undo it in the app if that was wrong; {}", trust, m, why)), Err(e) => ("failed", format!("learned fix: {}", e)) };
                        ledger::record(dir, a, &action, Some(t), status, &note);
                        if r.is_ok() { ran.push((learn::action_kind(&label), learn_name(t), learn::trigger_for(&label, &a.msg))); acted += 1; }
                        continue;
                    }
                    if let Some(id) = ledger::queue(dir, a, &action, t, &why) {
                        ledger::record(dir, a, &action, Some(t), "queued", &format!("pending id {}; {}", id, why));
                    }
                }
                Verdict::LogOnly(r) => ledger::record(dir, a, &action, Some(t), "would", &format!("{}; {}", r, why)),
                Verdict::Refuse(r) => ledger::record(dir, a, &action, Some(t), "refused", &r),
            }
        }
    }
    ledger::write_offset(dir, off);
    (acted, ran)
}

/// Record governor changes. `acted` false means dry run: logged as "would".
fn log_gov(dir: &Path, changes: Vec<snifrig_fix::governor::Change>, acted: bool) {
    for c in changes {
        let alert = snifrig_fix::Alert { t: String::new(), key: format!("gov:{}#{}", c.name, c.pid), msg: c.reason.clone() };
        let t = Target { pid: c.pid, name: c.name.clone(), ..Default::default() };
        let action = if c.to > c.from { Action::LowerPriority } else { Action::Report };
        let (status, note) = match (&c.result, acted) {
            (_, false) => ("would", format!("governor level {} -> {}: {}", c.from, c.to, c.reason)),
            (Ok(()), true) => ("done", format!("governor: level {} -> {}: {}", c.from, c.to, c.reason)),
            (Err(e), true) => ("failed", format!("governor level {} -> {}: {}", c.from, c.to, e)),
        };
        ledger::record(dir, &alert, &action, Some(&t), status, &note);
    }
}

fn approve(dir: &Path, id: &str) -> Result<String, String> {
    let mut items = ledger::load_pending(dir);
    let i = items.iter().position(|x| x.id == id).ok_or("no pending fix with that id")?;
    let item = items.remove(i);
    ledger::save_pending(dir, &items);
    let alert = snifrig_fix::Alert { t: String::new(), key: item.key.clone(), msg: item.msg.clone() };
    let action = Action::parse(&item.action).ok_or("unknown action in pending.json")?;
    let t = proc::target(item.pid).filter(|t| t.name.eq_ignore_ascii_case(&item.name))
        .ok_or_else(|| format!("{} (pid {}) is no longer running", item.name, item.pid))?;
    policy(dir).approve_check(&action, &t)?;
    let r = actions::run(&action, &t);
    let (status, note) = match &r { Ok(m) => ("done", format!("approved: {}", m)), Err(e) => ("failed", format!("approved: {}", e)) };
    ledger::record(dir, &alert, &action, Some(&t), status, &note);
    if r.is_ok() {
        let mut f = Feedback::new("approve", "fix", &learn::action_kind(&action.label()), &learn_name(&t));
        f.trigger = learn::trigger_for(&action.label(), &item.msg).as_str().into();
        f.start = true;
        learn::append_feedback(dir, &f);
    }
    r
}

/// "Rein it in" instead of ending or restarting: efficiency mode, below-normal priority, memory trimmed.
fn ease(dir: &Path, id: &str) -> Result<String, String> {
    if now_unix() < ledger::paused_until(dir) { return Err("fixing is paused".into()); }
    let mut items = ledger::load_pending(dir);
    let i = items.iter().position(|x| x.id == id).ok_or("no pending fix with that id")?;
    let item = items.remove(i);
    ledger::save_pending(dir, &items);
    let alert = snifrig_fix::Alert { t: String::new(), key: item.key.clone(), msg: item.msg.clone() };
    let t = proc::target(item.pid).filter(|t| t.name.eq_ignore_ascii_case(&item.name))
        .ok_or_else(|| format!("{} (pid {}) is no longer running", item.name, item.pid))?;
    if let Some(why) = snifrig_fix::policy::denied(&t, &Action::LowerPriority, &ledger::read_list(dir, "fix-deny.txt")) { return Err(why); }
    let mut done = Vec::new();
    if sys::set_efficiency(t.pid, true).is_ok() { done.push("efficiency mode"); }
    if sys::set_priority(t.pid, sys::BELOW_NORMAL).is_ok() { done.push("below-normal priority"); }
    if actions::run(&Action::Trim, &t).is_ok() { done.push("memory trimmed"); }
    if done.is_empty() { return Err(format!("could not change {}", t.name)); }
    let note = format!("reined in at your request instead of {}: {}", item.action, done.join(", "));
    ledger::record(dir, &alert, &Action::LowerPriority, Some(&t), "done", &note);
    learn::append_feedback(dir, &Feedback::new("dismiss", "fix", &item.action, &item.name));
    Ok(format!("{}: {}", t.name, done.join(", ")))
}

fn dismiss(dir: &Path, id: &str) -> Result<String, String> {
    let mut items = ledger::load_pending(dir);
    let i = items.iter().position(|x| x.id == id).ok_or("no pending fix with that id")?;
    let item = items.remove(i);
    ledger::save_pending(dir, &items);
    let alert = snifrig_fix::Alert { t: String::new(), key: item.key.clone(), msg: item.msg.clone() };
    let action = Action::parse(&item.action).unwrap_or(Action::Report);
    let t = Target { pid: item.pid, name: item.name.clone(), ..Default::default() };
    ledger::record(dir, &alert, &action, Some(&t), "dismissed", "dismissed by user");
    let key = proc::target(item.pid).filter(|x| x.name.eq_ignore_ascii_case(&item.name)).map(|x| learn_name(&x)).unwrap_or_else(|| item.name.to_lowercase());
    learn::append_feedback(dir, &Feedback::new("dismiss", "fix", &learn::action_kind(&action.label()), &key));
    Ok(format!("dismissed {} on {}", item.action, item.name))
}

/// Record what the rule enforcer did. `acting` false means dry run: logged as "would".
fn log_enf(dir: &Path, applied: Vec<rules::Applied>, acting: bool) {
    for a in applied {
        let alert = snifrig_fix::Alert { t: String::new(), key: format!("rule:{}#{}", a.name, a.pid), msg: a.note.clone() };
        let t = Target { pid: a.pid, name: a.name.clone(), ..Default::default() };
        match a.kind {
            "max_instances" if acting => {
                // Terminating stays a human decision: queue it through the usual pending flow.
                if let Some(id) = ledger::queue(dir, &alert, &Action::Terminate, &t, "more copies than the rule allows") {
                    ledger::record(dir, &alert, &Action::Terminate, Some(&t), "queued", &format!("pending id {}; {}", id, a.note));
                }
            }
            "max_instances" => ledger::record(dir, &alert, &Action::Terminate, Some(&t), "would", &format!("would ask before ending it: {}", a.note)),
            "refused" => ledger::record(dir, &alert, &Action::LowerPriority, Some(&t), "refused", &format!("rule not applied: {}", a.note)),
            _ => {
                let (status, note) = match (&a.result, acting) {
                    (_, false) => ("would", format!("rule: {}", a.note)),
                    (Ok(()), true) => ("done", format!("rule: {}", a.note)),
                    (Err(e), true) => ("failed", format!("rule: {}: {}", a.note, e)),
                };
                ledger::record(dir, &alert, &Action::LowerPriority, Some(&t), status, &note);
            }
        }
    }
}

/// Pick up `gov-undo-<pid>` request files written by `governor undo`.
fn handle_undo(dir: &Path, gov: &mut snifrig_fix::governor::Governor, act: bool) {
    // The undo request files are the only signal the loop sees directly; the learner reads it back from the feedback file.
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let fname = e.file_name().to_string_lossy().to_string();
        let Some(pid) = fname.strip_prefix("gov-undo-").and_then(|x| x.parse::<u32>().ok()) else { continue };
        let _ = std::fs::remove_file(e.path());
        let changes = gov.undo(pid, act);
        if changes.is_empty() {
            let alert = snifrig_fix::Alert { t: String::new(), key: format!("gov:undo#{}", pid), msg: String::new() };
            ledger::record(dir, &alert, &Action::Report, None, "refused", &format!("undo asked for pid {} but the governor has not changed it", pid));
        } else {
            for c in &changes { learn::append_feedback(dir, &Feedback::new("undo", "governor", "demote", &c.name)); }
            log_gov(dir, changes, act);
        }
    }
}

/// The governor.json text for the current state. The second value changes only when the
/// state does (not with the CPU number), so the loop can write on change.
fn state_json(gov: &snifrig_fix::governor::Governor, acting: bool, sugg: &[(String, usize)]) -> (String, String) {
    let game = gov.game.as_ref().map_or("null".to_string(), |g| format!("\"{}\"", snifrig_fix::json::esc(g)));
    let mut d: Vec<_> = gov.demoted.values().collect();
    d.sort_by_key(|x| x.pid);
    let demoted: Vec<String> = d.iter().map(|x| format!("{{\"pid\":{},\"name\":\"{}\",\"level\":{},\"reason\":\"{}\",\"since\":{:.0}}}", x.pid, snifrig_fix::json::esc(&x.name), x.level, snifrig_fix::json::esc(&x.reason), x.since)).collect();
    let sg: Vec<String> = sugg.iter().map(|(n, c)| format!("{{\"name\":\"{}\",\"count\":{}}}", snifrig_fix::json::esc(n), c)).collect();
    let f = snifrig_fix::feel::recent(10);
    let feel = format!("{{\"word\":\"{}\",\"ping_ms\":{:.0},\"lag_ms\":{:.0},\"hung\":{},\"desktop_pct\":{:.0},\"input_ms\":{:.0},\"input_events\":{}}}", snifrig_fix::feel::word_with(&f, gov.dwm_pct), f.ping_ms, f.lag_ms, f.hung, gov.dwm_pct, f.input_ms, f.input_events);
    let tail = format!("\"busy\":{},\"game\":{},\"busy_pct\":{:.0},\"demoted\":[{}],\"suggestions\":[{}]}}\n", gov.mood.busy_with(&gov.tuning), game, gov.tuning.busy_pct, demoted.join(","), sg.join(","));
    let full = format!("{{\"unix\":{:.0},\"acting\":{},\"cpu\":{:.0},\"feel\":{},{}", now_unix(), acting, gov.total_pct, feel, tail);
    (full, format!("{}{}", acting, tail))
}

/// The watch loop: alerts every 30 ticks, governor + rules every tick (1 s).
/// The desktop compositor (smoothed share of a core) this busy for this long is stuck, not busy.
const DWM_STUCK_PCT: f64 = 70.0;
const DWM_STUCK_SECS: f64 = 180.0;

/// Known fix: a stuck dwm. Ask once (a card with a Restart button), at most daily; a "no" is
/// remembered for a week, two for good. Restarting the desktop never becomes automatic.
fn offer_desktop_restart(dir: &Path, pct: f64, now: f64) {
    let mem = snifrig_fix::judge::Memory::load(dir);
    let label = Action::RestartService("dwm".into()).label();
    if mem.declined("dwm.exe", &label, now).is_some() || mem.asked_recently("dwm.exe", &label, now) { return; }
    let path = dir.join("ai-suggestions.json");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    if text.contains("\"restart_desktop\"") { return; }
    let reason = format!("The desktop compositor (dwm) has used {:.0}% of a core for over 3 minutes. That is what makes the mouse and every window stutter. Restarting it takes a second: the screen flashes, your apps stay open. Windows asks you to confirm.", pct);
    let item = format!("{{\"id\":\"desktop-{:.0}\",\"unix\":{:.0},\"kind\":\"restart_desktop\",\"app\":\"dwm.exe\",\"rule\":\"\",\"setting\":\"\",\"reason\":\"{}\",\"source\":\"snifrig\"}}", now, now, snifrig_fix::json::esc(&reason));
    let body = match text.find("\"items\":[") {
        Some(i) => { let at = i + 9; let rest = &text[at..]; format!("{}{}{}{}", &text[..at], item, if rest.trim_start().starts_with(']') { "" } else { "," }, rest) }
        None => format!("{{\"items\":[{}]}}\n", item),
    };
    rules::write_atomic(&path, &body);
    let alert = snifrig_fix::Alert { t: String::new(), key: "desktop:stuck".into(), msg: reason.clone() };
    let t = Target { pid: 0, name: "dwm.exe".into(), ..Default::default() };
    ledger::record(dir, &alert, &Action::RestartService("dwm".into()), Some(&t), "queued", &reason);
}

/// `desktop restart` (after your OK) or `desktop no`.
fn desktop(dir: &Path, yes: bool) -> Result<String, String> {
    let alert = snifrig_fix::Alert { t: String::new(), key: "desktop:stuck".into(), msg: String::new() };
    let t = Target { pid: 0, name: "dwm.exe".into(), ..Default::default() };
    let action = Action::RestartService("dwm".into());
    if !yes {
        ledger::record(dir, &alert, &action, Some(&t), "dismissed", "you said not to restart the desktop");
        learn::append_feedback(dir, &Feedback::new("dismiss", "fix", "restart-service", "dwm.exe"));
        return Ok("ok, leaving the desktop alone".into());
    }
    // dwm belongs to the system: ending it needs admin, so Windows shows its own confirmation.
    use std::os::windows::process::CommandExt;
    let st = std::process::Command::new("powershell.exe").args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", "Start-Process taskkill.exe -ArgumentList '/f','/im','dwm.exe' -Verb RunAs -WindowStyle Hidden -Wait"])
        .creation_flags(0x0800_0000).status().map_err(|e| e.to_string())?;
    if !st.success() { ledger::record(dir, &alert, &action, Some(&t), "refused", "the Windows confirmation was cancelled"); return Err("cancelled".into()); }
    ledger::record(dir, &alert, &action, Some(&t), "done", "restarted the desktop compositor with your OK; Windows started it again");
    let mut f = Feedback::new("approve", "fix", "restart-service", "dwm.exe");
    f.start = true;
    learn::append_feedback(dir, &f);
    Ok("desktop restarted".into())
}

/// learned.json for the tray and the app: what snifrig now does by itself, one item per line.
fn write_learned(dir: &Path, l: &Learner, h: &snifrig_fix::governor::Hints) {
    use snifrig_fix::json::esc;
    let mut lines = vec![format!("{{\"unix\":{:.0}}}", now_unix())];
    for (a, n, why) in l.learned_fixes() {
        let what = if a == "trim" { format!("Frees memory of {} by itself", n) } else { format!("Eases {} by itself", n) };
        lines.push(format!("{{\"text\":\"{}\",\"why\":\"{}\",\"name\":\"{}\",\"action\":\"{}\"}}", esc(&what), esc(&why), esc(&n), esc(&a)));
    }
    let present = rules::rule_lines(dir);
    for n in l.learned_rules() {
        if !present.iter().any(|r| r.trim().eq_ignore_ascii_case(&format!("{} background=yes", n))) { continue; }
        lines.push(format!("{{\"text\":\"Always runs {} in the background\",\"why\":\"eased 20+ times a week and you never undid it\",\"name\":\"{}\",\"action\":\"rule\"}}", esc(&n), esc(&n)));
    }
    let mut p: Vec<&String> = h.protect.iter().collect(); p.sort();
    for n in p { lines.push(format!("{{\"text\":\"Leaves {} alone\",\"why\":\"you undid it twice\",\"name\":\"{}\",\"action\":\"\"}}", esc(n), esc(n))); }
    let mut b: Vec<&String> = h.blocked.iter().collect(); b.sort();
    for n in b { lines.push(format!("{{\"text\":\"Stopped easing {}\",\"why\":\"it did not help\",\"name\":\"{}\",\"action\":\"\"}}", esc(n), esc(n))); }
    let body = lines.join("\n") + "\n";
    if std::fs::read_to_string(dir.join("learned.json")).map_or(true, |old| old.lines().skip(1).ne(body.lines().skip(1))) { rules::write_atomic(&dir.join("learned.json"), &body); }
}

/// `feel slow|fine`: you tell snifrig how the PC feels right now (tray buttons).
fn feel_cmd(dir: &Path, slow: bool) -> Result<String, String> {
    let g = std::fs::read_to_string(dir.join("governor.json")).unwrap_or_default();
    let meter = snifrig_fix::json::str_field(&g, "word").unwrap_or_else(|| "unknown".into());
    learn::append_feedback(dir, &Feedback::new(if slow { "feel_slow" } else { "feel_fine" }, "feel", "", ""));
    let msg = if slow { format!("you said the PC feels slow (the meter said {}): easing background apps now", meter) } else { format!("you said the PC feels fine (the meter said {}): holding off for 10 minutes", meter) };
    let alert = snifrig_fix::Alert { t: String::new(), key: "feel:you".into(), msg: msg.clone() };
    ledger::record(dir, &alert, &Action::Report, None, "report", &msg);
    Ok(msg)
}

/// `unlearn NAME ACTION`: stop doing a learned fix, or remove a learned rule. Counts as an undo.
fn unlearn(dir: &Path, name: &str, action: &str) -> Result<String, String> {
    let n = name.to_lowercase();
    if action == "rule" {
        let lines = rules::rule_lines(dir);
        if let Some(i) = lines.iter().position(|r| r.trim().eq_ignore_ascii_case(&format!("{} background=yes", n))) { rules::remove(dir, i + 1)?; }
    }
    learn::append_feedback(dir, &Feedback::new("undo", "fix", if action == "rule" { "demote" } else { action }, &n));
    let msg = format!("stopped: snifrig will ask again before doing that to {}", n);
    let alert = snifrig_fix::Alert { t: String::new(), key: "learn:unlearn".into(), msg: msg.clone() };
    ledger::record(dir, &alert, &Action::Report, None, "report", &msg);
    Ok(msg)
}

/// Times a background program is eased in a week before snifrig makes it a rule by itself.
const AUTO_RULE_AFTER: usize = 20;

fn watch(dir: &Path) {
    use snifrig_fix::governor::Governor;
    let extra = ledger::read_list(dir, "fix-deny.txt");
    let mut rule_set = rules::load(dir);
    let mut rules_mt = rules::mtime(dir);
    let mut gov = Governor::new(extra.clone(), rules::pinned_names(dir));
    gov.set_rules(rule_set.clone());
    let mut enf = rules::Enforcer::new(extra);
    let mut sugg = rules::suggestions(dir);
    let stop = dir.join("stop-fix");
    let (mut n, mut last_key, mut last_write) = (0u64, String::new(), 0.0f64);
    let mut learner = Learner::load(dir);
    let mut base = Baseline::load(dir);
    let brain = snifrig_fix::brain::Brain::start(dir);
    snifrig_fix::feel::start();
    let mut incidents = snifrig_fix::incident::Tracker::new();
    let note = |dir: &Path, key: &str, status: &str, msg: &str| {
        let alert = snifrig_fix::Alert { t: String::new(), key: key.into(), msg: msg.into() };
        ledger::record(dir, &alert, &Action::Report, None, status, msg);
    };
    use snifrig_fix::governor::Tuning;
    let tuning_mt = |dir: &Path| std::fs::metadata(dir.join(snifrig_fix::governor::TUNING_FILE)).and_then(|m| m.modified()).ok();
    let tf = dir.join(snifrig_fix::governor::TUNING_FILE);
    if !tf.exists() { let _ = std::fs::write(&tf, snifrig_fix::governor::TUNING_TEMPLATE); }
    let (t0, errs) = Tuning::load(dir);
    for e in errs { note(dir, "gov:settings", "refused", &format!("{}: {}", snifrig_fix::governor::TUNING_FILE, e)); }
    gov.tuning = t0.clone();
    enf.reapply_secs = t0.reapply_secs;
    let mut tune_mt = tuning_mt(dir);
    let mut dwm_hot = 0.0f64;
    // The feel gate was turned off while the meter was checked; it comes back on by itself after a day.
    if !t0.feel_gate && !dir.join("feel-gate-on-at").exists() && !dir.join("feel-gate-stays-off").exists() {
        let _ = std::fs::write(dir.join("feel-gate-on-at"), format!("{:.0}", now_unix() + 86400.0));
        let _ = std::fs::write(dir.join("feel-gate-stays-off"), "delete feel-gate-on-at to keep the feel gate off\n");
    }
    let (mut perf, left) = snifrig_fix::perfmode::PerfMode::new(dir);
    if let Some(m) = left { note(dir, "perf:mode", "done", &m); }
    let mut last_unusual = 0.0f64;
    base.set_tz(sys::local_offset_secs());
    let metrics = |gov: &snifrig_fix::governor::Governor| {
        let (commit_pct, avail_mb) = sys::memory_status().unwrap_or((0.0, 0.0));
        let f = snifrig_fix::feel::recent(30);
        Metrics { total_cpu_pct: gov.total_pct, commit_pct, avail_mb, fg_cpu_pct: gov.fg_cpu_pct, feel_ms: (f.samples >= 10).then(|| f.ms()) }
    };
    loop {
        if stop.exists() { let _ = std::fs::remove_file(&stop); break; }
        if n % 30 == 0 {
            let acting = gov.acting;
            let (_, ran) = process_once_ex(dir, Some((&learner, &mut gov, acting)));
            if !ran.is_empty() {
                let m = metrics(&gov);
                for (action, name, trig) in ran { learner.start("fix", &action, &name, trig, now_unix(), m.clone()); }
            }
        }
        if n > 0 && n % 30 == 0 && tuning_mt(dir) != tune_mt {
            tune_mt = tuning_mt(dir);
            let (t, errs) = Tuning::load(dir);
            for e in errs { note(dir, "gov:settings", "refused", &format!("{}: {}", snifrig_fix::governor::TUNING_FILE, e)); }
            note(dir, "gov:settings", "done", &format!("governor settings reloaded: busy at {:.0}% for {} s, demote apps over {:.0}% of a core, up to level {}, performance mode {}", t.busy_pct, t.busy_ticks, t.min_core_pct, t.max_level, if t.game_mode { "on" } else { "off" }));
            enf.reapply_secs = t.reapply_secs;
            gov.tuning = t;
        }
        if n > 0 && n % 30 == 0 {
            let mt = rules::mtime(dir);
            if mt != rules_mt {
                rules_mt = mt;
                rule_set = rules::load(dir);
                gov.set_rules(rule_set.clone());
                enf.reset();
                sugg = rules::suggestions(dir);
            }
        }
        if n % 10 == 7 {
            let now = now_unix();
            if gov.dwm_pct >= DWM_STUCK_PCT { if dwm_hot == 0.0 { dwm_hot = now; } } else { dwm_hot = 0.0; }
            if dwm_hot > 0.0 && now - dwm_hot >= DWM_STUCK_SECS { offer_desktop_restart(dir, gov.dwm_pct, now); dwm_hot = now; }
        }
        if n % 60 == 30 {
            write_learned(dir, &learner, &gov.hints);
            let at = dir.join("feel-gate-on-at");
            if let Some(t) = std::fs::read_to_string(&at).ok().and_then(|s| s.trim().parse::<f64>().ok()) {
                if now_unix() >= t {
                    let _ = std::fs::remove_file(&at);
                    let text = std::fs::read_to_string(&tf).unwrap_or_default();
                    if text.lines().any(|l| l.split('#').next().unwrap_or("").replace(' ', "").eq_ignore_ascii_case("feel_gate=off")) {
                        let new: Vec<String> = text.lines().map(|l| if l.split('#').next().unwrap_or("").replace(' ', "").eq_ignore_ascii_case("feel_gate=off") { "feel_gate = on".to_string() } else { l.to_string() }).collect();
                        rules::write_atomic(&tf, &(new.join("\n") + "\n"));
                        note(dir, "gov:settings", "done", "the feel meter has been watching for a day, so the feel gate is back on: snifrig only reins apps in when the PC actually feels slow");
                    }
                }
            }
        }
        if n > 0 && n % 60 == 0 {
            sugg = rules::suggestions(dir);
            // Learned rules: a background program (no window, not in use) eased again and again all
            // week, never undone, just always runs in the background from now on.
            for (name, count) in sugg.clone() {
                if count < AUTO_RULE_AFTER || snifrig_fix::inuse::is_host(&name) || learner.undos(&name) > 0 || snifrig_fix::inuse::why_in_use(&name, now_unix()).is_some() { continue; }
                if rules::add(dir, &format!("{} background=yes", name)).is_ok() {
                    learn::append_feedback(dir, &Feedback::new("rule_added", "learned", "add", &name));
                    note(dir, "learn:rule", "done", &format!("{} was eased {} times this week and you never undid it, so it now always runs in the background (a rule was added; remove it on the App rules page)", name, count));
                }
            }
        }
        let mode = ledger::read_mode(dir);
        let paused = now_unix() < ledger::paused_until(dir);
        let mut act = false;
        if mode == Mode::Off || paused {
            if !gov.demoted.is_empty() { let a = gov.acting; log_gov(dir, gov.restore_all(a), a); }
            for m in perf.stop(gov.acting) { note(dir, "perf:mode", "done", &m); }
            gov.game = None;
        } else {
            act = matches!(mode, Mode::Ask | Mode::Auto);
            if act != gov.acting && !gov.demoted.is_empty() { let a = gov.acting; log_gov(dir, gov.restore_all(a), a); }
            if act != enf.acting { enf.reset(); }
            gov.acting = act;
            enf.acting = act;
            gov.feels_slow = snifrig_fix::feel::sluggish_with(&snifrig_fix::feel::recent(5), gov.dwm_pct);
            gov.user_says = learner.user_feel(now_unix());
            if let Some(s) = gov.user_says { gov.feels_slow = s; }
            let changes = gov.tick(now_unix(), act);
            let brain_game = |name: &str| brain.cache().get(name).map_or(false, |v| v.kind == "game" && v.confidence >= snifrig_fix::brain::MIN_CONF);
            for m in perf.tick(gov.procs(), sys::foreground_pid(), &rule_set, &gov.tuning, act, &brain_game) {
                note(dir, "perf:mode", if act { "done" } else { "would" }, &m);
            }
            gov.game = perf.game.as_ref().map(|g| g.2.clone());
            if act {
                let fresh: Vec<&str> = changes.iter().filter(|c| c.result.is_ok() && c.from == 0 && c.to == 1).map(|c| c.name.as_str()).collect();
                if !fresh.is_empty() {
                    let m = metrics(&gov);
                    for name in fresh { learner.start("governor", "demote", name, Trigger::Cpu, now_unix(), m.clone()); }
                }
            }
            log_gov(dir, changes, act);
            let applied = enf.tick(gov.procs(), &rule_set);
            if act {
                let m = metrics(&gov);
                for a in applied.iter().filter(|a| a.result.is_ok() && !matches!(a.kind, "refused" | "max_instances")) {
                    learner.start("rule", a.kind, &a.name, Trigger::Cpu, now_unix(), m.clone());
                }
            }
            log_enf(dir, applied, act);
        }
        handle_undo(dir, &mut gov, act);
        if n % 10 == 0 {
            let (now, m) = (now_unix(), metrics(&gov));
            learner.poll_feedback(now, &m);
            learner.tick(now, &m);
            gov.hints = learner.hints();
            // Ask the local brain (OpenJev) what the busiest programs are; it answers later, off this thread.
            for (name, pct) in gov.top.iter().take(5) {
                if *pct < 5.0 { continue; }
                if let Some(p) = gov.procs().iter().find(|p| p.name.eq_ignore_ascii_case(name)) {
                    let parent = gov.procs().iter().find(|q| q.pid == p.ppid).map(|q| q.name.clone()).unwrap_or_default();
                    brain.want(name, &sys::cmdline(p.pid), &parent);
                }
            }
            let cache = brain.cache();
            snifrig_fix::brain::apply(&mut gov.hints, &cache);
            snifrig_fix::inuse::set_kinds(cache.iter().filter(|(_, v)| v.confidence >= snifrig_fix::brain::MIN_CONF).map(|(k, v)| (k.clone(), v.kind.clone())).collect());
            // Drop queued end/restart proposals for apps the user is using now.
            if n % 30 == 0 {
                let mut items = ledger::load_pending(dir);
                let before = items.len();
                let mut dropped = Vec::new();
                let listen = snifrig_fix::judge::listening_pids();
                items.retain(|x| {
                    let ends = x.action == "terminate" || x.action.starts_with("restart");
                    if listen.contains_key(&x.pid) { dropped.push((x.name.clone(), x.action.clone(), "it is a server other programs use".into())); return false; }
                    if !(ends || x.action == "trim") { return true; }
                    let why = match listen.get(&x.pid) { Some(p) => Some(format!("it is a server other programs use, port {}", p)), None => snifrig_fix::inuse::why_in_use(&x.name, now) };
                    match why { Some(r) => { dropped.push((x.name.clone(), x.action.clone(), r)); false } None => true }
                });
                if items.len() != before {
                    ledger::save_pending(dir, &items);
                    for (name, action, r) in dropped {
                        let what = if action == "trim" { "snifrig frees its memory by itself when needed" } else { "snifrig reins it in instead" };
                        note(dir, "fix:in-use", "withdrawn", &format!("withdrew the question to {} {}: you are using it ({}); {}", action, name, r, what));
                    }
                }
            }
            for note in learner.take_notes() {
                let alert = snifrig_fix::Alert { t: String::new(), key: "learn:blocked".into(), msg: note.clone() };
                ledger::record(dir, &alert, &Action::Report, None, "refused", &note);
            }
        }
        if n % 10 == 5 {
            // Incident memory: open, extend, match against the past, close with what ended it.
            let now = now_unix();
            let st = std::fs::read_to_string(dir.join("status.json")).unwrap_or_default();
            let fresh = now - snifrig_fix::json::num_field(&st, "unix").unwrap_or(0.0) < 180.0;
            let severity = if fresh { snifrig_fix::json::num_field(&st, "severity").unwrap_or(0.0) } else { 0.0 };
            let headline = if fresh { snifrig_fix::json::str_field(&st, "headline").unwrap_or_default() } else { String::new() };
            let f = snifrig_fix::feel::recent(10);
            let mut toks = snifrig_fix::incident::recent_alert_tokens(dir, now, 900.0);
            if snifrig_fix::feel::sluggish_with(&f, gov.dwm_pct) { toks.insert("feel:sluggish".into()); }
            if gov.dwm_pct >= snifrig_fix::feel::DWM_BUSY_PCT { toks.insert("desktop:busy".into()); }
            let you_slow = gov.user_says == Some(true);
            if you_slow { toks.insert("you:slow".into()); }
            for d in gov.demoted.values() { toks.insert(format!("busy:{}", d.name.to_lowercase())); }
            let date = |u: f64| local_date(u);
            if let Some(msg) = incidents.tick(dir, now, you_slow || severity >= 1.0 || snifrig_fix::feel::sluggish_with(&f, gov.dwm_pct), &headline, toks, &date) { note(dir, "incident", "report", &msg); }
        }
        if n > 0 && n % 60 == 0 {
            let (now, m) = (now_unix(), metrics(&gov));
            let top: Vec<String> = gov.top.iter().map(|(name, _)| name.clone()).collect();
            if gov.mood.busy_with(&gov.tuning) && now - last_unusual > 3600.0 {
                if let Some(z) = base.unusual(snifrig_fix::baseline::Metric::Cpu, m.total_cpu_pct, now).filter(|z| *z >= 2.5) {
                    last_unusual = now;
                    let usual = base.expected_names(now);
                    let new: Vec<&String> = top.iter().filter(|t| !usual.contains(t)).collect();
                    let who = if new.is_empty() { String::new() } else { format!("; not usually busy at this hour: {}", new.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")) };
                    note(dir, "learn:unusual", "report", &format!("CPU at {:.0}% is unusual for this hour of the week (z {:.1}){}", m.total_cpu_pct, z, who));
                }
            }
            base.update(now, m.total_cpu_pct, m.commit_pct, &top);
            base.maybe_save(dir, now);
        }
        let (full, key) = state_json(&gov, act, &sugg);
        if key != last_key || now_unix() - last_write >= 10.0 {
            rules::write_atomic(&dir.join("governor.json"), &full);
            last_key = key;
            last_write = now_unix();
        }
        n += 1;
        std::thread::sleep(std::time::Duration::from_millis(1000));
    }
    { let a = gov.acting; log_gov(dir, gov.restore_all(a), a); for m in perf.stop(a) { note(dir, "perf:mode", "done", &m); } }
    base.save(dir, now_unix());
    let _ = std::fs::remove_file(dir.join("governor.json"));
}

/// `governor status` in plain words, from governor.json.
fn gov_status(dir: &Path) -> Result<String, String> {
    use snifrig_fix::json::{num_field, str_field};
    let s = std::fs::read_to_string(dir.join("governor.json")).map_err(|_| "no governor.json yet: the governor is not running (start snifrig-fix with no command)".to_string())?;
    let age = now_unix() - num_field(&s, "unix").unwrap_or(0.0);
    let mut o = String::new();
    if age > 30.0 { o.push_str(&format!("(last update {:.0} s ago: the governor is probably not running)\n", age)); }
    let acting = s.contains("\"acting\":true");
    o.push_str(&format!("governor: {}\n", if acting { "acting (changes are real)" } else { "dry run (nothing is changed, only logged)" }));
    o.push_str(&format!("cpu: {:.0}% ({})\n", num_field(&s, "cpu").unwrap_or(0.0), if s.contains("\"busy\":true") { "busy" } else { "not busy" }));
    let seg = |from: &str, to: &str| -> String {
        let a = s.find(from).map_or(s.len(), |i| i + from.len());
        let b = s[a..].find(to).map_or(s.len(), |i| a + i);
        s[a..b].to_string()
    };
    let dem = seg("\"demoted\":[", "],\"suggestions\"");
    let items: Vec<&str> = dem.split("{\"pid\":").skip(1).collect();
    if items.is_empty() { o.push_str("demoted: none\n"); } else { o.push_str(&format!("demoted: {}\n", items.len())); }
    for it in items {
        let obj = format!("{{\"pid\":{}", it);
        let level = num_field(&obj, "level").unwrap_or(0.0) as u8;
        let what = match level { 1 => "efficiency mode + low I/O", 2 => "below-normal priority", _ => "idle priority + low memory priority" };
        o.push_str(&format!("  {} (pid {}): level {}, {}; {}\n", str_field(&obj, "name").unwrap_or_default(), num_field(&obj, "pid").unwrap_or(0.0), level, what, str_field(&obj, "reason").unwrap_or_default()));
    }
    let sg = seg("\"suggestions\":[", "]}");
    for it in sg.split("{\"name\":").skip(1) {
        let obj = format!("{{\"name\":{}", it);
        o.push_str(&format!("suggestion: {} was slowed down {} times this week; consider: snifrig-fix rule add \"{} background=yes\"\n", str_field(&obj, "name").unwrap_or_default(), num_field(&obj, "count").unwrap_or(0.0), str_field(&obj, "name").unwrap_or_default()));
    }
    Ok(o.trim_end().to_string())
}
const DENIED: &str = "snifrig-fix is the paid part of Snifrig and needs a valid key. Prices: https://github.com/estejosh/snifrig/blob/main/PRICING.md";
const USAGE: &str = "usage: snifrig-fix [once|status|pending|mode M|approve ID|ease ID|dismiss ID|governor status|governor undo PID|brain NAME|rule add \"PATTERN key=value ...\"|rule list|rule remove N|machine-id|license install PATH|license accept|license status] [--dir D] [--accept-license \"UFL-3.7 snifrig-fix\"]";
const ACCEPT_PHRASE: &str = "UFL-3.7 snifrig-fix";

/// The hard gate: a valid key and a recorded acceptance, or Err(reason).
fn gate(dir: &Path) -> Result<(), String> {
    license::check(dir)?;
    if !license::accepted(dir) {
        return Err("UFL-3.7 has not been accepted for snifrig-fix on this machine; run: snifrig-fix license accept".into());
    }
    Ok(())
}

fn accept(dir: &Path, given: Option<String>) -> Result<String, String> {
    let said = match given {
        Some(g) => g,
        None => {
            println!("Component snifrig-fix, UFL 3.7, Operational Scope: Paid. Published Price: {}. Without a valid key the fixer will not run.", license::PRICING_URL);
            println!("To accept, type exactly: I accept {}", ACCEPT_PHRASE);
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).map_err(|e| format!("cannot read input: {}", e))?;
            line.trim_start_matches('\u{feff}').trim_end_matches(|c| c == '\r' || c == '\n').strip_prefix("I accept ").unwrap_or("").to_string()
        }
    };
    if said != ACCEPT_PHRASE { return Err(format!("not accepted; the exact text is: I accept {} (flag form: --accept-license \"{}\")", ACCEPT_PHRASE, ACCEPT_PHRASE)); }
    license::record_acceptance(dir)?;
    Ok("accepted UFL-3.7 for snifrig-fix (recorded locally in fix-accepted.json, nothing sent)".into())
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let flag = |n: &str| a.iter().position(|x| x == n).and_then(|i| a.get(i + 1)).cloned();
    let dir = flag("--dir").map(PathBuf::from).unwrap_or_else(default_dir);
    let _ = std::fs::create_dir_all(&dir);
    let mut cmd: Vec<&str> = Vec::new();
    let mut skip = false;
    for s in a.iter().map(|s| s.as_str()) {
        if skip { skip = false; continue; }
        if s == "--dir" || s == "--accept-license" { skip = true; continue; }
        if !s.starts_with("--") { cmd.push(s); }
    }
    let help = a.iter().any(|x| matches!(x.as_str(), "--help" | "-h" | "help" | "/?"));
    let exempt = help || matches!(cmd.as_slice(), ["license", "install", _] | ["license"] | ["license", "status"] | ["license", "accept"] | ["machine-id"]);
    if !exempt {
        if let Err(why) = gate(&dir) {
            eprintln!("{}\n{}", DENIED, why);
            std::process::exit(2);
        }
    }
    let out = |r: Result<String, String>| match r { Ok(m) => println!("{}", m), Err(e) => { eprintln!("{}", e); std::process::exit(1) } };
    if help { println!("{}", USAGE); return; }
    match cmd.as_slice() {
        ["brain", name] => {
            let found = sys::snapshot(&mut Vec::new()).into_iter().find(|p| p.name.eq_ignore_ascii_case(name));
            let cmd = found.as_ref().map(|p| sys::cmdline(p.pid)).unwrap_or_default();
            out(snifrig_fix::brain::ask_now(&dir, name, &cmd, "").map(|v| {
                let (w, max) = snifrig_fix::brain::shape(&v);
                format!("{}: kind {} (confidence {:.2}), importance {:.2} -> weight {:.1}, deepest level {}", name, v.kind, v.confidence, v.importance, w, max)
            }))
        }
        ["ease", id] => out(ease(&dir, id)),
        ["feel", w @ ("slow" | "fine")] => out(feel_cmd(&dir, *w == "slow")),
        ["desktop", w @ ("restart" | "no")] => out(desktop(&dir, *w == "restart")),
        ["unlearn", name, action] => out(unlearn(&dir, name, action)),
        ["mode", m] => match Mode::parse(m) {
            Some(m) => { ledger::write_mode(&dir, m); println!("fixer mode: {}", m.as_str()); }
            None => out(Err("mode must be off, dry-run, ask or auto".into())),
        },
        ["status"] => {
            let p = policy(&dir);
            let l = license::status(&dir);
            println!("mode: {}", p.mode.as_str());
            println!("paused: {}", if p.now < p.paused_until { format!("yes, {:.0} min left", (p.paused_until - p.now) / 60.0) } else { "no".into() });
            println!("license: {}", if l.licensed { format!("{} ({} seats, valid until {})", l.licensee, l.seats, l.not_after) } else { format!("none ({})", l.note) });
            println!("pending: {}", ledger::load_pending(&dir).len());
        }
        ["pending"] => for p in ledger::load_pending(&dir) { println!("{}  {} {}#{}  {}", p.id, p.action, p.name, p.pid, p.why); },
        ["approve", id] => out(approve(&dir, id)),
        ["dismiss", id] => out(dismiss(&dir, id)),
        ["license", "install", path] => out(license::install(&dir, Path::new(path))),
        ["license", "accept"] => out(accept(&dir, flag("--accept-license"))),
        ["machine-id"] => match license::machine_hash() {
            Some(h) => println!("{}", h),
            None => out(Err("cannot read the machine id".into())),
        },
        ["license"] | ["license", "status"] => {
            let l = license::status(&dir);
            println!("{}", if l.licensed { format!("licensed to {}, valid until {}; UFL-3.7 accepted: {}", l.licensee, l.not_after, if license::accepted(&dir) { "yes" } else { "no" }) } else { format!("{} (UFL-3.7 accepted: {})", l.note, if license::accepted(&dir) { "yes" } else { "no" }) });
        }
        ["once"] => { let n = process_once(&dir); println!("processed; {} automatic actions", n); }
        [] => watch(&dir),
        ["governor", "status"] => out(gov_status(&dir)),
        ["governor", "undo", pid] => match pid.parse::<u32>() {
            Ok(p) => {
                let _ = std::fs::write(dir.join(format!("gov-undo-{}", p)), "1\n");
                println!("asked the running governor to restore pid {} (it acts within a second; if it is not running, nothing happens)", p);
            }
            Err(_) => out(Err("usage: snifrig-fix governor undo PID".into())),
        },
        ["rule", "add", rest @ ..] if !rest.is_empty() => {
            let r = rules::add(&dir, &rest.join(" "));
            if r.is_ok() {
                let pat = rest.join(" ").split_whitespace().next().unwrap_or("").to_lowercase();
                learn::append_feedback(&dir, &Feedback::new("rule_added", "rule", "add", &pat));
            }
            out(r.map(|t| format!("added rule: {}", t)))
        }
        ["rule", "list"] => {
            let lines = rules::rule_lines(&dir);
            if lines.is_empty() { println!("no rules yet; add one with: snifrig-fix rule add \"ffmpeg*.exe priority=below_normal cap=60\""); }
            for (i, l) in lines.iter().enumerate() {
                match rules::parse_line(l) { Ok(_) => println!("{}  {}", i + 1, l), Err(e) => println!("{}  {}   (ignored: {})", i + 1, l, e) }
            }
        }
        ["rule", "remove", n] => match n.parse::<usize>() {
            Ok(n) => out(rules::remove(&dir, n).map(|l| format!("removed rule: {}", l))),
            Err(_) => out(Err("usage: snifrig-fix rule remove N (numbers from `rule list`)".into())),
        },        _ => out(Err(USAGE.into())),
    }
}
