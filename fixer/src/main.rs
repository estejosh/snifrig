//! snifrig-fix: reads the monitor's alerts and fixes what it safely can.
//!   snifrig-fix [--dir D]                 watch loop (every 30 s); stops when D\stop-fix exists
//!   snifrig-fix once                      process new alerts once and exit
//!   snifrig-fix mode off|dry-run|ask|auto set the mode (default dry-run)
//!   snifrig-fix status                    mode, pause, license, pending count
//!   snifrig-fix pending                   list fixes waiting for approval
//!   snifrig-fix approve ID | dismiss ID   act on a pending fix
//!   snifrig-fix license install PATH      install a license key

use snifrig_fix::{actions, ledger, license, now_unix, plan, policy::Policy, proc, Action, Mode, Subject, Target, Verdict};
use std::path::{Path, PathBuf};

fn default_dir() -> PathBuf {
    std::env::var("LOCALAPPDATA").map(|p| PathBuf::from(p).join("snifrig")).unwrap_or_else(|_| PathBuf::from("data"))
}

fn policy(dir: &Path) -> Policy {
    Policy {
        mode: ledger::read_mode(dir),
        now: now_unix(),
        paused_until: ledger::paused_until(dir),
        licensed: license::status(dir).licensed,
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

fn process_once(dir: &Path) -> usize {
    let (alerts, off) = ledger::new_alerts(dir, ledger::read_offset(dir));
    let mut acted = 0;
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
            let action = refine(&it.action, t);
            // Fresh policy per target so the auto rate limit sees actions taken earlier in this pass.
            let p = policy(dir);
            match p.decide(&action, Some(t)) {
                Verdict::Execute => {
                    let (status, note) = match actions::run(&action, t) {
                        Ok(m) => ("done", format!("auto: {}; {}", m, it.why)),
                        Err(e) => ("failed", format!("auto: {}", e)),
                    };
                    ledger::record(dir, a, &action, Some(t), status, &note);
                    acted += 1;
                }
                Verdict::Queue => {
                    if let Some(id) = ledger::queue(dir, a, &action, t, &it.why) {
                        ledger::record(dir, a, &action, Some(t), "queued", &format!("pending id {}; {}", id, it.why));
                    }
                }
                Verdict::LogOnly(r) => ledger::record(dir, a, &action, Some(t), "would", &format!("{}; {}", r, it.why)),
                Verdict::Refuse(r) => ledger::record(dir, a, &action, Some(t), "refused", &r),
            }
        }
    }
    ledger::write_offset(dir, off);
    acted
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
    r
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
    Ok(format!("dismissed {} on {}", item.action, item.name))
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let flag = |n: &str| a.iter().position(|x| x == n).and_then(|i| a.get(i + 1)).cloned();
    let dir = flag("--dir").map(PathBuf::from).unwrap_or_else(default_dir);
    let _ = std::fs::create_dir_all(&dir);
    let cmd: Vec<&str> = a.iter().map(|s| s.as_str()).filter(|s| !s.starts_with("--")).collect();
    let cmd: Vec<&str> = match flag("--dir") { Some(d) => cmd.into_iter().filter(|s| *s != d).collect(), None => cmd };
    let out = |r: Result<String, String>| match r { Ok(m) => println!("{}", m), Err(e) => { eprintln!("{}", e); std::process::exit(1) } };
    match cmd.as_slice() {
        ["mode", m] => match Mode::parse(m) {
            Some(m) => { ledger::write_mode(&dir, m); println!("fixer mode: {}", m.as_str()); }
            None => out(Err("mode must be off, dry-run, ask or auto".into())),
        },
        ["status"] => {
            let p = policy(&dir);
            let l = license::status(&dir);
            println!("mode: {}", p.mode.as_str());
            println!("paused: {}", if p.now < p.paused_until { format!("yes, {:.0} min left", (p.paused_until - p.now) / 60.0) } else { "no".into() });
            println!("license: {}", if l.licensed { format!("{} ({} computers, expires {})", l.holder, l.computers, l.expires) } else { format!("none ({})", l.note) });
            println!("pending: {}", ledger::load_pending(&dir).len());
        }
        ["pending"] => for p in ledger::load_pending(&dir) { println!("{}  {} {}#{}  {}", p.id, p.action, p.name, p.pid, p.why); },
        ["approve", id] => out(approve(&dir, id)),
        ["dismiss", id] => out(dismiss(&dir, id)),
        ["license", "install", path] => out(license::install(&dir, Path::new(path))),
        ["license"] | ["license", "status"] => { let l = license::status(&dir); println!("{}", if l.licensed { format!("licensed to {}", l.holder) } else { l.note }); }
        ["once"] => { let n = process_once(&dir); println!("processed; {} automatic actions", n); }
        [] => loop {
            let stop = dir.join("stop-fix");
            if stop.exists() { let _ = std::fs::remove_file(&stop); break; }
            process_once(&dir);
            for _ in 0..60 { if stop.exists() { break; } std::thread::sleep(std::time::Duration::from_millis(500)); }
        },
        _ => out(Err("usage: snifrig-fix [once|status|pending|mode M|approve ID|dismiss ID|license install PATH] [--dir D]".into())),
    }
}
