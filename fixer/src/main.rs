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
                    // You said you use this app and want it fast: never ask about slowing it.
                    if kept(dir, &t.name) { continue; }
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

/// Windows' refusal in plain words.
fn plain_err(e: &str) -> String {
    if e.ends_with("failed: 5") || e.ends_with("error 5") { "Windows refused: it runs with more rights than snifrig (as administrator, or protected)".into() }
    else if e.ends_with("failed: 87") || e.ends_with("error 87") { "it had just exited".into() }
    else { e.to_string() }
}

/// The process to act on: the same pid if it is still that app, else the busiest copy of the app
/// (apps such as browsers restart their processes, so the pid in a question goes stale).
fn find_target(pid: u32, name: &str) -> Option<Target> {
    proc::target(pid).filter(|t| t.name.eq_ignore_ascii_case(name)).or_else(|| {
        sys::snapshot(&mut Vec::new()).into_iter().filter(|p| p.name.eq_ignore_ascii_case(name)).filter_map(|p| proc::target(p.pid))
            .max_by(|a, b| a.private_mb.total_cmp(&b.private_mb))
    })
}

/// "Rein it in" instead of ending or restarting: efficiency mode, below-normal priority, memory trimmed.
/// Works even when the question was already withdrawn (the window passes the app's name and pid).
/// If nothing could be changed, the question comes back so you can choose something else; the
/// failure is recorded with the reason, like every failed command (see `out` in main).
fn ease(dir: &Path, id: &str, name: Option<&str>, pid: Option<u32>) -> Result<String, String> {
    if now_unix() < ledger::paused_until(dir) { return Err("could not rein it in: fixing is paused".into()); }
    let mut items = ledger::load_pending(dir);
    let item = match items.iter().position(|x| x.id == id) {
        Some(i) => { let it = items.remove(i); ledger::save_pending(dir, &items); Some(it) }
        None => None,
    };
    let (name, pid) = match (&item, name) {
        (Some(it), _) => (it.name.clone(), it.pid),
        (None, Some(n)) => (n.to_string(), pid.unwrap_or(0)),
        (None, None) => return Err("could not rein it in: that question was already answered or withdrawn".into()),
    };
    let put_back = |dir: &Path| if let Some(it) = &item { let mut v = ledger::load_pending(dir); if !v.iter().any(|x| x.id == it.id) { v.push(it.clone()); ledger::save_pending(dir, &v); } };
    let Some(t) = find_target(pid, &name) else {
        let there = sys::snapshot(&mut Vec::new()).iter().any(|p| p.name.eq_ignore_ascii_case(&name));
        return Err(format!("could not rein in {}: {}", name, if there { "Windows would not let snifrig open it (it runs with more rights than snifrig, or is protected)" } else { "it is no longer running" }));
    };
    if let Some(why) = snifrig_fix::policy::denied(&t, &Action::LowerPriority, &ledger::read_list(dir, "fix-deny.txt")) { return Err(format!("could not rein in {}: {}", name, why)); }
    let (mut done, mut failed) = (Vec::new(), Vec::new());
    match sys::set_efficiency(t.pid, true) { Ok(_) => done.push("efficiency mode"), Err(e) => failed.push(format!("efficiency mode ({})", plain_err(&e))) }
    match sys::set_priority(t.pid, sys::BELOW_NORMAL) { Ok(_) => done.push("below-normal priority"), Err(e) => failed.push(format!("priority ({})", plain_err(&e))) }
    match actions::run(&Action::Trim, &t) { Ok(_) => done.push("memory trimmed"), Err(e) => failed.push(format!("memory trim ({})", plain_err(&e))) }
    let key = item.as_ref().map(|i| i.key.clone()).unwrap_or_else(|| format!("ease:{}", name));
    let alert = snifrig_fix::Alert { t: String::new(), key, msg: item.as_ref().map(|i| i.msg.clone()).unwrap_or_default() };
    if done.is_empty() {
        put_back(dir);
        return Err(format!("could not rein in {} (pid {}): {}", t.name, t.pid, failed.join("; ")));
    }
    let instead = item.as_ref().map(|i| format!(" instead of {}", i.action)).unwrap_or_default();
    let note = format!("reined in at your request{}: {}{}", instead, done.join(", "), if failed.is_empty() { String::new() } else { format!("; not done: {}", failed.join("; ")) });
    ledger::record(dir, &alert, &Action::LowerPriority, Some(&t), if failed.is_empty() { "done" } else { "partial" }, &note);
    if let Some(i) = &item { learn::append_feedback(dir, &Feedback::new("dismiss", "fix", &i.action, &i.name)); }
    Ok(format!("{}: {}{}", t.name, done.join(", "), if failed.is_empty() { String::new() } else { format!(" (not done: {})", failed.join("; ")) }))
}

// ---- Heavy loads the governor cannot turn down ----
// The Linux VM (WSL) and a hidden full-screen game that keeps the desktop redrawing. snifrig
// asks first. After you say yes twice it does it by itself (and on a quick fix), unless you
// marked the app "keep it fast". One "no" sends it back to asking. Never while you are in a game.

struct Heavy { wsl_hot: f64, game_hidden: f64, last_game: String }
static HEAVY: std::sync::Mutex<Heavy> = std::sync::Mutex::new(Heavy { wsl_hot: 0.0, game_hidden: 0.0, last_game: String::new() });
const WSL_HEAVY_PCT: f64 = 150.0;

/// Yeses for this kind since the last no.
fn heavy_trust(dir: &Path, kind: &str) -> usize {
    let text = std::fs::read_to_string(dir.join("heavy-trust.txt")).unwrap_or_default();
    let mut yes = 0;
    for l in text.lines() {
        let mut it = l.split_whitespace();
        match (it.next(), it.next()) { (Some("yes"), Some(k)) if k == kind => yes += 1, (Some("no"), Some(k)) if k == kind => yes = 0, _ => {} }
    }
    yes
}

fn heavy_said_no_lately(dir: &Path, kind: &str, secs: f64) -> bool {
    let text = std::fs::read_to_string(dir.join("heavy-trust.txt")).unwrap_or_default();
    text.lines().filter_map(|l| { let f: Vec<&str> = l.split_whitespace().collect(); (f.len() >= 3 && f[0] == "no" && f[1] == kind).then(|| f[2].parse::<f64>().ok()).flatten() })
        .any(|t| now_unix() - t < secs)
}

/// Games snifrig has seen in front (games-seen.txt), so the desktop is never restarted under one.
fn remember_game(dir: &Path, name: &str) {
    let p = dir.join("games-seen.txt");
    let text = std::fs::read_to_string(&p).unwrap_or_default();
    if text.lines().any(|l| l.trim().eq_ignore_ascii_case(name)) { return; }
    let _ = std::fs::write(&p, format!("{}{}\n", text, name));
}

/// A game that is running right now (in front, hidden or minimized), if any.
fn running_game(dir: &Path) -> Option<String> {
    let mut names: Vec<String> = std::fs::read_to_string(dir.join("games-seen.txt")).unwrap_or_default().lines().map(|l| l.trim().to_lowercase()).filter(|l| !l.is_empty()).collect();
    for r in rules::load(dir) { if r.game == Some(true) && !r.pattern.contains('*') { names.push(r.pattern.clone()); } }
    if names.is_empty() { return None; }
    let mut buf = Vec::new();
    sys::snapshot(&mut buf).into_iter().map(|p| p.name.to_lowercase()).find(|n| names.contains(n))
}

fn heavy_log(dir: &Path, kind: &str, yes: bool) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("heavy-trust.txt")) {
        let _ = writeln!(f, "{} {} {:.0}", if yes { "yes" } else { "no" }, kind, now_unix());
    }
}

/// A non-minimized window of the last game that covers its monitor while something else is in front.
fn hidden_game_window(gov: &snifrig_fix::governor::Governor, game: &str) -> Option<(isize, String)> {
    if game.is_empty() || gov.game.is_some() { return None; }
    let fg = sys::foreground_pid();
    sys::visible_windows().into_iter().filter(|(_, pid)| Some(*pid) != fg)
        .filter_map(|(h, pid)| gov.procs().iter().find(|p| p.pid == pid).map(|p| (h, p.name.clone())))
        .find(|(h, name)| name.eq_ignore_ascii_case(game) && sys::covers_monitor(*h))
}

fn shutdown_wsl(dir: &Path, why: &str) -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    let st = std::process::Command::new("wsl.exe").arg("--shutdown").creation_flags(0x0800_0000).status().map_err(|e| e.to_string())?;
    if !st.success() { return Err("wsl --shutdown failed".into()); }
    let msg = format!("shut down WSL (the Linux side): {}", why);
    let alert = snifrig_fix::Alert { t: String::new(), key: "heavy:wsl".into(), msg: msg.clone() };
    let t = Target { pid: 0, name: "vmmemwsl.exe".into(), ..Default::default() };
    ledger::record(dir, &alert, &Action::Report, Some(&t), "done", &msg);
    Ok(msg)
}

/// Look at the heavy loads; act on the ones you trusted, ask about the others. Returns what it did.
fn check_heavy(dir: &Path, gov: &snifrig_fix::governor::Governor, now: f64, quick: bool) -> Vec<String> {
    let mut did = Vec::new();
    let Ok(mut hv) = HEAVY.lock() else { return did };
    if let Some(g) = &gov.game { hv.last_game = g.to_lowercase(); remember_game(dir, &hv.last_game); }
    if gov.game.is_some() { return did; } // in a game: hold everything
    // WSL
    let wsl = gov.name_pct("vmmemwsl.exe");
    if wsl >= WSL_HEAVY_PCT { if hv.wsl_hot == 0.0 { hv.wsl_hot = now; } } else { hv.wsl_hot = 0.0; }
    let crowded = gov.total_pct >= 60.0 || gov.dwm_pct >= 70.0 || gov.feels_slow;
    if wsl >= WSL_HEAVY_PCT && !kept(dir, "vmmemwsl.exe") && (quick || (crowded && now - hv.wsl_hot >= 180.0)) {
        hv.wsl_hot = now;
        let linux_on_screen = gov.visible.iter().any(|n| { let n = n.to_lowercase(); n.starts_with("wsl") || n.contains("ubuntu") });
        if heavy_trust(dir, "wsl") >= 2 && !linux_on_screen {
            if let Ok(m) = shutdown_wsl(dir, &format!("it used {:.1} cores while the PC was crowded; you said yes to this before", wsl / 100.0)) { did.push(m); }
        } else if !heavy_said_no_lately(dir, "wsl", 86400.0) {
            add_card(dir, "shutdown_wsl", "vmmemwsl.exe", "shutdown_wsl", &format!("WSL (the Linux side of this PC) is using {:.1} cores while the PC is crowded. Shutting it down frees them at once; whatever runs in Linux stops. Say yes twice and snifrig does this by itself next time (never while a Linux window is on screen).", wsl / 100.0));
        }
    }
    // A hidden game still drawing full screen keeps dwm busy.
    let game = hv.last_game.clone();
    match (gov.dwm_pct >= 50.0).then(|| hidden_game_window(gov, &game)).flatten() {
        Some((h, name)) if !kept(dir, &name) => {
            if hv.game_hidden == 0.0 { hv.game_hidden = now; }
            if quick || now - hv.game_hidden >= 60.0 {
                hv.game_hidden = now;
                if quick || heavy_trust(dir, "minimize") >= 2 {
                    if sys::minimize_quietly(h) {
                        let m = format!("minimized {}: it was hidden but still drawing full screen, keeping the desktop at {:.0}% of a core", name, gov.dwm_pct);
                        let alert = snifrig_fix::Alert { t: String::new(), key: "heavy:game".into(), msg: m.clone() };
                        ledger::record(dir, &alert, &Action::Report, None, "done", &m);
                        did.push(m);
                    }
                } else if !heavy_said_no_lately(dir, "minimize", 86400.0) {
                    add_card(dir, "minimize_game", &name, "minimize_game", &format!("{} is hidden but still drawing full screen, which keeps the desktop (dwm) at {:.0}% of a core and makes every window stutter. Minimizing it stops that; the game keeps running. Say yes twice and snifrig does this by itself next time.", name, gov.dwm_pct));
                }
            }
        }
        _ => hv.game_hidden = 0.0,
    }
    did
}

/// `heavy wsl|minimize yes|no APP`: your answer on a card.
fn heavy_cmd(dir: &Path, kind: &str, yes: bool, app: &str) -> Result<String, String> {
    heavy_log(dir, kind, yes);
    if !yes { return Ok("ok, snifrig won't ask about this for a day".into()); }
    match kind {
        "wsl" => shutdown_wsl(dir, "you said yes"),
        "minimize" => {
            let fg = sys::foreground_pid();
            let n = sys::visible_windows().into_iter().filter(|(_, pid)| Some(*pid) != fg)
                .filter(|(h, pid)| sys::covers_monitor(*h) && proc::target(*pid).map_or(false, |t| t.name.eq_ignore_ascii_case(app)))
                .filter(|(h, _)| sys::minimize_quietly(*h)).count();
            if n == 0 { Err(format!("{} has no hidden full-screen window right now", app)) } else { Ok(format!("minimized {}", app)) }
        }
        _ => Err("unknown kind".into()),
    }
}

/// Apps you marked "I use this, keep it fast".
fn kept(dir: &Path, name: &str) -> bool {
    let rs = rules::load(dir);
    rules::rule_keeps(&rs, &name.to_lowercase())
}

/// `keep NAME`: you use this app and want it fast. Replaces any rule written for exactly that
/// name (a core limit, a background rule) with keep=yes, and drops the questions about it.
fn keep_cmd(dir: &Path, name: &str) -> Result<String, String> {
    let mut n = name.trim().to_lowercase();
    if n.is_empty() || n.contains(char::is_whitespace) || n.contains('=') { return Err("give the program name, e.g. claude.exe".into()); }
    if !n.ends_with(".exe") { n.push_str(".exe"); }
    let mut removed = Vec::new();
    loop {
        let lines = rules::rule_lines(dir);
        let Some(i) = lines.iter().position(|l| l.split_whitespace().next().map_or(false, |p| p.eq_ignore_ascii_case(&n))) else { break };
        removed.push(lines[i].trim().to_string());
        rules::remove(dir, i + 1)?;
    }
    rules::add(dir, &format!("{} keep=yes", n))?;
    let mut items = ledger::load_pending(dir);
    let before = items.len();
    items.retain(|x| !x.name.eq_ignore_ascii_case(&n));
    if items.len() != before { ledger::save_pending(dir, &items); }
    learn::append_feedback(dir, &Feedback::new("keep", "user", "keep", &n));
    let mut msg = format!("{} is kept fast: never slowed while it is on a screen, only a lower priority while hidden.", n);
    if !removed.is_empty() { msg.push_str(&format!(" Replaced: {}.", removed.join("; "))); }
    Ok(msg)
}

/// `unkeep NAME`: snifrig may ease it again like any other app.
fn unkeep_cmd(dir: &Path, name: &str) -> Result<String, String> {
    let mut n = name.trim().to_lowercase();
    if !n.ends_with(".exe") { n.push_str(".exe"); }
    let lines = rules::rule_lines(dir);
    let i = lines.iter().position(|l| l.trim().eq_ignore_ascii_case(&format!("{} keep=yes", n))).ok_or("that app is not on the keep-fast list")?;
    rules::remove(dir, i + 1)?;
    Ok(format!("{} is no longer on the keep-fast list.", n))
}

// ---- Jobs: your scheduled tasks, run by snifrig (see sched.rs) ----

const JOBS_IMPORT_PS1: &str = include_str!("jobs-import.ps1");
const JOBS_TOGGLE_PS1: &str = include_str!("jobs-toggle.ps1");

/// Run one of snifrig's PowerShell helpers with no window; output comes back as text.
fn run_ps(dir: &Path, file: &str, body: &str, args: &[String]) -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    let p = dir.join(file);
    std::fs::write(&p, body).map_err(|e| e.to_string())?;
    let o = std::process::Command::new("powershell.exe").args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]).arg(&p).args(args)
        .creation_flags(0x0800_0000).output().map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&o.stdout).to_string())
}

/// Turn Windows tasks off or on (listed in `list`, one per line) as administrator: Windows
/// shows its own confirmation first.
fn toggle_elevated(dir: &Path, mode: &str, list: &Path) -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    let script = dir.join("jobs-toggle.ps1");
    std::fs::write(&script, JOBS_TOGGLE_PS1).map_err(|e| e.to_string())?;
    let out = dir.join("jobs-toggle.out");
    let _ = std::fs::remove_file(&out);
    let wrapper = dir.join("jobs-toggle-elevated.ps1");
    std::fs::write(&wrapper, format!("& '{}' -Mode '{}' -ListFile '{}' *> '{}'\r\n", script.display(), mode, list.display(), out.display())).map_err(|e| e.to_string())?;
    let st = std::process::Command::new("powershell.exe").args(["-NoProfile", "-WindowStyle", "Hidden", "-Command"])
        .arg(format!("Start-Process powershell.exe -Verb RunAs -Wait -WindowStyle Hidden -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File','{}'", wrapper.display()))
        .creation_flags(0x0800_0000).status().map_err(|e| e.to_string())?;
    if !st.success() { return Err("the Windows confirmation was cancelled".into()); }
    Ok(std::fs::read_to_string(&out).unwrap_or_default())
}

/// `jobs import`: move your script-type scheduled tasks into snifrig (Windows copies backed up
/// and turned off; undo puts them back).
fn import_jobs(dir: &Path, admin_ok: bool) -> Result<String, String> {
    use snifrig_fix::sched;
    let report = run_ps(dir, "jobs-import.ps1", JOBS_IMPORT_PS1, &["-Dir".into(), dir.display().to_string()])?;
    let (new, errs) = sched::parse(&std::fs::read_to_string(dir.join("jobs-import.txt")).unwrap_or_default());
    let (mut jobs, _) = sched::load(dir);
    let mut added = 0;
    for j in new { if !jobs.iter().any(|x| x.name.eq_ignore_ascii_case(&j.name)) { jobs.push(j); added += 1; } }
    sched::save(dir, &jobs);
    let mut msg = report.trim().to_string();
    for e in errs { msg.push_str(&format!("\nproblem: {}", e)); }
    let denied = dir.join("jobs-import-denied.txt");
    let denied_list = std::fs::read_to_string(&denied).unwrap_or_default();
    if !denied_list.trim().is_empty() && !admin_ok {
        // Found on its own (the task watcher): never raise a Windows prompt unasked, ask on a card.
        let names: Vec<&str> = denied_list.lines().filter(|l| !l.trim().is_empty()).collect();
        add_card(dir, "take_over_task", &names.join(", "), "take_over_task", &format!("snifrig now runs {} new scheduled task{} with no window, but {} belong{} to an administrator: Windows needs your OK to turn its own copy off (until then snifrig never starts a second copy).", names.len(), if names.len() == 1 { "" } else { "s" }, if names.len() == 1 { "it" } else { "they" }, if names.len() == 1 { "s" } else { "" }));
    } else if !denied_list.trim().is_empty() {
        msg.push_str("\nSome tasks belong to an administrator; turning their Windows copies off (Windows asks you first):\n");
        msg.push_str(&toggle_elevated(dir, "disable", &denied).unwrap_or_else(|e| format!("not turned off: {}. They still run from Windows too; snifrig will not start a second copy.", e)));
    }
    msg.push_str(&format!("\n{} job{} now run under snifrig. To put them back in Windows: snifrig-fix jobs undo-import", added, if added == 1 { "" } else { "s" }));
    Ok(msg)
}

/// `jobs undo-import`: turn the Windows copies back on and remove the imported jobs.
fn undo_import(dir: &Path) -> Result<String, String> {
    use snifrig_fix::sched;
    let (jobs, _) = sched::load(dir);
    let tasks: Vec<String> = jobs.iter().filter(|j| !j.from_task.is_empty()).map(|j| j.from_task.clone()).collect();
    if tasks.is_empty() { return Ok("no imported jobs to put back".into()); }
    let list = dir.join("jobs-undo.txt");
    std::fs::write(&list, tasks.join("\r\n")).map_err(|e| e.to_string())?;
    let mut out = run_ps(dir, "jobs-toggle.ps1", JOBS_TOGGLE_PS1, &["-Mode".into(), "enable".into(), "-ListFile".into(), list.display().to_string()])?;
    let failed: Vec<String> = out.lines().filter_map(|l| l.strip_prefix("failed: ")).map(|l| l.split(" (").next().unwrap_or(l).to_string()).collect();
    if !failed.is_empty() {
        let f = dir.join("jobs-undo-elevated.txt");
        std::fs::write(&f, failed.join("\r\n")).map_err(|e| e.to_string())?;
        out.push_str(&toggle_elevated(dir, "enable", &f).unwrap_or_else(|e| format!("not turned back on: {}", e)));
    }
    let keep: Vec<sched::Job> = jobs.into_iter().filter(|j| j.from_task.is_empty()).collect();
    sched::save(dir, &keep);
    Ok(format!("{}\nThe imported jobs are back in Windows Task Scheduler.", out.trim()))
}

/// Soft handback: Windows runs the moved jobs again (their Windows copies turned back on), and
/// snifrig stops running them, until it can take them back. Admin-owned ones wait for your OK
/// (a card), so nothing pops up on its own.
fn handback(dir: &Path, why: &str) -> Result<String, String> {
    use snifrig_fix::sched;
    let flag = dir.join(sched::HANDED_BACK);
    if flag.exists() { return Ok("Windows already runs the moved jobs".into()); }
    let (jobs, _) = sched::load(dir);
    let tasks: Vec<String> = jobs.iter().filter(|j| !j.from_task.is_empty()).map(|j| j.from_task.clone()).collect();
    if tasks.is_empty() { return Ok("no moved jobs".into()); }
    let list = dir.join("jobs-handback.txt");
    std::fs::write(&list, tasks.join("\r\n")).map_err(|e| e.to_string())?;
    let out = run_ps(dir, "jobs-toggle.ps1", JOBS_TOGGLE_PS1, &["-Mode".into(), "enable".into(), "-ListFile".into(), list.display().to_string()])?;
    let failed: Vec<String> = out.lines().filter_map(|l| l.strip_prefix("failed: ")).map(|l| l.split(" (").next().unwrap_or(l).to_string()).collect();
    let _ = std::fs::write(&flag, format!("{:.0} {}\n", now_unix(), why));
    if !failed.is_empty() {
        let _ = std::fs::write(dir.join("jobs-handback-denied.txt"), failed.join("\r\n"));
        add_card(dir, "jobs_handback", &failed.join(", "), "jobs_handback", &format!("{}, so Windows runs your jobs again. {} of them belong to an administrator and need your OK to turn back on in Windows.", why, failed.len()));
    }
    Ok(format!("{}: Windows runs {} moved job{} again.", why, tasks.len() - failed.len(), if tasks.len() - failed.len() == 1 { "" } else { "s" }))
}

/// Take the moved jobs back from Windows after a handback. `admin_ok`: ask Windows right away
/// for admin-owned ones (only when you asked); otherwise they wait for your OK on a card.
fn takeover(dir: &Path, admin_ok: bool) -> String {
    use snifrig_fix::sched;
    let flag = dir.join(sched::HANDED_BACK);
    if !flag.exists() { return "snifrig already runs the moved jobs".into(); }
    let (jobs, _) = sched::load(dir);
    let tasks: Vec<String> = jobs.iter().filter(|j| !j.from_task.is_empty()).map(|j| j.from_task.clone()).collect();
    let list = dir.join("jobs-takeover.txt");
    let _ = std::fs::write(&list, tasks.join("\r\n"));
    let out = run_ps(dir, "jobs-toggle.ps1", JOBS_TOGGLE_PS1, &["-Mode".into(), "disable".into(), "-ListFile".into(), list.display().to_string()]).unwrap_or_default();
    let failed: Vec<String> = out.lines().filter_map(|l| l.strip_prefix("failed: ")).map(|l| l.split(" (").next().unwrap_or(l).to_string()).collect();
    let _ = std::fs::remove_file(&flag);
    if !failed.is_empty() {
        let f = dir.join("jobs-import-denied.txt");
        let _ = std::fs::write(&f, failed.join("\r\n"));
        if admin_ok { let _ = toggle_elevated(dir, "disable", &f); }
        else { add_card(dir, "take_over_task", &failed.join(", "), "take_over_task", &format!("snifrig is running your jobs again. {} of them belong to an administrator: Windows needs your OK to turn its copies off (until then snifrig still never starts a second copy).", failed.len())); }
    }
    format!("snifrig runs the {} moved jobs again", tasks.len())
}

/// `popup yes|no WHO`: your answer to "do you want me to make it so this does not pop up and
/// take control of the desktop?". A task becomes a silent snifrig job; an app gets no_focus=yes.
fn popup_cmd(dir: &Path, yes: bool, who: &str) -> Result<String, String> {
    use std::io::Write;
    if !yes {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("popup-no.txt")) { let _ = writeln!(f, "{:.0}\t{}", now_unix(), who.to_lowercase()); }
        return Ok("ok, snifrig will not ask about this one for a week".into());
    }
    if who.starts_with('\\') {
        let report = run_ps(dir, "jobs-import.ps1", JOBS_IMPORT_PS1, &["-Dir".into(), dir.display().to_string(), "-Only".into(), who.to_string()])?;
        if report.contains("moved:") {
            let (new, _) = snifrig_fix::sched::parse(&std::fs::read_to_string(dir.join("jobs-import.txt")).unwrap_or_default());
            let (mut jobs, _) = snifrig_fix::sched::load(dir);
            for j in new { if !jobs.iter().any(|x| x.name.eq_ignore_ascii_case(&j.name)) { jobs.push(j); } }
            snifrig_fix::sched::save(dir, &jobs);
            let denied = dir.join("jobs-import-denied.txt");
            if !std::fs::read_to_string(&denied).unwrap_or_default().trim().is_empty() { let _ = toggle_elevated(dir, "disable", &denied); }
            return Ok(format!("{} now runs under snifrig, with no window.", who.trim_start_matches('\\')));
        }
        // Not a script task (an app's own updater, say): keep it from taking the screen instead.
    }
    let exe = who.rsplit('\\').next().unwrap_or(who).to_lowercase();
    let exe = if exe.ends_with(".exe") { exe } else { format!("{}.exe", exe) };
    rules::add(dir, &format!("{} no_focus=yes", exe))?;
    Ok(format!("{} can no longer take the screen from you: snifrig puts you back where you were.", exe))
}

fn popup_said_no(dir: &Path, key: &str) -> bool {
    std::fs::read_to_string(dir.join("popup-no.txt")).unwrap_or_default().lines().filter_map(|l| l.split_once('\t'))
        .any(|(t, k)| k == key && now_unix() - t.parse::<f64>().unwrap_or(0.0) < 7.0 * 86400.0)
}

/// Process chain of a pid, newest first: (image name, command line).
fn chain_of(pid: u32, procs: &[sys::Proc]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut p = pid;
    for _ in 0..6 {
        let Some(x) = procs.iter().find(|x| x.pid == p) else { break };
        out.push((x.name.clone(), sys::cmdline(x.pid)));
        if x.ppid == 0 || x.ppid == p { break; }
        p = x.ppid;
    }
    out
}

/// Which Windows task runs this command line, if any.
fn task_for_cmd(dir: &Path, cmd: &str) -> Option<String> {
    use std::os::windows::process::CommandExt;
    let script = r#"$c = ($env:SNIF_CMD.ToLower() -replace '"','' -replace '\s+',' ').Trim()
Get-ScheduledTask | Where-Object { $_.TaskPath -notlike '\Microsoft\*' } | ForEach-Object { $t = $_; foreach ($a in $t.Actions) {
  $x = ([Environment]::ExpandEnvironmentVariables("$($a.Execute) $($a.Arguments)").ToLower() -replace '"','' -replace '\s+',' ').Trim()
  $x = $x -replace '^\S*conhost\.exe --headless ',''
  if ($x -and ($c.Contains($x) -or $x.Contains($c))) { "$($t.TaskPath)$($t.TaskName)" } } } | Select-Object -First 1"#;
    let p = dir.join("popup-find-task.ps1");
    std::fs::write(&p, script).ok()?;
    let o = std::process::Command::new("powershell.exe").args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]).arg(&p)
        .env("SNIF_CMD", cmd).creation_flags(0x0800_0000 | 0x0000_0040).output().ok()?;
    let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// The pop-up guard thread: watches what comes to the front. When something takes the screen
/// without you asking, it names the culprit and asks once (a card, held while you game); for
/// apps you said yes to, it puts you straight back where you were.
fn start_guard(dir: std::path::PathBuf) {
    use snifrig_fix::guard;
    let _ = std::thread::Builder::new().name("popup-guard".into()).spawn(move || {
        let mut last = guard::foreground().unwrap_or_default();
        let mut rules_at = 0.0f64;
        let mut rules_set = rules::load(&dir);
        let mut today: std::collections::HashMap<String, (f64, u32)> = std::collections::HashMap::new();
        loop {
            std::thread::sleep(std::time::Duration::from_millis(120));
            let Some(now_fg) = guard::foreground() else { continue };
            if now_fg.hwnd == last.hwnd { continue; }
            let prev = std::mem::replace(&mut last, now_fg.clone());
            if prev.hwnd == 0 || prev.pid == now_fg.pid { continue; }
            let idle_ms = sys::idle_secs() * 1000.0;
            if guard::user_caused(idle_ms, guard::switch_key_down(), guard::cursor_over(now_fg.hwnd)) { continue; }
            let mut buf = Vec::new();
            let procs = sys::snapshot(&mut buf);
            let Some(p) = procs.iter().find(|x| x.pid == now_fg.pid) else { continue };
            if guard::is_system(&p.name) { continue; }
            let t = now_unix();
            if t - rules_at > 30.0 { rules_set = rules::load(&dir); rules_at = t; }
            // Who did it: for a console window, the newest console program; then a task, if one launched it.
            let culprit = if guard::is_console_host(&p.name) {
                let ft_now = (t * 1e7) as u64 + 116_444_736_000_000_000;
                let fresh = procs.iter().filter(|x| ft_now.saturating_sub(x.created) < 60_000_000 && !guard::is_console_host(&x.name) && !x.name.to_lowercase().starts_with("snifrig")).max_by_key(|x| x.created);
                match fresh {
                    Some(f) => {
                        let ch = chain_of(f.pid, &procs);
                        match guard::task_launch(&ch).and_then(|c| task_for_cmd(&dir, &c).map(|task| (task, c))) {
                            Some((task, c)) => guard::Culprit::Task(task, c.split_whitespace().next().unwrap_or("").trim_matches('"').rsplit('\\').next().unwrap_or("").to_string()),
                            None => guard::Culprit::App(ch.iter().rev().find(|(n, _)| !matches!(n.to_lowercase().as_str(), "svchost.exe" | "services.exe" | "wininit.exe" | "explorer.exe")).map(|(n, _)| n.clone()).unwrap_or_else(|| f.name.clone())),
                        }
                    }
                    None => continue, // a console you opened earlier, coming forward again
                }
            } else {
                let ch = chain_of(p.pid, &procs);
                match guard::task_launch(&ch).and_then(|c| task_for_cmd(&dir, &c).map(|task| (task, c))) {
                    Some((task, _)) => guard::Culprit::Task(task, p.name.clone()),
                    None => guard::Culprit::App(p.name.clone()),
                }
            };
            let key = culprit.key();
            let e = today.entry(key.clone()).or_insert((t, 0));
            if t - e.0 > 86400.0 { *e = (t, 0); }
            e.1 += 1;
            let count = e.1;
            // Told to stay out of the way: put you back right away.
            if let guard::Culprit::App(a) = &culprit {
                if rules::rule_no_focus(&rules_set, &a.to_lowercase()) {
                    if guard::give_back(prev.hwnd, now_fg.hwnd) { last = prev.clone(); }
                    let m = format!("kept {} from taking the screen (back to {})", a, if prev.title.is_empty() { "what you were doing".to_string() } else { prev.title.clone() });
                    ledger::record(&dir, &snifrig_fix::Alert { t: String::new(), key: "guard:popup".into(), msg: m.clone() }, &Action::Report, None, "done", &m);
                    continue;
                }
            }
            let m = format!("{} and took the screen from you{}", culprit.says(), if count > 1 { format!(" ({} times today)", count) } else { String::new() });
            ledger::record(&dir, &snifrig_fix::Alert { t: String::new(), key: "guard:popup".into(), msg: m.clone() }, &Action::Report, None, "noted", &m);
            if !popup_said_no(&dir, &key) {
                let who = match &culprit { guard::Culprit::Task(t, _) => t.clone(), guard::Culprit::App(a) => a.clone() };
                add_card(&dir, "popup_guard", &who, &format!("popup:{}", key), &format!("{} at {}. Do you want me to make it so this does not pop up and take control of the desktop?", m, local_hhmm(t)));
            }
        }
    });
}

/// Clean the chosen junk (moved to quarantine, undo for 7 days), then scan again. A category
/// you clean every time is offered as a weekly cleanup, run when you are away.
fn junk_clean(dir: &Path, ids: &[String]) -> Result<String, String> {
    use snifrig_fix::junk;
    let (moved, bytes, skipped, batch) = junk::clean(dir, ids);
    let _ = junk::scan(dir);
    let alert = snifrig_fix::Alert { t: String::new(), key: "junk:clean".into(), msg: String::new() };
    let msg = format!("cleaned {} ({} item{}){}; undo for 7 days: snifrig-fix junk undo {}", junk::human(bytes), moved, if moved == 1 { "" } else { "s" },
        if skipped > 0 { format!(", skipped {} in use", skipped) } else { String::new() }, batch);
    ledger::record(dir, &alert, &Action::Report, None, "done", &msg);
    // Learning: the same safe categories cleaned 3 times → offer to do it weekly.
    let hist = std::fs::read_to_string(dir.join(junk::BATCHES)).unwrap_or_default();
    let regular: Vec<&String> = ids.iter().filter(|id| !id.starts_with("build:") && hist.lines().filter(|l| l.split('\t').nth(3).map_or(false, |c| c.split(',').any(|x| x == id.as_str()))).count() >= 3).collect();
    let (jobs, _) = snifrig_fix::sched::load(dir);
    if !regular.is_empty() && !jobs.iter().any(|j| j.name == "snifrig weekly cleanup") {
        add_card(dir, "junk_auto", &regular.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(","), "junk_auto", "You clean the same things every time. Want snifrig to do it weekly, when you are away? It still keeps everything 7 days so you can undo.");
    }
    Ok(msg)
}

/// Link duplicate model copies: `which` is a group number from disk-report.json, or "all".
fn disk_dedupe(dir: &Path, which: &str) -> Result<String, String> {
    let r = std::fs::read_to_string(dir.join(snifrig_fix::disk::REPORT)).map_err(|_| "scan first: snifrig-fix disk scan".to_string())?;
    // Each group's file list, in order, from the report.
    let groups: Vec<Vec<std::path::PathBuf>> = r.split("\"files\":[").skip(1).map(|g| {
        g.split(']').next().unwrap_or("").split("\",\"").map(|f| std::path::PathBuf::from(f.trim_matches('"').replace("\\\\", "\\"))).filter(|p| !p.as_os_str().is_empty()).collect()
    }).collect();
    let pick: Vec<&Vec<std::path::PathBuf>> = if which == "all" { groups.iter().collect() } else { groups.get(which.parse::<usize>().map_err(|_| "a group number or all")?).into_iter().collect() };
    let (mut linked, mut freed, mut skipped) = (0, 0, 0);
    let mut batches = Vec::new();
    let mut asked = 0;
    for g in pick {
        let (l, f, s, b, questions) = snifrig_fix::disk::dedupe_checked(dir, g);
        linked += l; freed += f; skipped += s;
        if l > 0 { batches.push(b); }
        // Copies that differ inside: changed nothing, ask which one is right.
        for q in questions {
            asked += 1;
            let list: Vec<String> = q.iter().map(|(p, h)| format!("{} (checksum {}...){}", p.display(), &h[..h.len().min(12)], if snifrig_fix::disk::expected_sha(p).as_deref() == Some(h.as_str()) { ", proven right by its own checksum" } else if snifrig_fix::disk::expected_sha(p).is_some() { ", FAILS its own checksum" } else { "" })).collect();
            let paths: Vec<String> = q.iter().map(|(p, _)| p.display().to_string()).collect();
            add_card(dir, "model_differs", &paths.join("|"), &format!("differs:{}", paths.join("|")), &format!("These model files look alike but are different inside, so snifrig changed nothing: {}. Which one is right? snifrig then keeps that one and replaces the others with links to it (undo for 7 days).", list.join("; ")));
        }
    }
    let _ = snifrig_fix::disk::scan(dir);
    let msg = format!("checked every copy's checksum; linked {} identical model file{}, freed {}{}{}; undo for 7 days: snifrig-fix disk undo {}", linked, if linked == 1 { "" } else { "s" }, snifrig_fix::junk::human(freed),
        if skipped > 0 { format!(" ({} skipped: in use)", skipped) } else { String::new() },
        if asked > 0 { format!("; {} group{} differ inside, so snifrig asks which copy is right", asked, if asked == 1 { "" } else { "s" }) } else { String::new() }, batches.join(" "));
    ledger::record(dir, &snifrig_fix::Alert { t: String::new(), key: "disk:dedupe".into(), msg: msg.clone() }, &Action::Report, None, "done", &msg);
    Ok(msg)
}

// ---- Administrator helpers: frame measuring and driver installs ----

const HELPERS_SETUP_PS1: &str = include_str!("helpers-setup.ps1");
const DRIVERS_INSTALL_PS1: &str = include_str!("drivers-install.ps1");

/// C:\Program Files\snifrig: only administrators can change files there, so the scheduled tasks
/// that run these helpers with full rights cannot be pointed at something else.
fn admin_root() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into())).join("snifrig")
}
fn admin_state() -> std::path::PathBuf { admin_root().join("state") }
fn frames_now_file(dir: &Path) -> std::path::PathBuf {
    if admin_root().join("helpers").join("snifrig-frames.exe").exists() { admin_state().join("frames-now.txt") } else { dir.join("frames-now.txt") }
}
fn drivers_helper_on() -> bool { admin_root().join("helpers").join("drivers-install.ps1").exists() }

/// Set up the administrator helpers you want (the rest are removed). Windows asks you first.
fn helpers_setup(dir: &Path, frames: bool, drivers: bool) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let src = dir.join("helpers-src");
    std::fs::create_dir_all(&src).map_err(|e| e.to_string())?;
    if frames {
        let newer = dir.join("bin").join("snifrig-frames.exe.new");
        let exe = if newer.exists() { newer } else { dir.join("bin").join("snifrig-frames.exe") };
        if !exe.exists() { return Err(format!("{} is missing; install snifrig again", exe.display())); }
        std::fs::copy(&exe, src.join("snifrig-frames.exe")).map_err(|e| e.to_string())?;
    }
    std::fs::write(src.join("drivers-install.ps1"), DRIVERS_INSTALL_PS1).map_err(|e| e.to_string())?;
    let script = dir.join("helpers-setup.ps1");
    std::fs::write(&script, HELPERS_SETUP_PS1).map_err(|e| e.to_string())?;
    let out = dir.join("helpers-setup.out");
    let _ = std::fs::remove_file(&out);
    let want = [frames.then_some("frames"), drivers.then_some("drivers")].into_iter().flatten().collect::<Vec<_>>().join(",");
    let q = |p: &Path| p.display().to_string().replace('\'', "''");
    let wrapper = dir.join("helpers-setup-run.ps1");
    std::fs::write(&wrapper, format!("& '{}' -Src '{}' -Dir '{}' -Want '{}' -Out '{}'\r\n", q(&script), q(&src), q(dir), want, q(&out))).map_err(|e| e.to_string())?;
    let st = std::process::Command::new("powershell.exe").args(["-NoProfile", "-WindowStyle", "Hidden", "-Command"])
        .arg(format!("Start-Process powershell.exe -Verb RunAs -Wait -WindowStyle Hidden -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File','\"{}\"'", q(&wrapper)))
        .creation_flags(0x0800_0000).status().map_err(|e| e.to_string())?;
    let r = std::fs::read_to_string(&out).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&src);
    if !st.success() || r.is_empty() { return Err("the Windows confirmation was cancelled".into()); }
    if !r.trim_start_matches('\u{feff}').trim().starts_with("ok") { return Err(r.trim().to_string()); }
    let _ = std::fs::remove_file(dir.join("bin").join("snifrig-frames.exe.new"));
    if drivers { let _ = std::fs::write(dir.join("drivers-helper-on"), "1"); } else { let _ = std::fs::remove_file(dir.join("drivers-helper-on")); }
    Ok(())
}

/// `frames on|off`: real frame-rate measuring in games. Windows lets only an administrator read
/// a game's present events, so snifrig-frames runs from Program Files as a scheduled task with
/// highest privileges (Windows asks you once). Off removes it again.
fn frames_cmd(dir: &Path, on: bool) -> Result<String, String> {
    if on { let _ = std::fs::remove_file(dir.join("frames-off")); } else { let _ = std::fs::write(dir.join("frames-off"), "1"); }
    helpers_setup(dir, on, drivers_helper_on())?;
    if on {
        let _ = std::fs::write(dir.join("frames-on"), "1");
        Ok("Frame measuring is on: snifrig now sees real fps and hitches in Direct3D games (Vulkan and OpenGL games still use the CPU-based check).".into())
    } else { let _ = std::fs::remove_file(dir.join("frames-on")); Ok("Frame measuring is off.".into()) }
}

// ---- Drivers (see drivers.rs for the rules) ----

/// The last scan if it is from today, else a new one.
fn drivers_fresh(dir: &Path) -> Result<snifrig_fix::drivers::Scan, String> {
    match snifrig_fix::drivers::load(dir) {
        Some(s) if now_unix() - (s.unix as f64) < 6.0 * 3600.0 => Ok(s),
        _ => snifrig_fix::drivers::scan(dir),
    }
}

/// `drivers update all|ID..`: the updates that pass the rules go into the plan; snifrig installs
/// them the next time you are away (or now, with `now`).
fn drivers_update(dir: &Path, which: &[&str], now: bool) -> Result<String, String> {
    use snifrig_fix::drivers;
    let s = drivers_fresh(dir)?;
    let (ok, held) = drivers::sort(&s, drivers::gpu_choice(dir));
    let mine = drivers::held_by_you(dir);
    let pick: Vec<&drivers::Update> = ok.into_iter().filter(|u| (which.contains(&"all") && !mine.contains(&u.id)) || which.iter().any(|w| w.eq_ignore_ascii_case(&u.id))).collect();
    if pick.is_empty() {
        let why = held.iter().find(|(u, _)| which.iter().any(|w| w.eq_ignore_ascii_case(&u.id))).map(|(u, w)| format!("{} is held back: {}", u.title, w));
        return Err(why.unwrap_or_else(|| "no driver updates to install right now".into()));
    }
    if !drivers_helper_on() { helpers_setup(dir, dir.join("frames-on").exists(), true)?; }
    std::fs::write(dir.join("drivers-plan.txt"), pick.iter().map(|u| u.id.as_str()).collect::<Vec<_>>().join("\r\n")).map_err(|e| e.to_string())?;
    let _ = std::fs::write(dir.join("drivers-plan-titles.txt"), pick.iter().map(|u| u.title.as_str()).collect::<Vec<_>>().join("; "));
    let list = pick.iter().map(|u| u.title.as_str()).collect::<Vec<_>>().join("; ");
    if now { return drivers_run(dir); }
    let (mut jobs, _) = snifrig_fix::sched::load(dir);
    let exe = dir.join("bin").join("snifrig-fix.exe");
    let name = "snifrig driver updates";
    let mut j = snifrig_fix::sched::Job::new(name, &format!("\"{}\" --dir \"{}\" drivers run", exe.display(), dir.display()));
    j.sched = snifrig_fix::sched::Sched::Once(now_unix() + 60.0);
    j.when = snifrig_fix::sched::When::Idle;
    j.latest_secs = 7.0 * 86400.0;
    j.max_secs = 2.0 * 3600.0;
    jobs.retain(|x| x.name != name);
    jobs.push(j);
    snifrig_fix::sched::save(dir, &jobs);
    Ok(format!("snifrig installs these the next time you are away, one at a time after a restore point: {}", list))
}

/// Start the administrator helper on whatever plan or rollback file is waiting, and wait for it.
fn drivers_helper_run(_dir: &Path) -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    if !drivers_helper_on() { return Err("the driver helper is not set up; snifrig-fix drivers update sets it up (Windows asks once)".into()); }
    let start = now_unix() - 2.0;
    let st = std::process::Command::new("schtasks.exe").args(["/run", "/tn", "snifrig drivers"]).creation_flags(0x0800_0000).output().map_err(|e| e.to_string())?;
    if !st.status.success() { return Err(format!("could not start the driver helper: {}", String::from_utf8_lossy(&st.stderr).trim())); }
    let res = admin_state().join("drivers-result.txt");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(5));
        let r = std::fs::read_to_string(&res).unwrap_or_default();
        let r = r.trim_start_matches('\u{feff}').trim();
        if let Some((t, what)) = r.split_once(' ') { if t.parse::<f64>().map_or(false, |t| t >= start) { return Ok(what.to_string()); } }
        if now_unix() - start > 2.0 * 3600.0 { return Err("the driver helper did not finish in 2 hours".into()); }
    }
}

/// `drivers run`: install the plan now (the scheduler calls this when you are away).
fn drivers_run(dir: &Path) -> Result<String, String> {
    let plan = std::fs::read_to_string(dir.join("drivers-plan.txt")).map_err(|_| "nothing planned".to_string())?;
    let ids: Vec<String> = plan.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
    let titles = std::fs::read_to_string(dir.join("drivers-plan-titles.txt")).unwrap_or_default();
    let r = drivers_helper_run(dir);
    let _ = std::fs::remove_file(dir.join("drivers-plan.txt"));
    let _ = std::fs::remove_file(dir.join("drivers-plan-titles.txt"));
    let what = r?;
    // "installed N rolledback M reboot True"
    let f: Vec<&str> = what.split_whitespace().collect();
    let installed: u32 = f.get(1).and_then(|x| x.parse().ok()).unwrap_or(0);
    let bad: u32 = f.get(3).and_then(|x| x.parse().ok()).unwrap_or(0);
    let reboot = f.get(5).map_or(false, |x| x.eq_ignore_ascii_case("true"));
    if installed > 0 { snifrig_fix::drivers::write_watch(dir, &snifrig_fix::drivers::Watch { at: now_unix(), ids, titles: titles.clone(), before: None }); }
    let _ = snifrig_fix::drivers::scan(dir);
    let msg = match f.first().copied() {
        Some("norestore") => "Windows could not make a restore point, so no driver was installed. Turn on System Protection for C: and try again.".to_string(),
        Some("nothing") => "nothing was planned".to_string(),
        _ => format!("{} driver update{} installed{}{}.", installed, if installed == 1 { "" } else { "s" },
            if bad > 0 { format!("; {} put back right away because the device reported a problem", bad) } else { String::new() },
            if reboot { "; Windows needs a restart to finish (snifrig does not restart for you)" } else { "" }),
    };
    drivers_note(dir, &msg);
    Ok(msg)
}

/// `drivers rollback ID..|last`: take back driver updates snifrig installed.
fn drivers_rollback(dir: &Path, which: &[&str]) -> Result<String, String> {
    let ids: Vec<String> = if which == ["last"] { snifrig_fix::drivers::read_watch(dir).map(|w| w.ids).unwrap_or_default() } else { which.iter().map(|s| s.to_string()).collect() };
    if ids.is_empty() { return Err("no driver update to take back".into()); }
    std::fs::write(dir.join("drivers-rollback.txt"), ids.join("\r\n")).map_err(|e| e.to_string())?;
    let r = drivers_helper_run(dir);
    let _ = std::fs::remove_file(dir.join("drivers-rollback.txt"));
    let what = r?;
    snifrig_fix::drivers::hold(dir, &ids);
    let n: u32 = what.split_whitespace().nth(1).and_then(|x| x.parse().ok()).unwrap_or(0);
    let _ = std::fs::remove_file(dir.join(snifrig_fix::drivers::WATCH));
    remove_card(dir, "drivers_slower");
    let _ = snifrig_fix::drivers::scan(dir);
    let msg = format!("took back {} driver update{}; those devices use their previous drivers again", n, if n == 1 { "" } else { "s" });
    drivers_note(dir, &msg);
    Ok(msg)
}

fn drivers_note(dir: &Path, msg: &str) {
    let alert = snifrig_fix::Alert { t: String::new(), key: "drivers".into(), msg: msg.into() };
    ledger::record(dir, &alert, &Action::Report, None, "done", msg);
}

/// The daily check, while you are away: a card when manufacturer updates are ready.
fn drivers_check(dir: &Path) {
    use snifrig_fix::drivers;
    let Ok(s) = drivers::scan(dir) else { return };
    let held = drivers::held_by_you(dir);
    let (ok, _) = drivers::sort(&s, drivers::gpu_choice(dir));
    let ok: Vec<_> = ok.into_iter().filter(|u| !held.contains(&u.id)).collect();
    if ok.is_empty() || dir.join("drivers-plan.txt").exists() { remove_card(dir, "drivers_ready"); return; }
    let list = ok.iter().map(|u| u.title.as_str()).collect::<Vec<_>>().join("; ");
    add_card(dir, "drivers_ready", "snifrig", "drivers_ready", &format!("{} driver update{} from the manufacturer{} for your exact hardware: {}. Install {} the next time you are away? A restore point comes first, one driver at a time, and a device that has problems afterwards gets its old driver back.",
        ok.len(), if ok.len() == 1 { "" } else { "s" }, if ok.len() == 1 { "" } else { "s" }, list, if ok.len() == 1 { "it" } else { "them" }));
}

/// Your answer to "how did the game run?". Stutters: the programs that were busy during them
/// are turned down hardest in game mode from now on.
fn game_answer(dir: &Path, a: &str) -> Result<String, String> {
    let last = std::fs::read_to_string(dir.join(snifrig_fix::game::LAST)).unwrap_or_default();
    let mut lines = last.lines();
    let game = lines.next().unwrap_or("the game").to_string();
    let busy: Vec<String> = lines.next().unwrap_or("").split(',').map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty() && !snifrig_fix::guard::is_system(s)).collect();
    learn::append_feedback(dir, &Feedback::new(if a == "smooth" { "game_smooth" } else { "game_stutter" }, "game", a, &game.to_lowercase()));
    if a == "smooth" { return Ok(format!("Good. snifrig keeps game mode as it is for {}.", game)); }
    if busy.is_empty() { return Ok(format!("Thanks. snifrig did not catch who was busy during {}; next time it turns everything else down a bit harder and keeps watching.", game)); }
    let have = snifrig_fix::game::load_ease(dir);
    let add: Vec<&String> = busy.iter().filter(|b| !have.contains(*b)).collect();
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(snifrig_fix::game::EASE_FILE)) { for b in &add { let _ = writeln!(f, "{}", b); } }
    Ok(format!("Thanks. Next time you play, snifrig turns {} all the way down while the game is in front.", busy.join(" and ")))
}

/// Take a card away again (it was only valid for a moment, like the test question).
fn remove_card(dir: &Path, rule: &str) {
    let path = dir.join("ai-suggestions.json");
    let Ok(text) = std::fs::read_to_string(&path) else { return };
    let mark = format!("\"rule\":\"{}\"", rule);
    let Some(at) = text.find(&mark) else { return };
    let (Some(a), Some(b)) = (text[..at].rfind('{'), text[at..].find('}').map(|i| at + i + 1)) else { return };
    let mut s = format!("{}{}", &text[..a], &text[b..]);
    s = s.replace(",,", ",").replace("[,", "[").replace(",]", "]");
    rules::write_atomic(&path, &s);
}

fn local_hhmm(t: f64) -> String {
    let l = t as i64 + sys::local_offset_secs();
    format!("{:02}:{:02}", l.rem_euclid(86400) / 3600, l.rem_euclid(3600) / 60)
}

/// The task watcher: anything newly scheduled in Windows is moved into snifrig right away
/// (checked every 30 seconds, at the lowest priority). Not while Windows has the jobs back.
fn start_task_watch(dir: std::path::PathBuf) {
    use std::os::windows::process::CommandExt;
    let _ = std::thread::Builder::new().name("task-watch".into()).spawn(move || {
        let list = || -> Option<std::collections::HashSet<String>> {
            let o = std::process::Command::new("schtasks.exe").args(["/query", "/fo", "csv", "/nh"]).creation_flags(0x0800_0000 | 0x0000_0040).output().ok()?;
            Some(String::from_utf8_lossy(&o.stdout).lines().filter_map(|l| {
                let f: Vec<&str> = l.split("\",\"").collect();
                let name = f.first()?.trim_matches('"');
                (!name.starts_with("\\Microsoft\\") && name != "TaskName" && !l.contains("\"Disabled\"")).then(|| name.to_string())
            }).collect())
        };
        let mut seen = list();
        loop {
            std::thread::sleep(std::time::Duration::from_secs(30));
            if dir.join(snifrig_fix::sched::HANDED_BACK).exists() || dir.join("jobs-auto-off").exists() { seen = None; continue; }
            let Some(now) = list() else { continue };
            let fresh: Vec<&String> = match &seen { Some(s) => now.iter().filter(|n| !s.contains(*n)).collect(), None => Vec::new() };
            seen = Some(now.clone());
            if fresh.is_empty() { continue; }
            let Ok(report) = import_jobs(&dir, false) else { continue };
            for l in report.lines().filter(|l| l.starts_with("moved: ")) {
                let m = format!("a new scheduled task appeared and now runs under snifrig, with no window: {}", l.trim_start_matches("moved: "));
                ledger::record(&dir, &snifrig_fix::Alert { t: String::new(), key: "jobs:new".into(), msg: m.clone() }, &Action::Report, None, "done", &m);
            }
            seen = list();
        }
    });
}

fn jobs_cmd(dir: &Path, a: &[&str]) -> Result<String, String> {
    use snifrig_fix::sched;
    let find = |jobs: &[sched::Job], n: &str| jobs.iter().position(|j| j.name.eq_ignore_ascii_case(n)).ok_or_else(|| format!("no job named '{}' (see: snifrig-fix jobs)", n));
    match a {
        [] | ["list"] => {
            let (jobs, errs) = sched::load(dir);
            let status = std::fs::read_to_string(dir.join(sched::STATUS_FILE)).unwrap_or_default();
            let mut out = String::new();
            for j in &jobs {
                let w = status.split(&format!("\"name\":\"{}\"", snifrig_fix::json::esc(&j.name))).nth(1)
                    .and_then(|s| s.split("\"waiting\":\"").nth(1)).and_then(|s| s.split('"').next()).unwrap_or("");
                out.push_str(&format!("{}{} | {} | {}{}\n", j.name, if j.enabled { "" } else { " (paused)" }, sched::sched_text(&j.sched), sched::when_text(&j.when), if w.is_empty() { String::new() } else { format!(" | {}", w) }));
            }
            for e in errs { out.push_str(&format!("problem: {}\n", e)); }
            if jobs.is_empty() { out.push_str("no jobs yet. snifrig-fix jobs import moves your scheduled tasks into snifrig.\n"); }
            Ok(out.trim_end().to_string())
        }
        ["run", n] => {
            let _ = std::fs::remove_file(dir.join("job-run-result.txt"));
            std::fs::write(dir.join("job-run"), n).map_err(|e| e.to_string())?;
            for _ in 0..30 {
                std::thread::sleep(std::time::Duration::from_millis(200));
                if let Ok(r) = std::fs::read_to_string(dir.join("job-run-result.txt")) { return Ok(r); }
            }
            Ok("asked; the fixer starts it within a few seconds".into())
        }
        [op @ ("pause" | "resume"), n] => {
            let (mut jobs, _) = sched::load(dir);
            let i = find(&jobs, n)?;
            jobs[i].enabled = *op == "resume";
            sched::save(dir, &jobs);
            Ok(format!("{} {}", jobs[i].name, if *op == "resume" { "resumed" } else { "paused" }))
        }
        ["when", n, w] => {
            let (mut jobs, _) = sched::load(dir);
            let i = find(&jobs, n)?;
            jobs[i].when = match *w { "anytime" => sched::When::Anytime, "not-gaming" | "not gaming" => sched::When::NotGaming, "idle" => sched::When::Idle, _ => return Err("when is anytime, not-gaming or idle".into()) };
            sched::save(dir, &jobs);
            Ok(format!("{} now runs: {}", jobs[i].name, sched::when_text(&jobs[i].when)))
        }
        ["import"] => import_jobs(dir, true),
        ["preview"] => {
            let r = run_ps(dir, "jobs-import.ps1", JOBS_IMPORT_PS1, &["-Dir".into(), dir.display().to_string(), "-Preview".into()])?;
            Ok(format!("{}\n--- the jobs it would add (nothing was changed) ---{}", r.trim(), std::fs::read_to_string(dir.join("jobs-import-preview.txt")).unwrap_or_default()))
        }
        ["undo-import"] => undo_import(dir),
        ["handback"] => handback(dir, "you asked"),
        ["add-cleanup", ids] => {
            let (mut jobs, _) = sched::load(dir);
            let exe = dir.join("bin").join("snifrig-fix.exe");
            let mut j = sched::Job::new("snifrig weekly cleanup", &format!("\"{}\" --dir \"{}\" junk clean {}", exe.display(), dir.display(), ids.replace(',', " ")));
            j.sched = sched::Sched::Every(7.0 * 86400.0);
            j.when = sched::When::Idle;
            j.latest_secs = 3.0 * 86400.0;
            if !jobs.iter().any(|x| x.name == j.name) { jobs.push(j); }
            sched::save(dir, &jobs);
            Ok("snifrig cleans these weekly when you are away (undo still works for 7 days)".into())
        }
        ["handback-admin"] => {
            let f = dir.join("jobs-handback-denied.txt");
            if std::fs::read_to_string(&f).unwrap_or_default().trim().is_empty() { return Ok("nothing needs your OK".into()); }
            toggle_elevated(dir, "enable", &f).map(|r| format!("{}\nWindows runs these again.", r.trim()))
        }
        ["takeover"] => Ok(takeover(dir, true)),
        ["takeover-admin"] => {
            let f = dir.join("jobs-import-denied.txt");
            toggle_elevated(dir, "disable", &f).map(|r| format!("{}\nsnifrig runs these now.", r.trim()))
        }
        _ => Err("jobs [list] | run NAME | pause NAME | resume NAME | when NAME anytime|not-gaming|idle | import | undo-import".into()),
    }
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
    let slow: Vec<String> = gov.apps_slow.iter().map(|(n, ms, base, why)| format!("{{\"name\":\"{}\",\"ms\":{:.0},\"usual_ms\":{:.0},\"why\":\"{}\"}}", snifrig_fix::json::esc(n), ms, base, snifrig_fix::json::esc(why))).collect();
    let vis: Vec<String> = gov.visible.iter().map(|n| format!("\"{}\"", snifrig_fix::json::esc(n))).collect();
    let full = format!("{{\"unix\":{:.0},\"acting\":{},\"cpu\":{:.0},\"feel\":{},\"apps_slow\":[{}],\"visible\":[{}],{}", now_unix(), acting, gov.total_pct, feel, slow.join(","), vis.join(","), tail);
    let names: Vec<&str> = gov.apps_slow.iter().map(|a| a.0.as_str()).collect();
    (full, format!("{}{}{}", acting, names.join(","), tail))
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
    // Restarting the desktop can freeze a game (even a minimized one): never while one runs.
    if running_game(dir).is_some() { return; }
    let path = dir.join("ai-suggestions.json");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    if text.contains("\"restart_desktop\"") { return; }
    let reason = format!("The desktop compositor (dwm) has used {:.0}% of a core for over 3 minutes. That is what makes the mouse and every window stutter. Restarting it takes a second: the screen flashes and your apps stay open (games can freeze, so snifrig never offers this while one is running). Windows asks you to confirm.", pct);
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
        heavy_log(dir, "desktop", false);
        return Ok("ok, leaving the desktop alone".into());
    }
    if let Some(g) = running_game(dir) {
        ledger::record(dir, &alert, &action, Some(&t), "refused", &format!("{} is running; restarting the desktop can freeze it", g));
        return Err(format!("{} is running, and restarting the desktop can freeze it. Close the game first.", g));
    }
    // dwm belongs to the system: ending it needs admin, so Windows shows its own confirmation.
    use std::os::windows::process::CommandExt;
    let st = std::process::Command::new("powershell.exe").args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", "Start-Process taskkill.exe -ArgumentList '/f','/im','dwm.exe' -Verb RunAs -WindowStyle Hidden -Wait"])
        .creation_flags(0x0800_0000).status().map_err(|e| e.to_string())?;
    if !st.success() { ledger::record(dir, &alert, &action, Some(&t), "refused", "the Windows confirmation was cancelled"); return Err("cancelled".into()); }
    ledger::record(dir, &alert, &action, Some(&t), "done", "restarted the desktop compositor with your OK; Windows started it again");
    heavy_log(dir, "desktop", true);
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

/// `launch sync`: write Windows launch options (Image File Execution Options PerfOptions) for
/// rules with at_launch=yes, so the program starts at that priority instead of being caught a
/// second later. HKLM needs admin, so Windows asks you to confirm; nothing else is touched, and
/// entries snifrig wrote before but no rule wants any more are removed.
fn launch_sync(dir: &Path) -> Result<String, String> {
    let want: Vec<(String, Option<u32>, Option<u32>)> = rules::load(dir).into_iter()
        .filter(|r| r.at_launch && !r.pattern.contains(['*', '?']))
        .filter_map(|r| {
            let cpu = match r.priority { Some(sys::IDLE) => Some(1), Some(sys::BELOW_NORMAL) => Some(5), Some(sys::NORMAL) => Some(2), _ => None };
            let io = r.io.map(|v| v.min(2));
            (cpu.is_some() || io.is_some()).then_some((r.pattern, cpu, io))
        }).collect();
    let old: Vec<String> = ledger::read_list(dir, "launch-managed.txt").iter().filter_map(|l| l.split_whitespace().next().map(String::from)).collect();
    let names: Vec<String> = want.iter().map(|w| w.0.clone()).collect();
    let lines: Vec<String> = want.iter().map(|(n, c, i)| format!("{} {} {}", n, c.map_or("-".to_string(), |v| v.to_string()), i.map_or("-".to_string(), |v| v.to_string()))).collect();
    if old == names && want.is_empty() { return Ok("nothing to set at launch".into()); }
    let ok_name = |n: &str| n.ends_with(".exe") && n.chars().all(|c| c.is_ascii_alphanumeric() || "._- ".contains(c));
    let mut ps = vec!["$b='HKLM:\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\Image File Execution Options'".to_string()];
    for o in old.iter().filter(|o| !names.contains(o) && ok_name(o)) { ps.push(format!("Remove-Item \"$b\\{}\\PerfOptions\" -Recurse -ErrorAction SilentlyContinue", o)); }
    for (n, cpu, io) in want.iter().filter(|w| ok_name(&w.0)) {
        let k = format!("\"$b\\{}\\PerfOptions\"", n);
        ps.push(format!("New-Item {} -Force | Out-Null", k));
        for (prop, v) in [("CpuPriorityClass", cpu), ("IoPriority", io)] {
            ps.push(match v { Some(v) => format!("New-ItemProperty -Path {} -Name {} -PropertyType DWord -Value {} -Force | Out-Null", k, prop, v), None => format!("Remove-ItemProperty -Path {} -Name {} -ErrorAction SilentlyContinue", k, prop) });
        }
    }
    let script = dir.join("launch-sync.ps1");
    std::fs::write(&script, ps.join("\r\n")).map_err(|e| e.to_string())?;
    use std::os::windows::process::CommandExt;
    let cmd = format!("Start-Process powershell.exe -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File','{}' -Verb RunAs -Wait -WindowStyle Hidden", script.display().to_string().replace('\'', "''"));
    let st = std::process::Command::new("powershell.exe").args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &cmd]).creation_flags(0x0800_0000).status().map_err(|e| e.to_string())?;
    if !st.success() { return Err("the Windows confirmation was cancelled".into()); }
    let _ = std::fs::write(dir.join("launch-managed.txt"), lines.join("\n"));
    let msg = if names.is_empty() { "removed snifrig's launch settings".to_string() } else { format!("set at launch: {}", names.join(", ")) };
    let alert = snifrig_fix::Alert { t: String::new(), key: "rule:launch".into(), msg: msg.clone() };
    ledger::record(dir, &alert, &Action::Report, None, "done", &msg);
    Ok(msg)
}

/// Snifrig's own parts give way first: the local brain (OpenJev) is stopped when the graphics
/// card has been nearly full for two GPU readings in a row (about 5 minutes). Command in
/// brain-stop.txt (one line, run with cmd /c), default: podman stop open-jev in WSL Ubuntu.
fn stop_brain(dir: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let custom = std::fs::read_to_string(dir.join("brain-stop.txt")).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let st = match custom {
        Some(c) => std::process::Command::new("cmd.exe").args(["/c", &c]).creation_flags(0x0800_0000).status(),
        None => std::process::Command::new("wsl.exe").args(["-d", "Ubuntu", "--exec", "podman", "stop", "open-jev"]).creation_flags(0x0800_0000).status(),
    }.map_err(|e| e.to_string())?;
    if st.success() { Ok(()) } else { Err(format!("stop command exited with {}", st)) }
}

/// One round of "which app feels slow": measure every window on a screen, judge, announce new ones.
fn check_apps(dir: &Path, gov: &mut snifrig_fix::governor::Governor, rules: &[rules::Rule], tracker: &mut snifrig_fix::slowapp::Tracker) {
    use snifrig_fix::slowapp::{AppNow, Ctx};
    let mut by: Vec<(String, f64, bool)> = Vec::new();
    for (pid, ms, hung) in snifrig_fix::feel::apps() {
        let Some(name) = gov.procs().iter().find(|p| p.pid == pid).map(|p| p.name.clone()) else { continue };
        match by.iter_mut().find(|x| x.0.eq_ignore_ascii_case(&name)) { Some(x) => { x.1 = x.1.max(ms); x.2 |= hung; } None => by.push((name, ms, hung)) }
    }
    gov.visible = by.iter().map(|x| x.0.clone()).collect();
    let apps: Vec<(AppNow, Ctx)> = by.into_iter().map(|(name, ms, hung)| {
        let (cores, total) = gov.name_cores(&name);
        let a = AppNow { name: name.clone(), ms, hung, pct: gov.name_pct(&name), cores };
        let c = Ctx { total_cores: total, dwm_pct: gov.dwm_pct, total_pct: gov.total_pct,
            eased: gov.eased(&name).iter().map(|e| e.1).max(),
            rule_capped: rules.iter().any(|r| r.affinity.is_some() && rules::glob_match(&r.pattern, &name.to_lowercase())),
            hog: gov.top.iter().find(|(n, _)| !n.eq_ignore_ascii_case(&name) && !n.starts_with("snifrig")).map(|(n, _)| n.clone()) };
        (a, c)
    }).collect();
    for (name, why) in tracker.update(&apps) {
        let msg = format!("{} feels slow: {}", name.trim_end_matches(".exe"), why);
        let alert = snifrig_fix::Alert { t: String::new(), key: format!("app:slow:{}", name.to_lowercase()), msg: msg.clone() };
        ledger::record(dir, &alert, &Action::Report, None, "report", &msg);
    }
    gov.apps_slow = tracker.slow.clone();
}

/// You said this app feels slow (tray or `snifrig-fix slow NAME`): find out why and do what fits.
fn handle_slow(dir: &Path, gov: &mut snifrig_fix::governor::Governor, rules: &[rules::Rule], name: &str, act: bool) {
    let (cores, total) = gov.name_cores(name);
    let pct = gov.name_pct(name);
    let short = name.trim_end_matches(".exe").to_string();
    let mut done: Vec<String> = Vec::new();
    // 1. snifrig itself is holding it back: give it full speed now (and leave it alone from now on).
    let eased = gov.eased(name);
    if !eased.is_empty() {
        let mut changes = Vec::new();
        for (pid, _) in &eased { changes.extend(gov.undo(*pid, act)); }
        log_gov(dir, changes, act);
        done.push("gave it full speed back (snifrig had it eased)".into());
    }
    // 2. A rule pins it to a few cores: offer more cores, still in the background.
    let rule = rules.iter().find(|r| r.affinity.is_some() && rules::glob_match(&r.pattern, &name.to_lowercase()));
    // Only when it really is using (nearly) all the cores the rule allows; otherwise more cores would not help.
    let maxed = pct >= 70.0 * cores as f64;
    if let Some(r) = rule.filter(|_| cores < total && maxed) {
        let want = (cores * 2).min(total);
        let new_aff = format!("{}-{}", total - want, total - 1);
        let text: Vec<String> = r.text.split_whitespace().map(|t| if t.to_lowercase().starts_with("affinity=") { format!("affinity={}", new_aff) } else { t.to_string() }).collect();
        let reason = format!("{} is held to {} core{} by your rule and uses {:.0}% of {}. You said it feels slow: this gives it {} cores and keeps everything else in the rule.",
            short, cores, if cores == 1 { "" } else { "s" }, pct / cores.max(1) as f64, if cores == 1 { "it" } else { "them" }, want);
        add_card(dir, "rule", name, &text.join(" "), &reason);
        done.push(format!("asked you to allow {} cores instead of {} (card in the app)", want, cores));
    }
    // 3. Otherwise make room: ease busy background apps, this one kept at full speed.
    if eased.is_empty() && !(rule.is_some() && maxed && cores < total) {
        let changes = gov.quick(act, 120.0, Some(name));
        let n = changes.iter().filter(|c| c.result.is_ok()).count();
        log_gov(dir, changes, act);
        done.push(format!("eased {} busy background app{} to make room for it", n, if n == 1 { "" } else { "s" }));
    }
    learn::append_feedback(dir, &Feedback::new("app_slow", "feel", "", name));
    let msg = format!("you said {} feels slow ({:.0}% of a core, {} of {} cores allowed): {}", short, pct, cores, total, done.join("; "));
    let alert = snifrig_fix::Alert { t: String::new(), key: format!("app:you-slow:{}", name.to_lowercase()), msg: msg.clone() };
    ledger::record(dir, &alert, &Action::Report, None, "done", &msg);
    let _ = std::fs::write(dir.join("app-slow-result.txt"), &msg);
}

/// Add a card to the app's suggestions (kind rule|restart_desktop|...), shown with buttons.
fn add_card(dir: &Path, kind: &str, app: &str, rule: &str, reason: &str) {
    use snifrig_fix::json::esc;
    let path = dir.join("ai-suggestions.json");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    if text.contains(&format!("\"rule\":\"{}\"", esc(rule))) { return; }
    let now = now_unix();
    let item = format!("{{\"id\":\"snif-{:.0}\",\"unix\":{:.0},\"kind\":\"{}\",\"app\":\"{}\",\"rule\":\"{}\",\"setting\":\"\",\"reason\":\"{}\",\"source\":\"snifrig\"}}", now, now, esc(kind), esc(app), esc(rule), esc(reason));
    let body = match text.find("\"items\":[") {
        Some(i) => { let at = i + 9; let rest = &text[at..]; format!("{}{}{}{}", &text[..at], item, if rest.trim_start().starts_with(']') { "" } else { "," }, rest) }
        None => format!("{{\"items\":[{}]}}\n", item),
    };
    rules::write_atomic(&path, &body);
}

/// Write any panic (thread, message, where) to crash.log before the process ends, so a crash
/// is never silent again. Kept to the last ~64 KB.
fn crash_log(dir: &Path) {
    let p = dir.join("crash.log");
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current().name().unwrap_or("main").to_string();
        let msg = info.payload().downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| info.payload().downcast_ref::<String>().cloned()).unwrap_or_default();
        let loc = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        let line = format!("{:.0} snifrig-fix {} panicked at {}: {}\n", now_unix(), thread, loc, msg);
        if std::fs::metadata(&p).map(|m| m.len() > 65536).unwrap_or(false) { let _ = std::fs::remove_file(&p); }
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) { let _ = f.write_all(line.as_bytes()); }
    }));
}

/// Run a quick-fix request (from the tray, the hotkey, the phone or `snifrig-fix quick`).
fn run_quick(dir: &Path, gov: &mut snifrig_fix::governor::Governor, act: bool) {
    let changes = gov.quick(act, 120.0, None);
    let n = changes.iter().filter(|c| c.result.is_ok()).count();
    let names: Vec<String> = changes.iter().filter(|c| c.result.is_ok()).take(6).map(|c| c.name.clone()).collect();
    let mut freed = 0;
    if act {
        // Free memory of the eased apps nobody is using; harmless, nothing closes.
        for c in changes.iter().filter(|c| c.result.is_ok()) {
            if snifrig_fix::inuse::why_in_use(&c.name, now_unix()).is_some() { continue; }
            if let Some(t) = proc::target(c.pid) { if actions::run(&Action::Trim, &t).is_ok() { freed += 1; } }
        }
    }
    log_gov(dir, changes, act);
    let heavy = if act { check_heavy(dir, gov, now_unix(), true) } else { Vec::new() };
    if gov.dwm_pct >= 70.0 && heavy.is_empty() && heavy_trust(dir, "desktop") >= 2 {
        // You said yes to this twice: restart dwm now (Windows still asks you to confirm).
        let d = dir.to_path_buf();
        std::thread::spawn(move || { let _ = desktop(&d, true); });
    } else if gov.dwm_pct >= 50.0 && heavy.is_empty() { offer_desktop_restart(dir, gov.dwm_pct, now_unix()); }
    learn::append_feedback(dir, &Feedback::new("feel_slow", "feel", "", ""));
    let mut msg = format!("quick fix: eased {} busy background app{}{}, freed memory of {}; holding for 2 minutes, then easing back. Undo any of them in the app.",
        n, if n == 1 { "" } else { "s" }, if names.is_empty() { String::new() } else { format!(" ({})", names.join(", ")) }, freed);
    for h in &heavy { msg.push(' '); msg.push_str(h); }
    let alert = snifrig_fix::Alert { t: String::new(), key: "fix:quick".into(), msg: msg.clone() };
    ledger::record(dir, &alert, &Action::Report, None, "done", &msg);
    let _ = std::fs::write(dir.join("quick-result.txt"), &msg);
}

/// Phone remote: a small page on this PC's Wi-Fi, opened from the phone with a private key in
/// the link (`phone on` makes it, `phone off` ends it). Off unless you turn it on. Plain HTTP on
/// your own network for now; the key is checked on every request.
const PHONE_PORT: u16 = 8792;
const PHONE_PAGE: &str = include_str!("phone.html");

fn random_hex(n: usize) -> String {
    #[link(name = "advapi32", kind = "raw-dylib")]
    extern "system" { #[link_name = "SystemFunction036"] fn RtlGenRandom(buf: *mut u8, len: u32) -> u8; }
    let mut b = vec![0u8; n];
    unsafe { RtlGenRandom(b.as_mut_ptr(), n as u32); }
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

/// This PC's address on the local network (no packet is sent: a UDP socket only picks a route).
fn lan_ip() -> Option<std::net::IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.168.0.1:9").ok()?;
    s.local_addr().ok().map(|a| a.ip())
}

fn phone_cmd(dir: &Path, on: bool) -> Result<String, String> {
    let f = dir.join("phone.json");
    if !on { let _ = std::fs::remove_file(&f); return Ok("phone remote is off".into()); }
    let key = snifrig_fix::json::str_field(&std::fs::read_to_string(&f).unwrap_or_default(), "key").unwrap_or_else(|| random_hex(16));
    std::fs::write(&f, format!("{{\"key\":\"{}\",\"port\":{}}}", key, PHONE_PORT)).map_err(|e| e.to_string())?;
    let ip = lan_ip().map(|i| i.to_string()).unwrap_or_else(|| "this-pc".into());
    Ok(format!("http://{}:{}/?k={}", ip, PHONE_PORT, key))
}

fn phone_serve(dir: PathBuf) {
    use std::io::{Read, Write};
    std::thread::spawn(move || {
        let mut listener: Option<std::net::TcpListener> = None;
        let mut key = String::new();
        loop {
            let cfg = std::fs::read_to_string(dir.join("phone.json")).ok();
            match (&cfg, &listener) {
                (None, Some(_)) => { listener = None; }
                (Some(c), None) => {
                    key = snifrig_fix::json::str_field(c, "key").unwrap_or_default();
                    if key.len() >= 32 {
                        if let Ok(l) = std::net::TcpListener::bind(("0.0.0.0", PHONE_PORT)) { let _ = l.set_nonblocking(true); listener = Some(l); }
                    }
                }
                _ => {}
            }
            let Some(l) = &listener else { std::thread::sleep(std::time::Duration::from_secs(5)); continue };
            match l.accept() {
                Ok((mut s, _)) => {
                    let _ = s.set_nonblocking(false);
                    let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(3)));
                    let mut buf = [0u8; 4096];
                    let n = s.read(&mut buf).unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let line = req.lines().next().unwrap_or("");
                    let mut it = line.split_whitespace();
                    let (method, target) = (it.next().unwrap_or(""), it.next().unwrap_or("/"));
                    let (path, query) = target.split_once('?').unwrap_or((target, ""));
                    let param = |name: &str| query.split('&').find_map(|kv| kv.strip_prefix(&format!("{}=", name))).unwrap_or("").to_string();
                    let ok_key = { let k = param("k"); k.len() == key.len() && k.bytes().zip(key.bytes()).fold(0u8, |a, (x, y)| a | (x ^ y)) == 0 };
                    let (code, ctype, body) = if !ok_key { ("403 Forbidden", "text/plain", "wrong or missing key: scan the code in snifrig Settings again".to_string()) } else {
                        match (method, path) {
                            ("GET", "/") => ("200 OK", "text/html; charset=utf-8", PHONE_PAGE.to_string()),
                            ("GET", "/api/status") => {
                                let gov = std::fs::read_to_string(dir.join("governor.json")).unwrap_or_else(|_| "{}".into());
                                let pend = std::fs::read_to_string(dir.join("pending.json")).unwrap_or_else(|_| "{\"items\":[]}".into());
                                let live = now_unix() - snifrig_fix::json::num_field(&gov, "unix").unwrap_or(0.0) < 30.0;
                                let pc = std::env::var("COMPUTERNAME").unwrap_or_default();
                                ("200 OK", "application/json", format!("{{\"live\":{},\"pc\":\"{}\",\"gov\":{},\"pending\":{}}}", live, snifrig_fix::json::esc(&pc), gov.trim(), pend.trim()))
                            }
                            ("POST", "/api/quick") => {
                                let _ = std::fs::write(dir.join("quick-fix"), "phone");
                                ("200 OK", "application/json", "{\"msg\":\"Quick fix sent. Your PC should come back in a second or two.\"}".to_string())
                            }
                            ("POST", "/api/approve") | ("POST", "/api/dismiss") => {
                                let id = param("id");
                                let r = if path.ends_with("approve") { approve(&dir, &id) } else { dismiss(&dir, &id) };
                                let m = match r { Ok(m) => m, Err(e) => e };
                                ("200 OK", "application/json", format!("{{\"msg\":\"{}\"}}", snifrig_fix::json::esc(&m)))
                            }
                            _ => ("404 Not Found", "text/plain", "not found".to_string()),
                        }
                    };
                    let _ = write!(s, "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{}", code, ctype, body.len(), body);
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(150)),
            }
        }
    });
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
    phone_serve(dir.to_path_buf());
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
    // Your recurring jobs, run by snifrig: no windows, no pileups, at a good moment for the PC.
    let mut jobs = snifrig_fix::sched::Scheduler::new(dir);
    // Measured impact on overall performance: all-day meter plus random 2-minute holdouts (impact.rs).
    let mut impact = snifrig_fix::impact::Impact::new(dir);
    let tz = sys::local_offset_secs();
    let local_day = || ((now_unix() as i64) + tz).div_euclid(86400);
    snifrig_fix::impact::write_summary(dir, local_day());
    if dir.join(snifrig_fix::sched::HANDED_BACK).exists() { note(dir, "jobs", "done", &takeover(dir, false)); }
    start_guard(dir.to_path_buf());
    start_task_watch(dir.to_path_buf());
    let boot = sys::boot_unix();
    let offset = sys::local_offset_secs();
    for m in snifrig_fix::governor::recover(&dir.join("demoted.txt")) { note(dir, "gov:recover", "done", &m); }
    let mut slow_apps = snifrig_fix::slowapp::Tracker::default();
    // The game sniffer and game mode's learned list.
    let mut session: Option<snifrig_fix::game::Session> = None;
    let mut want_pid = 0u32;
    let mut drv_feel = snifrig_fix::drivers::Feel::default();
    let _ = std::fs::remove_file(dir.join("frames-want"));
    let mut game_ease = snifrig_fix::game::load_ease(dir);
    let (mut gpu_seen, mut gpu_full) = (0.0f64, 0u32);
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
        if stop.exists() {
            let _ = std::fs::remove_file(&stop);
            // Tells the tray's watchdog this stop was on purpose (an update, a pause): wait.
            let _ = std::fs::write(dir.join("fix-stopped"), format!("{:.0}", now_unix()));
            break;
        }
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
            if gov.acting { check_heavy(dir, &gov, now, false); }
            // A hidden game drawing full screen is the usual reason dwm is busy; only ask about
            // restarting dwm when that is not it.
            if dwm_hot > 0.0 && now - dwm_hot >= DWM_STUCK_SECS && hidden_game_window(&gov, &HEAVY.lock().map(|h| h.last_game.clone()).unwrap_or_default()).is_none() { offer_desktop_restart(dir, gov.dwm_pct, now); dwm_hot = now; }
        }
        if n % 30 == 20 {
            let g = std::fs::read_to_string(dir.join("gpu.json")).unwrap_or_default();
            let gu = snifrig_fix::json::num_field(&g, "unix").unwrap_or(0.0);
            if gu != gpu_seen {
                gpu_seen = gu;
                let (used, total) = (snifrig_fix::json::num_field(&g, "used_mb").unwrap_or(0.0), snifrig_fix::json::num_field(&g, "total_mb").unwrap_or(0.0));
                gpu_full = if total > 0.0 && used / total >= 0.92 { gpu_full + 1 } else { 0 };
                let brain_up = std::net::TcpStream::connect_timeout(&std::net::SocketAddr::from(([127, 0, 0, 1], 8791)), std::time::Duration::from_millis(200)).is_ok();
                if gpu_full >= 2 && brain_up && gov.acting {
                    gpu_full = 0;
                    let msg = match stop_brain(dir) {
                        Ok(()) => format!("the graphics card was {:.0}% full for 5 minutes, so snifrig stopped its own local brain (OpenJev) to give the memory back; it starts again at the next login if there is room", used / total * 100.0),
                        Err(e) => format!("tried to stop the local brain (graphics card {:.0}% full): {}", used / total * 100.0, e),
                    };
                    note(dir, "brain:gpu", "done", &msg);
                }
            }
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
            sugg.retain(|(n, _)| !kept(dir, n));
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
        {
            let t = now_unix();
            let sample = (n % 5 == 0).then(|| snifrig_fix::feel::recent(5)).filter(|f| f.samples > 0)
                .map(|f| (f.ms(), snifrig_fix::feel::sluggish_with(&f, gov.dwm_pct)));
            let wants_help = learner.user_feel(t) == Some(true) || t < gov.hold_until;
            if let Some((_, slow)) = sample { drv_feel.add(t, slow); }
            let now = snifrig_fix::impact::Now { now: t, local_day: local_day(), sample, fullscreen: sys::fullscreen_d3d(), user_wants_help: wants_help, idle_secs: sys::idle_secs() };
            if !paused && mode != Mode::Off {
                let o = impact.tick(dir, &now);
                if let Some(m) = o.note { note(dir, "impact", "done", &m); }
                match o.card {
                    Some(true) => add_card(dir, "impact_ask", "snifrig", "impact_ask", "Can I run a small test to see how your system is performing? For 2 minutes snifrig steps back and measures, then it gets back to work. And is your PC running slow right now? With this test I can optimize it."),
                    Some(false) => remove_card(dir, "impact_ask"),
                    None => {}
                }
                if o.felt_slow { learn::append_feedback(dir, &Feedback::new("feel_slow", "feel", "", "")); }
            }
        }
        let holding = impact.holding(now_unix());
        let mut act = false;
        if mode == Mode::Off || paused {
            if !gov.demoted.is_empty() { let a = gov.acting; log_gov(dir, gov.restore_all(a), a); }
            for m in perf.stop(gov.acting) { note(dir, "perf:mode", "done", &m); }
            gov.game = None;
        } else {
            // During the "without snifrig" block of an impact test, snifrig holds off.
            act = matches!(mode, Mode::Ask | Mode::Auto) && !holding;
            if act != gov.acting && !gov.demoted.is_empty() { let a = gov.acting; log_gov(dir, gov.restore_all(a), a); }
            if act != enf.acting { enf.reset(); }
            gov.acting = act;
            enf.acting = act;
            gov.feels_slow = snifrig_fix::feel::sluggish_with(&snifrig_fix::feel::recent(5), gov.dwm_pct);
            if n % 30 == 7 { game_ease = snifrig_fix::game::load_ease(dir); }
            gov.hints.game_front = gov.game.as_ref().filter(|_| gov.tuning.game_mode).map(|g| g.to_lowercase());
            // Full when the game is in front full screen, soft when it is in front in a window,
            // super soft when it runs but you are in another app.
            gov.hints.game_level = match &gov.hints.game_front {
                None => 0,
                Some(g) => {
                    let fg = sys::foreground_pid().and_then(|p| gov.procs().iter().find(|x| x.pid == p).map(|x| x.name.to_lowercase()));
                    if fg.as_deref() == Some(g.as_str()) { if sys::fullscreen_d3d() { 3 } else { 2 } } else { 1 }
                }
            };
            gov.hints.game_ease = game_ease.clone();
            gov.user_says = learner.user_feel(now_unix());
            if let Some(s) = gov.user_says { gov.feels_slow = s; }
            let changes = gov.tick(now_unix(), act);
            if !changes.is_empty() || n % 30 == 3 { gov.save_demoted(&dir.join("demoted.txt")); }
            let brain_game = |name: &str| brain.cache().get(name).map_or(false, |v| v.kind == "game" && v.confidence >= snifrig_fix::brain::MIN_CONF);
            for m in perf.tick(gov.procs(), sys::foreground_pid(), &rule_set, &gov.tuning, act, &brain_game) {
                note(dir, "perf:mode", if act { "done" } else { "would" }, &m);
            }
            gov.game = perf.game.as_ref().map(|g| g.2.clone());
            // Game sniffer: record while you play; ask how it ran when a full-screen game ends.
            match (&gov.game, session.is_some()) {
                (Some(g), _) => {
                    let s = session.get_or_insert_with(|| snifrig_fix::game::Session::new(g, now_unix()));
                    let (top, top_pct) = gov.top.iter().find(|(nm, _)| !nm.eq_ignore_ascii_case(g) && nm != "system" && nm != "idle").cloned().unwrap_or_default();
                    let avail = sys::memory_status().map(|m| m.1).unwrap_or(0.0);
                    // Real frame times, if you turned frame measuring on: ask the helper for the game in front.
                    let gpid = if gov.hints.game_level >= 2 { sys::foreground_pid().unwrap_or(0) } else { 0 };
                    let frames = if dir.join("frames-on").exists() && gpid != 0 {
                        if want_pid != gpid { let _ = std::fs::write(dir.join("frames-want"), gpid.to_string()); want_pid = gpid; }
                        std::fs::read_to_string(frames_now_file(dir)).ok().and_then(|l| {
                            let f: Vec<&str> = l.split_whitespace().collect();
                            let fresh = f.first().and_then(|x| x.parse::<f64>().ok()).map_or(false, |t| now_unix() - t <= 3.0);
                            (f.len() >= 6 && fresh && f[1] == gpid.to_string()).then(|| (f[2].parse().unwrap_or(0.0), f[4].parse().unwrap_or(0.0), f[5].parse().unwrap_or(0)))
                        })
                    } else { if want_pid != 0 { let _ = std::fs::remove_file(dir.join("frames-want")); want_pid = 0; } None };
                    s.add(&snifrig_fix::game::Sample { t: now_unix(), game_pct: gov.name_pct(g), total: gov.total_pct, top, top_pct, avail_mb: avail, fullscreen: sys::fullscreen_d3d(), frames });
                }
                (None, true) => {
                    if want_pid != 0 { let _ = std::fs::remove_file(dir.join("frames-want")); want_pid = 0; }
                    let s = session.take().unwrap();
                    let c = s.culprits();
                    let line = format!("{{\"game\":\"{}\",\"start\":{:.0},\"mins\":{:.0},\"full_screen_mins\":{:.0},\"stutters\":{},\"busy\":\"{}\"}}",
                        snifrig_fix::json::esc(&s.game), s.start, s.secs / 60.0, s.fs_secs / 60.0, s.hitches.len(), snifrig_fix::json::esc(&c.iter().map(|(n, k)| format!("{} {}", n, k)).collect::<Vec<_>>().join(", ")));
                    use std::io::Write;
                    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(snifrig_fix::game::SESSIONS)) { let _ = writeln!(f, "{}", line); }
                    if s.worth_asking() {
                        let _ = std::fs::write(dir.join(snifrig_fix::game::LAST), format!("{}\n{}\n", s.game, c.iter().take(3).map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(",")));
                        add_card(dir, "game_ask", &s.game, &format!("game:{:.0}", s.start), &s.question());
                    }
                }
                _ => {}
            }
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
        if dir.join("quick-fix").exists() { let _ = std::fs::remove_file(dir.join("quick-fix")); run_quick(dir, &mut gov, act); }
        if let Ok(name) = std::fs::read_to_string(dir.join("job-run")) {
            let _ = std::fs::remove_file(dir.join("job-run"));
            let r = jobs.run_now(name.trim(), now_unix());
            let _ = std::fs::write(dir.join("job-run-result.txt"), r.unwrap_or_else(|e| e));
        }
        if n % 60 == 30 {
            // After driver updates: did things get slower? Then offer to take them back.
            if let Some(mut w) = snifrig_fix::drivers::read_watch(dir) {
                let t = now_unix();
                match w.before {
                    None => { w.before = Some(drv_feel.rate(w.at - 86400.0, w.at)); snifrig_fix::drivers::write_watch(dir, &w); }
                    Some(b) if t - w.at > 86400.0 => {
                        let after = drv_feel.rate(w.at, t);
                        if snifrig_fix::drivers::worse(b, after) {
                            add_card(dir, "drivers_slower", "snifrig", "drivers_slower", &format!("Since the driver update{} ({}), your PC felt slow {:.0}% of the time, up from {:.0}% the day before. Take {} back?",
                                if w.ids.len() == 1 { "" } else { "s" }, w.titles, after.1 * 100.0, b.1 * 100.0, if w.ids.len() == 1 { "it" } else { "them" }));
                        } else if t - w.at > 3.0 * 86400.0 { let _ = std::fs::remove_file(dir.join(snifrig_fix::drivers::WATCH)); }
                    }
                    _ => {}
                }
            }
        }
        if n % 3600 == 2700 && sys::idle_secs() > 600.0 && gov.game.is_none() && !sys::fullscreen_d3d() {
            let fresh = snifrig_fix::drivers::load(dir).map_or(false, |s| now_unix() - (s.unix as f64) < 20.0 * 3600.0);
            if !fresh && license::status(dir).licensed { let d = dir.to_path_buf(); std::thread::spawn(move || drivers_check(&d)); }
        }
        if n % 3600 == 900 { let f = snifrig_fix::junk::purge_old(dir); if f > 0 { note(dir, "junk:purge", "done", &format!("emptied {} of cleanups older than 7 days from quarantine", snifrig_fix::junk::human(f))); } }
        if n % 3600 == 1800 && !license::status(dir).licensed {
            note(dir, "jobs", "done", &handback(dir, "the snifrig fix licence ended").unwrap_or_else(|e| e));
            break;
        }
        if n % 5 == 2 {
            let ctx = snifrig_fix::sched::Ctx { now: now_unix(), boot, gaming: gov.game.is_some() || sys::fullscreen_d3d(), cpu_pct: gov.total_pct, idle_secs: sys::idle_secs(), local_offset: offset };
            for m in jobs.tick(&ctx) { note(dir, "jobs", "failed", &m); }
        }
        if let Ok(name) = std::fs::read_to_string(dir.join("app-slow")) {
            let _ = std::fs::remove_file(dir.join("app-slow"));
            let name = name.trim().to_string();
            if !name.is_empty() { handle_slow(dir, &mut gov, &rule_set, &name, act); }
        }
        if n % 3 == 1 { check_apps(dir, &mut gov, &rule_set, &mut slow_apps); }
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
    let exempt = help || matches!(cmd.as_slice(), ["license", "install", _] | ["license"] | ["license", "status"] | ["license", "accept"] | ["machine-id"] | ["jobs", "handback"] | ["jobs", "handback-admin"]);
    if !exempt {
        if let Err(why) = gate(&dir) {
            // No valid licence: the fixer cannot run your jobs, so Windows takes them back.
            if cmd.is_empty() { let _ = handback(&dir, "snifrig fix is not licensed on this PC"); }
            eprintln!("{}\n{}", DENIED, why);
            std::process::exit(2);
        }
    }
    // Every command that fails is recorded (Activity in the window, fixes.jsonl), not just shown once.
    // The command's name, without ids or paths ("ease", "drivers update", "jobs run").
    let what = cmd.iter().take(2).enumerate().filter(|(i, w)| *i == 0 || w.chars().all(|c| c.is_ascii_lowercase() || c == '-')).map(|(_, w)| *w).collect::<Vec<_>>().join(" ");
    let out = |r: Result<String, String>| match r {
        Ok(m) => println!("{}", m),
        Err(e) => {
            if !what.is_empty() {
                let alert = snifrig_fix::Alert { t: String::new(), key: format!("cmd:{}", what), msg: e.clone() };
                ledger::record(&dir, &alert, &Action::Report, None, "failed", &format!("{} failed: {}", what, e));
            }
            eprintln!("{}", e);
            std::process::exit(1)
        }
    };
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
        ["ease", id] => out(ease(&dir, id, None, None)),
        ["ease", id, name, pid] => out(ease(&dir, id, Some(name), pid.parse().ok())),
        ["feel", w @ ("slow" | "fine")] => out(feel_cmd(&dir, *w == "slow")),
        ["desktop", w @ ("restart" | "no")] => out(desktop(&dir, *w == "restart")),
        ["unlearn", name, action] => out(unlearn(&dir, name, action)),
        ["launch", "sync"] => out(launch_sync(&dir)),
        ["slow", name] => { let _ = std::fs::write(dir.join("app-slow"), name); println!("noted; the fixer looks at {} within a second", name); }
        ["quick"] => { let _ = std::fs::write(dir.join("quick-fix"), "cli"); println!("quick fix asked; the fixer runs it within a second"); }
        ["phone", w @ ("on" | "off")] => out(phone_cmd(&dir, *w == "on")),
        ["mode", m] => match Mode::parse(m) {
            Some(m) => { ledger::write_mode(&dir, m); println!("fixer mode: {}", m.as_str()); }
            None => out(Err("mode must be off, dry-run, ask or auto".into())),
        },
        ["status"] => {
            let p = policy(&dir);
            let l = license::status(&dir);
            println!("mode: {}", p.mode.as_str());
            println!("paused: {}", if p.now < p.paused_until { format!("yes, {:.0} min left", (p.paused_until - p.now) / 60.0) } else { "no".into() });
            println!("license: {}", if l.licensed { format!("{} ({} seats{}, valid until {})", l.licensee, l.seats, if l.plan.is_empty() { String::new() } else { format!(", {}", l.plan) }, l.not_after) } else { format!("none ({})", l.note) });
            if let Some(id) = license::machine_hash() { println!("this PC: {}", id); }
            println!("pending: {}", ledger::load_pending(&dir).len());
        }
        ["pending"] => for p in ledger::load_pending(&dir) { println!("{}  {} {}#{}  {}", p.id, p.action, p.name, p.pid, p.why); },
        ["approve", id] => out(approve(&dir, id)),
        ["dismiss", id] => out(dismiss(&dir, id)),
        ["keep", name] => out(keep_cmd(&dir, name)),
        ["jobs", rest @ ..] => out(jobs_cmd(&dir, rest)),
        ["impact"] => {
            let s = snifrig_fix::impact::write_summary(&dir, ((now_unix() as i64) + sys::local_offset_secs()).div_euclid(86400));
            println!("{}", snifrig_fix::impact::sentence(&s));
            println!("with snifrig: {:.0} ms average, sluggish {:.1}% of the time ({:.1} hours measured)", s.on.mean(), s.on.slow_pct(), s.on.n as f64 * 5.0 / 3600.0);
            println!("snifrig holding off: {:.0} ms average, sluggish {:.1}% of the time ({:.1} minutes measured)", s.off.mean(), s.off.slow_pct(), s.off.n as f64 * 5.0 / 60.0);
            println!("days: {}; every holdout: {}", dir.join(snifrig_fix::impact::DAYS_FILE).display(), dir.join(snifrig_fix::impact::HOLDS_LOG).display());
        }
        ["impact", "answer", a @ ("yes" | "slow" | "no")] => { let _ = std::fs::write(dir.join(snifrig_fix::impact::ANSWER_FILE), a); println!("thanks"); }
        ["game", "answer", a @ ("smooth" | "stutter" | "bad")] => out(game_answer(&dir, a)),
        ["frames", op @ ("on" | "off")] => out(frames_cmd(&dir, *op == "on")),
        ["disk", "scan"] => { let (d, v) = snifrig_fix::disk::scan(&dir); println!("duplicates: {} to free; virtual disks: {} to give back", snifrig_fix::junk::human(d), snifrig_fix::junk::human(v)); }
        ["disk", "dedupe", which] => out(disk_dedupe(&dir, which)),
        ["disk", "keep", path] => {
            // You picked the right copy of a model whose copies differ.
            let r = std::fs::read_to_string(dir.join(snifrig_fix::disk::REPORT)).unwrap_or_default();
            let group: Vec<std::path::PathBuf> = r.split("\"files\":[").skip(1).map(|g| g.split(']').next().unwrap_or("").split("\",\"").map(|f| std::path::PathBuf::from(f.trim_matches('"').replace("\\\\", "\\"))).collect::<Vec<_>>())
                .find(|g| g.iter().any(|f| f.as_os_str().to_string_lossy().eq_ignore_ascii_case(path))).unwrap_or_default();
            if group.is_empty() { out(Err("that file is not in the last disk scan".into())); }
            let (l, f, s, b) = snifrig_fix::disk::keep_this(&dir, std::path::Path::new(path), &group);
            let _ = snifrig_fix::disk::scan(&dir);
            out(Ok(format!("kept {}; {} other cop{} now link to it, freed {}{}; undo for 7 days: snifrig-fix disk undo {}", path, l, if l == 1 { "y" } else { "ies" }, snifrig_fix::junk::human(f), if s > 0 { format!(" ({} in use, left)", s) } else { String::new() }, b)));
        }
        ["disk", "undo", batch] => out(snifrig_fix::disk::undo_links(&dir, batch).map(|n| format!("put back {} separate cop{}", n, if n == 1 { "y" } else { "ies" }))),
        ["disk", "compact", distro] => out(snifrig_fix::disk::wsl_compact(distro)),
        ["disk", "compact-when-away", distro] => {
            let (mut jobs, _) = snifrig_fix::sched::load(&dir);
            let exe = dir.join("bin").join("snifrig-fix.exe");
            let name = format!("snifrig shrink {}", distro);
            let mut j = snifrig_fix::sched::Job::new(&name, &format!("\"{}\" --dir \"{}\" disk compact {}", exe.display(), dir.display(), distro));
            j.sched = snifrig_fix::sched::Sched::Once(now_unix() + 60.0);
            j.when = snifrig_fix::sched::When::Idle;
            j.latest_secs = 7.0 * 86400.0;
            j.max_secs = 3600.0;
            jobs.retain(|x| x.name != name);
            jobs.push(j);
            snifrig_fix::sched::save(&dir, &jobs);
            println!("snifrig shrinks {} the next time you are away (it restarts that WSL distribution)", distro);
        }
        ["junk", "scan"] => { let t = snifrig_fix::junk::scan(&dir); println!("found {} you could free", snifrig_fix::junk::human(t)); }
        ["junk", "clean", ids @ ..] if !ids.is_empty() => out(junk_clean(&dir, &ids.iter().map(|s| s.to_string()).collect::<Vec<_>>())),
        ["junk", "undo"] | ["junk", "undo", _] => {
            let b = cmd.get(2).copied().unwrap_or("last");
            out(snifrig_fix::junk::restore(&dir, b).map(|(back, failed)| { let _ = snifrig_fix::junk::scan(&dir); format!("put back {} item{}{}", back, if back == 1 { "" } else { "s" }, if failed > 0 { format!(" ({} could not go back: something is in their place now)", failed) } else { String::new() }) }))
        }
        ["junk", "empty-bin"] => out(if snifrig_fix::junk::empty_recycle_bin() { let _ = snifrig_fix::junk::scan(&dir); Ok("the Recycle Bin is empty".into()) } else { Err("Windows did not empty the Recycle Bin".into()) }),
        ["drivers"] | ["drivers", "scan"] => out(snifrig_fix::drivers::scan(&dir).map(|s| snifrig_fix::drivers::summary(&s, snifrig_fix::drivers::gpu_choice(&dir)).join("\n"))),
        ["drivers", "gpu", c @ ("newest" | "stay")] => { let _ = std::fs::write(dir.join(snifrig_fix::drivers::GPU_CHOICE), c); println!("{}", if *c == "stay" { "snifrig keeps your current graphics driver" } else { "snifrig keeps your graphics driver up to date" }); }
        ["drivers", "update", "now", ids @ ..] if !ids.is_empty() => out(drivers_update(&dir, ids, true)),
        ["drivers", "update", ids @ ..] if !ids.is_empty() => out(drivers_update(&dir, ids, false)),
        ["drivers", "run"] => out(drivers_run(&dir)),
        ["drivers", "rollback", ids @ ..] if !ids.is_empty() => out(drivers_rollback(&dir, ids)),
        ["drivers", "keep"] => { let _ = std::fs::remove_file(dir.join(snifrig_fix::drivers::WATCH)); println!("ok, the driver updates stay"); }
        ["drivers", "not-now"] => { let s = snifrig_fix::drivers::load(&dir).unwrap_or_default(); snifrig_fix::drivers::hold(&dir, &s.updates.iter().map(|u| u.id.clone()).collect::<Vec<_>>()); println!("ok, snifrig will not offer these updates again (newer ones are offered)"); }
        ["drivers", "status"] => { let l = std::fs::read_to_string(admin_state().join("drivers-install.log")).unwrap_or_default(); let v: Vec<&str> = l.lines().collect(); println!("{}", if v.is_empty() { "no driver installs yet".to_string() } else { v[v.len().saturating_sub(15)..].join("\n") }); }
        ["drivers", "check"] => drivers_check(&dir),
        ["frames"] | ["frames", "status"] => println!("{}", if dir.join("frames-on").exists() { "frame measuring is on (snifrig-frames runs from the scheduled task 'snifrig frames')" } else { "frame measuring is off. snifrig-fix frames on turns it on (Windows asks once)." }),
        ["impact", "test"] => { let _ = std::fs::write(dir.join(snifrig_fix::impact::NOW_FILE), "1"); println!("asked: snifrig holds off for 2 minutes now, to compare"); }
        ["impact", "off"] => { let _ = std::fs::write(dir.join(snifrig_fix::impact::OFF_FILE), "1"); println!("impact tests are off"); }
        ["impact", "on"] => { let _ = std::fs::remove_file(dir.join(snifrig_fix::impact::OFF_FILE)); println!("impact tests are on"); }
        ["popup", yn @ ("yes" | "no"), who @ ..] => out(popup_cmd(&dir, *yn == "yes", &who.join(" "))),
        ["heavy", kind, yn @ ("yes" | "no"), app] => out(heavy_cmd(&dir, kind, *yn == "yes", app)),
        ["unkeep", name] => out(unkeep_cmd(&dir, name)),
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
        [] => {
            // One fixer at a time (the tray's watchdog restarts it if it dies).
            let name: Vec<u16> = "Global\\snifrig-fix".encode_utf16().chain(Some(0)).collect();
            let _m = unsafe { windows_sys::Win32::System::Threading::CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
            if unsafe { windows_sys::Win32::Foundation::GetLastError() } == 183 { eprintln!("snifrig-fix is already running."); std::process::exit(1); }
            crash_log(&dir);
            watch(&dir)
        }
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
