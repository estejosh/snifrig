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

fn process_once(dir: &Path) -> usize { process_once_ex(dir).0 }

/// Like process_once, and also returns what ran: (action kind, lowercase name, metric it was meant to help).
fn process_once_ex(dir: &Path) -> (usize, Vec<(String, String, Trigger)>) {
    let (alerts, off) = ledger::new_alerts(dir, ledger::read_offset(dir));
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
                    if status == "done" { ran.push((learn::action_kind(&action.label()), t.name.to_lowercase(), learn::trigger_for(&action.label(), &a.msg))); }
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
        let mut f = Feedback::new("approve", "fix", &learn::action_kind(&action.label()), &t.name);
        f.trigger = learn::trigger_for(&action.label(), &item.msg).as_str().into();
        f.start = true;
        learn::append_feedback(dir, &f);
    }
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
    learn::append_feedback(dir, &Feedback::new("dismiss", "fix", &learn::action_kind(&action.label()), &item.name));
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
    let mut d: Vec<_> = gov.demoted.values().collect();
    d.sort_by_key(|x| x.pid);
    let demoted: Vec<String> = d.iter().map(|x| format!("{{\"pid\":{},\"name\":\"{}\",\"level\":{},\"reason\":\"{}\",\"since\":{:.0}}}", x.pid, snifrig_fix::json::esc(&x.name), x.level, snifrig_fix::json::esc(&x.reason), x.since)).collect();
    let sg: Vec<String> = sugg.iter().map(|(n, c)| format!("{{\"name\":\"{}\",\"count\":{}}}", snifrig_fix::json::esc(n), c)).collect();
    let tail = format!("\"busy\":{},\"demoted\":[{}],\"suggestions\":[{}]}}\n", gov.mood.busy(), demoted.join(","), sg.join(","));
    let full = format!("{{\"unix\":{:.0},\"acting\":{},\"cpu\":{:.0},{}", now_unix(), acting, gov.total_pct, tail);
    (full, format!("{}{}", acting, tail))
}

/// The watch loop: alerts every 30 ticks, governor + rules every tick (1 s).
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
    base.set_tz(sys::local_offset_secs());
    let metrics = |gov: &snifrig_fix::governor::Governor| {
        let (commit_pct, avail_mb) = sys::memory_status().unwrap_or((0.0, 0.0));
        Metrics { total_cpu_pct: gov.total_pct, commit_pct, avail_mb, fg_cpu_pct: gov.fg_cpu_pct }
    };
    loop {
        if stop.exists() { let _ = std::fs::remove_file(&stop); break; }
        if n % 30 == 0 {
            let (_, ran) = process_once_ex(dir);
            if !ran.is_empty() {
                let m = metrics(&gov);
                for (action, name, trig) in ran { learner.start("fix", &action, &name, trig, now_unix(), m.clone()); }
            }
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
        if n > 0 && n % 60 == 0 { sugg = rules::suggestions(dir); }
        let mode = ledger::read_mode(dir);
        let paused = now_unix() < ledger::paused_until(dir);
        let mut act = false;
        if mode == Mode::Off || paused {
            if !gov.demoted.is_empty() { let a = gov.acting; log_gov(dir, gov.restore_all(a), a); }
        } else {
            act = matches!(mode, Mode::Ask | Mode::Auto);
            if act != gov.acting && !gov.demoted.is_empty() { let a = gov.acting; log_gov(dir, gov.restore_all(a), a); }
            if act != enf.acting { enf.reset(); }
            gov.acting = act;
            enf.acting = act;
            let changes = gov.tick(now_unix(), act);
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
            snifrig_fix::brain::apply(&mut gov.hints, &brain.cache());
            for note in learner.take_notes() {
                let alert = snifrig_fix::Alert { t: String::new(), key: "learn:blocked".into(), msg: note.clone() };
                ledger::record(dir, &alert, &Action::Report, None, "refused", &note);
            }
        }
        if n > 0 && n % 60 == 0 {
            let (now, m) = (now_unix(), metrics(&gov));
            let top: Vec<String> = gov.top.iter().map(|(name, _)| name.clone()).collect();
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
    { let a = gov.acting; log_gov(dir, gov.restore_all(a), a); }
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
const USAGE: &str = "usage: snifrig-fix [once|status|pending|mode M|approve ID|dismiss ID|governor status|governor undo PID|rule add \"PATTERN key=value ...\"|rule list|rule remove N|machine-id|license install PATH|license accept|license status] [--dir D] [--accept-license \"UFL-3.7 snifrig-fix\"]";
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
