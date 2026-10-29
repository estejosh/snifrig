//! snifrig's scheduler: your recurring jobs run under snifrig instead of Windows Task Scheduler,
//! so they can be run the way that suits the PC:
//! - never a window (no console, no Terminal pop-up taking the desktop from you),
//! - one run at a time per job (a job still running is never started again: no pileups),
//! - low priority unless the job says otherwise,
//! - at a good moment: not while you are in a game or full screen, not while the PC is swamped,
//!   or only when you have stepped away; a run that waited too long goes anyway, quietly,
//! - a time limit that ends the run and everything it started, if it hangs,
//! - one catch-up run after a missed time, not ten.
//!
//! Jobs live in jobs.txt (edited by the app's Jobs page or by hand); what happened lives in
//! jobs-state.txt and jobs-history.jsonl; the app reads jobs-status.json.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub const JOBS_FILE: &str = "jobs.txt";
pub const STATE_FILE: &str = "jobs-state.txt";
pub const HISTORY_FILE: &str = "jobs-history.jsonl";
pub const STATUS_FILE: &str = "jobs-status.json";
/// While this file exists, Windows runs the moved jobs again (snifrig handed them back softly:
/// licence lapsed, uninstall, or the fixer could not stay up). snifrig takes them back when it can.
pub const HANDED_BACK: &str = "jobs-in-windows";

/// When a job may start once it is due.
#[derive(Clone, Debug, PartialEq)]
pub enum When { Anytime, NotGaming, Idle }

#[derive(Clone, Debug, PartialEq)]
pub enum Sched { Every(f64), Daily(u32), Logon, KeepRunning, Once(f64) }

#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    pub name: String,
    pub run: String,
    pub dir: String,
    pub sched: Sched,
    pub when: When,
    /// End the run (and what it started) after this long; 0 = no limit.
    pub max_secs: f64,
    /// A due run waits at most this long for a good moment, then runs anyway.
    pub latest_secs: f64,
    pub normal_priority: bool,
    pub enabled: bool,
    /// The Windows task this came from (import), so undo can turn it back on.
    pub from_task: String,
}

impl Job {
    pub fn new(name: &str, run: &str) -> Job {
        Job { name: name.into(), run: run.into(), dir: String::new(), sched: Sched::Every(3600.0), when: When::NotGaming, max_secs: 0.0, latest_secs: 7200.0, normal_priority: false, enabled: true, from_task: String::new() }
    }
}

pub fn parse_secs(s: &str) -> Option<f64> {
    let s = s.trim().to_lowercase();
    if s == "0" || s == "none" { return Some(0.0); }
    let mut total = 0.0;
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() || c == '.' { num.push(c); continue; }
        if c == ' ' { continue; }
        let n: f64 = num.parse().ok()?;
        num.clear();
        total += n * match c { 's' => 1.0, 'm' => 60.0, 'h' => 3600.0, 'd' => 86400.0, _ => return None };
    }
    if !num.is_empty() { total += num.parse::<f64>().ok()? * 60.0; } // bare number = minutes
    Some(total)
}

pub fn fmt_secs(s: f64) -> String {
    let s = s.round() as u64;
    if s == 0 { "none".into() }
    else if s % 86400 == 0 { format!("{}d", s / 86400) }
    else if s % 3600 == 0 { format!("{}h", s / 3600) }
    else if s % 60 == 0 { format!("{}m", s / 60) }
    else { format!("{}s", s) }
}

pub fn sched_text(s: &Sched) -> String {
    match s {
        Sched::Every(x) => format!("every {}", fmt_secs(*x)),
        Sched::Daily(m) => format!("daily {:02}:{:02}", m / 60, m % 60),
        Sched::Logon => "at logon".into(),
        Sched::KeepRunning => "keep running".into(),
        Sched::Once(t) => format!("once {:.0}", t),
    }
}

pub fn when_text(w: &When) -> &'static str {
    match w { When::Anytime => "anytime", When::NotGaming => "not gaming", When::Idle => "idle" }
}

fn parse_sched(v: &str) -> Option<Sched> {
    let v = v.trim().to_lowercase();
    if let Some(x) = v.strip_prefix("every") { return parse_secs(x).filter(|s| *s >= 60.0).map(Sched::Every); }
    if let Some(x) = v.strip_prefix("daily") {
        let (h, m) = x.trim().split_once(':')?;
        let (h, m): (u32, u32) = (h.trim().parse().ok()?, m.trim().parse().ok()?);
        return (h < 24 && m < 60).then_some(Sched::Daily(h * 60 + m));
    }
    if let Some(x) = v.strip_prefix("once") { return x.trim().parse::<f64>().ok().filter(|t| *t > 1e9).map(Sched::Once); }
    match v.as_str() { "at logon" | "logon" => Some(Sched::Logon), "keep running" => Some(Sched::KeepRunning), _ => None }
}

/// Parse jobs.txt. Blocks: `[name]` then `key = value` lines; `#` starts a comment line.
/// Returns the jobs and plain-words errors for lines it could not use.
pub fn parse(text: &str) -> (Vec<Job>, Vec<String>) {
    let (mut jobs, mut errs) = (Vec::new(), Vec::new());
    let mut cur: Option<Job> = None;
    for (i, raw) in text.lines().enumerate() {
        let l = raw.trim();
        if l.is_empty() || l.starts_with('#') { continue; }
        if l.starts_with('[') && l.ends_with(']') {
            if let Some(j) = cur.take() { jobs.push(j); }
            cur = Some(Job::new(l[1..l.len() - 1].trim(), ""));
            continue;
        }
        let Some(j) = cur.as_mut() else { errs.push(format!("line {}: put a [job name] line first", i + 1)); continue };
        let Some((k, v)) = l.split_once('=') else { errs.push(format!("line {}: expected key = value", i + 1)); continue };
        let (k, v) = (k.trim().to_lowercase(), v.trim());
        let ok = match k.as_str() {
            "run" => { j.run = v.to_string(); true }
            "dir" => { j.dir = v.to_string(); true }
            "schedule" => parse_sched(v).map(|s| j.sched = s).is_some(),
            "when" => match v.to_lowercase().as_str() {
                "anytime" => { j.when = When::Anytime; true }
                "not gaming" | "not-gaming" => { j.when = When::NotGaming; true }
                "idle" => { j.when = When::Idle; true }
                _ => false,
            },
            "max" => parse_secs(v).map(|s| j.max_secs = s).is_some(),
            "latest" => parse_secs(v).map(|s| j.latest_secs = s).is_some(),
            "priority" => match v.to_lowercase().as_str() { "low" => { j.normal_priority = false; true } "normal" => { j.normal_priority = true; true } _ => false },
            "enabled" => match v.to_lowercase().as_str() { "yes" | "on" | "true" => { j.enabled = true; true } "no" | "off" | "false" => { j.enabled = false; true } _ => false },
            "from_task" => { j.from_task = v.to_string(); true }
            _ => false,
        };
        if !ok { errs.push(format!("line {} ({}): '{}' is not a valid {}", i + 1, j.name, v, k)); }
    }
    if let Some(j) = cur.take() { jobs.push(j); }
    for j in &jobs { if j.run.is_empty() { errs.push(format!("{}: no run = line, it will not run", j.name)); } }
    jobs.retain(|j| !j.run.is_empty());
    (jobs, errs)
}

pub fn write(jobs: &[Job]) -> String {
    let mut s = String::from("# snifrig jobs: they run with no window, one at a time, at a good moment for the PC.\r\n# schedule: every 15m | daily 05:00 | at logon | keep running\r\n# when: anytime | not gaming | idle     max: time limit (none = no limit)     latest: longest wait for a good moment\r\n");
    for j in jobs {
        s.push_str(&format!("\r\n[{}]\r\nrun = {}\r\n", j.name, j.run));
        if !j.dir.is_empty() { s.push_str(&format!("dir = {}\r\n", j.dir)); }
        s.push_str(&format!("schedule = {}\r\nwhen = {}\r\nmax = {}\r\nlatest = {}\r\n", sched_text(&j.sched), when_text(&j.when), fmt_secs(j.max_secs), fmt_secs(j.latest_secs)));
        if j.normal_priority { s.push_str("priority = normal\r\n"); }
        if !j.enabled { s.push_str("enabled = no\r\n"); }
        if !j.from_task.is_empty() { s.push_str(&format!("from_task = {}\r\n", j.from_task)); }
    }
    s
}

pub fn load(dir: &Path) -> (Vec<Job>, Vec<String>) { parse(&std::fs::read_to_string(dir.join(JOBS_FILE)).unwrap_or_default()) }

pub fn save(dir: &Path, jobs: &[Job]) { crate::rules::write_atomic(&dir.join(JOBS_FILE), &write(jobs)); }

/// What the scheduler remembers per job.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct State { pub last_start: f64, pub last_end: f64, pub last_exit: i64, pub runs: u32, pub fails: u32, pub avg_secs: f64 }

fn load_state(dir: &Path) -> HashMap<String, State> {
    let mut m = HashMap::new();
    for l in std::fs::read_to_string(dir.join(STATE_FILE)).unwrap_or_default().lines() {
        let f: Vec<&str> = l.split('\t').collect();
        if f.len() < 7 { continue; }
        let n = |i: usize| f[i].parse::<f64>().unwrap_or(0.0);
        m.insert(f[0].to_string(), State { last_start: n(1), last_end: n(2), last_exit: f[3].parse().unwrap_or(-1), runs: n(4) as u32, fails: n(5) as u32, avg_secs: n(6) });
    }
    m
}

fn save_state(dir: &Path, m: &HashMap<String, State>) {
    let mut keys: Vec<&String> = m.keys().collect();
    keys.sort();
    let body: String = keys.iter().map(|k| { let s = &m[*k]; format!("{}\t{:.0}\t{:.0}\t{}\t{}\t{}\t{:.0}\n", k, s.last_start, s.last_end, s.last_exit, s.runs, s.fails, s.avg_secs) }).collect();
    crate::rules::write_atomic(&dir.join(STATE_FILE), &body);
}

/// What the PC is doing right now, as the scheduler needs it.
#[derive(Clone, Debug, Default)]
pub struct Ctx { pub now: f64, pub boot: f64, pub gaming: bool, pub cpu_pct: f64, pub idle_secs: f64, pub local_offset: i64 }

const BUSY_PCT: f64 = 85.0;
const IDLE_SECS: f64 = 300.0;
const SPREAD_SECS: f64 = 20.0;
const ANYTIME_BUSY_WAIT: f64 = 600.0;

/// Pure: is this job due now, and since when? None = not due.
pub fn due_since(j: &Job, s: &State, ctx: &Ctx, first_seen: f64) -> Option<f64> {
    match j.sched {
        Sched::Every(x) => {
            let due = if s.last_start > 0.0 { s.last_start + x } else { first_seen };
            (ctx.now >= due).then_some(due)
        }
        Sched::Daily(m) => {
            let local = ctx.now + ctx.local_offset as f64;
            let day = (local / 86400.0).floor() * 86400.0 - ctx.local_offset as f64;
            let today = day + m as f64 * 60.0;
            let slot = if ctx.now >= today { today } else { today - 86400.0 };
            // Catch up a missed slot once; a new job waits for its first slot after it was added.
            let ok = if s.last_start > 0.0 { s.last_start < slot } else { slot >= first_seen };
            ok.then_some(slot)
        }
        Sched::Logon => (s.last_start < ctx.boot).then_some(ctx.boot.max(first_seen)),
        Sched::KeepRunning => Some(ctx.now),
        Sched::Once(t) => (s.last_start < t && ctx.now >= t).then_some(t),
    }
}

/// Pure: may a due job start now? Ok(()) or the reason it waits, in plain words.
pub fn gate(j: &Job, due: f64, ctx: &Ctx, last_any_start: f64) -> Result<(), String> {
    if matches!(j.sched, Sched::KeepRunning | Sched::Logon) { return Ok(()); }
    let waited = ctx.now - due;
    if waited >= j.latest_secs && j.latest_secs > 0.0 { return Ok(()); }
    if ctx.gaming && j.when != When::Anytime { return Err("waiting: you are in a game or full screen".into()); }
    if j.when == When::Idle && ctx.idle_secs < IDLE_SECS { return Err("waiting for you to step away".into()); }
    if ctx.cpu_pct >= BUSY_PCT && !(j.when == When::Anytime && waited >= ANYTIME_BUSY_WAIT) { return Err(format!("waiting: the PC is busy ({:.0}% CPU)", ctx.cpu_pct)); }
    if ctx.now - last_any_start < SPREAD_SECS { return Err("waiting: another job just started".into()); }
    Ok(())
}

/// Lowercase, no quotes, single spaces: two command lines that run the same thing compare equal.
pub fn norm_cmd(s: &str) -> String { s.to_lowercase().replace('"', "").split_whitespace().collect::<Vec<_>>().join(" ") }

/// Program and the rest of a command line.
pub fn split_cmd(run: &str) -> (String, String) {
    let r = run.trim();
    if let Some(rest) = r.strip_prefix('"') {
        if let Some(i) = rest.find('"') { return (rest[..i].to_string(), rest[i + 1..].trim().to_string()); }
    }
    match r.split_once(' ') { Some((a, b)) => (a.to_string(), b.trim().to_string()), None => (r.to_string(), String::new()) }
}

struct Running { child: std::process::Child, job: isize, started: f64, log: PathBuf }

pub struct Scheduler {
    pub jobs: Vec<Job>,
    pub errors: Vec<String>,
    state: HashMap<String, State>,
    running: HashMap<String, Running>,
    first_seen: HashMap<String, f64>,
    waiting: HashMap<String, String>,
    backoff: HashMap<String, (f64, f64)>,
    jobs_mtime: Option<std::time::SystemTime>,
    last_any_start: f64,
    dir: PathBuf,
    last_status: String,
}

impl Scheduler {
    pub fn new(dir: &Path) -> Scheduler {
        Scheduler { jobs: Vec::new(), errors: Vec::new(), state: load_state(dir), running: HashMap::new(), first_seen: HashMap::new(), waiting: HashMap::new(), backoff: HashMap::new(), jobs_mtime: None, last_any_start: 0.0, dir: dir.to_path_buf(), last_status: String::new() }
    }

    fn reload(&mut self, now: f64) {
        let mt = std::fs::metadata(self.dir.join(JOBS_FILE)).and_then(|m| m.modified()).ok();
        if mt == self.jobs_mtime && !self.jobs.is_empty() { return; }
        self.jobs_mtime = mt;
        let (jobs, errs) = load(&self.dir);
        // New jobs are staggered so an import does not start everything at once.
        let mut k = 0.0;
        let mut stamped = false;
        // Last run times carried over from Windows by the import (name<TAB>unix), so a moved job
        // keeps its rhythm: the weekly one still runs on its day, not right after the move.
        let seed: HashMap<String, f64> = std::fs::read_to_string(self.dir.join("jobs-seed.txt")).unwrap_or_default().lines()
            .filter_map(|l| { let (n, t) = l.split_once('\t')?; Some((n.to_string(), t.trim().parse().ok()?)) }).collect();
        for j in &jobs {
            if self.first_seen.contains_key(&j.name) { continue; }
            self.first_seen.insert(j.name.clone(), now + 60.0 + k * 45.0);
            k += 1.0;
            if let (false, Some(&t)) = (self.state.contains_key(&j.name), seed.get(&j.name)) {
                self.state.insert(j.name.clone(), State { last_start: t, last_end: t, last_exit: 0, ..Default::default() });
                stamped = true;
            }
            // Moved from Windows: Windows already started its at-logon jobs this boot, so snifrig
            // takes over from the next logon instead of starting a second copy now.
            if j.sched == Sched::Logon && !j.from_task.is_empty() && !self.state.contains_key(&j.name) {
                self.state.insert(j.name.clone(), State { last_start: now, last_end: now, last_exit: 0, ..Default::default() });
                stamped = true;
            }
        }
        if stamped { save_state(&self.dir, &self.state); }
        self.jobs = jobs;
        self.errors = errs;
    }

    /// Call every few seconds. Returns plain-words notes of what started, ended or failed.
    pub fn tick(&mut self, ctx: &Ctx) -> Vec<String> {
        self.reload(ctx.now);
        let mut notes = Vec::new();
        let mut changed = false;
        // Finished or over its time limit?
        let names: Vec<String> = self.running.keys().cloned().collect();
        for n in names {
            let max = self.jobs.iter().find(|j| j.name == n).map_or(0.0, |j| j.max_secs);
            let r = self.running.get_mut(&n).unwrap();
            let done = r.child.try_wait().ok().flatten();
            let over = done.is_none() && max > 0.0 && ctx.now - r.started > max;
            if done.is_none() && !over { continue; }
            if over { end_group(r.job, r.child.id()); let _ = r.child.wait(); }
            let code = if over { -2 } else { done.and_then(|s| s.code()).map(|c| c as i64).unwrap_or(-1) };
            let r = self.running.remove(&n).unwrap();
            // Whatever the run left behind (a server it started on purpose) keeps running.
            close_group(r.job);
            let secs = ctx.now - r.started;
            let st = self.state.entry(n.clone()).or_default();
            st.last_end = ctx.now; st.last_exit = code; st.runs += 1;
            if code != 0 { st.fails += 1; }
            st.avg_secs = if st.avg_secs == 0.0 { secs } else { st.avg_secs * 0.7 + secs * 0.3 };
            let note = if over { format!("ended: it ran past its {} limit", fmt_secs(max)) } else if code == 0 { "ok".to_string() } else { format!("failed (exit code {}); output in {}", code, r.log.display()) };
            self.history(&n, r.started, ctx.now, code, &note);
            if code != 0 { notes.push(format!("job {}: {}", n, note)); }
            // A keep-running job that dies fast backs off: 30 s, doubling, at most 30 min.
            if self.jobs.iter().any(|j| j.name == n && j.sched == Sched::KeepRunning) {
                let b = self.backoff.entry(n.clone()).or_insert((0.0, 15.0));
                b.1 = if secs < 60.0 { (b.1 * 2.0).min(1800.0) } else { 30.0 };
                b.0 = ctx.now + b.1;
            }
            changed = true;
        }
        // Start what is due and allowed.
        let jobs = self.jobs.clone();
        let mut procs_cache: Option<Vec<(String, String)>> = None;
        let handed_back = self.dir.join(HANDED_BACK).exists();
        for j in jobs.iter().filter(|j| j.enabled) {
            if handed_back && !j.from_task.is_empty() { self.waiting.insert(j.name.clone(), "Windows runs this one for now".into()); continue; }
            if self.running.contains_key(&j.name) { self.waiting.remove(&j.name); continue; }
            if let Some((t, _)) = self.backoff.get(&j.name) { if ctx.now < *t { continue; } }
            let st = self.state.get(&j.name).cloned().unwrap_or_default();
            let first = *self.first_seen.get(&j.name).unwrap_or(&ctx.now);
            let Some(due) = due_since(j, &st, ctx, first) else { self.waiting.remove(&j.name); continue };
            if let Err(why) = gate(j, due, ctx, self.last_any_start) { self.waiting.insert(j.name.clone(), why); continue; }
            // Already running outside snifrig (started by Windows before the move, or by hand)?
            let procs = procs_cache.get_or_insert_with(|| running_cmdlines());
            let want = norm_cmd(&j.run);
            if procs.iter().any(|(_, c)| *c == want) {
                if matches!(j.sched, Sched::Every(_) | Sched::Daily(_) | Sched::Logon) {
                    // Count it as this slot's run, so it is not started twice.
                    let st = self.state.entry(j.name.clone()).or_default();
                    st.last_start = ctx.now;
                    changed = true;
                }
                self.waiting.insert(j.name.clone(), "already running (started outside snifrig)".into());
                continue;
            }
            match self.start(j, ctx.now) {
                Ok(r) => {
                    self.running.insert(j.name.clone(), r);
                    self.state.entry(j.name.clone()).or_default().last_start = ctx.now;
                    self.waiting.remove(&j.name);
                    self.last_any_start = ctx.now;
                    changed = true;
                }
                Err(e) => {
                    let st = self.state.entry(j.name.clone()).or_default();
                    st.last_start = ctx.now; st.last_end = ctx.now; st.last_exit = -1; st.fails += 1;
                    self.history(&j.name, ctx.now, ctx.now, -1, &format!("could not start: {}", e));
                    notes.push(format!("job {}: could not start: {}", j.name, e));
                    changed = true;
                }
            }
        }
        if changed { save_state(&self.dir, &self.state); }
        self.write_status(ctx);
        notes
    }

    /// Run a job right now (the app's Run now button), skipping the gates but never a second copy.
    pub fn run_now(&mut self, name: &str, now: f64) -> Result<String, String> {
        self.reload(now);
        let j = self.jobs.iter().find(|j| j.name.eq_ignore_ascii_case(name)).cloned().ok_or("no job with that name")?;
        if self.running.contains_key(&j.name) { return Err(format!("{} is already running", j.name)); }
        let r = self.start(&j, now)?;
        self.running.insert(j.name.clone(), r);
        self.state.entry(j.name.clone()).or_default().last_start = now;
        save_state(&self.dir, &self.state);
        Ok(format!("{} started", j.name))
    }

    #[cfg(windows)]
    fn start(&self, j: &Job, now: f64) -> Result<Running, String> {
        use std::os::windows::io::AsRawHandle;
        use crate::os_ext::CommandExt;
        let (prog, args) = split_cmd(&j.run);
        let logs = self.dir.join("jobs");
        let _ = std::fs::create_dir_all(&logs);
        let safe: String = j.name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
        let log = logs.join(format!("{}.log", safe));
        if std::fs::metadata(&log).map(|m| m.len() > 4 << 20).unwrap_or(false) { let _ = std::fs::rename(&log, logs.join(format!("{}.old.log", safe))); }
        let out = std::fs::OpenOptions::new().create(true).append(true).open(&log).map_err(|e| e.to_string())?;
        use std::io::Write;
        let _ = writeln!(&out, "\r\n=== {} started by snifrig at {:.0} ===", j.name, now);
        let err = out.try_clone().map_err(|e| e.to_string())?;
        let mut c = std::process::Command::new(&prog);
        if !args.is_empty() { c.raw_arg(&args); }
        if !j.dir.is_empty() && Path::new(&j.dir).is_dir() { c.current_dir(&j.dir); }
        // No window, started paused so it is inside the job object before it can start anything.
        let prio = if j.normal_priority { 0 } else { 0x0000_4000 };
        c.creation_flags(0x0800_0000 | 0x0000_0004 | prio).stdin(std::process::Stdio::null()).stdout(out).stderr(err);
        let child = c.spawn().map_err(|e| format!("{} ({})", e, prog))?;
        let h = child.as_raw_handle() as isize;
        let job = unsafe { windows_sys::Win32::System::JobObjects::CreateJobObjectW(std::ptr::null(), std::ptr::null()) } as isize;
        if job != 0 { unsafe { windows_sys::Win32::System::JobObjects::AssignProcessToJobObject(job as _, h as _); } }
        #[link(name = "ntdll", kind = "raw-dylib")]
        extern "system" { fn NtResumeProcess(h: isize) -> i32; }
        unsafe { NtResumeProcess(h); }
        Ok(Running { child, job, started: now, log })
    }

    #[cfg(not(windows))]
    fn start(&self, j: &Job, now: f64) -> Result<Running, String> {
        use std::os::unix::process::CommandExt;
        let logs = self.dir.join("jobs");
        let _ = std::fs::create_dir_all(&logs);
        let safe: String = j.name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
        let log = logs.join(format!("{}.log", safe));
        if std::fs::metadata(&log).map(|m| m.len() > 4 << 20).unwrap_or(false) { let _ = std::fs::rename(&log, logs.join(format!("{}.old.log", safe))); }
        let out = std::fs::OpenOptions::new().create(true).append(true).open(&log).map_err(|e| e.to_string())?;
        use std::io::Write;
        let _ = writeln!(&out, "\n=== {} started by snifrig at {:.0} ===", j.name, now);
        let err = out.try_clone().map_err(|e| e.to_string())?;
        // The command line as you would type it in a terminal.
        let mut c = std::process::Command::new("/bin/sh");
        c.arg("-c").arg(&j.run);
        if !j.dir.is_empty() && Path::new(&j.dir).is_dir() { c.current_dir(&j.dir); }
        let nice = if j.normal_priority { 0 } else { 10 };
        c.process_group(0).stdin(std::process::Stdio::null()).stdout(out).stderr(err);
        unsafe { c.pre_exec(move || { if nice != 0 { libc::setpriority(libc::PRIO_PROCESS, 0, nice); } Ok(()) }); }
        let child = c.spawn().map_err(|e| format!("{} (/bin/sh -c {})", e, j.run))?;
        Ok(Running { job: child.id() as isize, child, started: now, log })
    }

    fn history(&self, name: &str, start: f64, end: f64, code: i64, note: &str) {
        use crate::json::esc;
        use std::io::Write;
        let p = self.dir.join(HISTORY_FILE);
        if std::fs::metadata(&p).map(|m| m.len() > 2 << 20).unwrap_or(false) { let _ = std::fs::rename(&p, self.dir.join("jobs-history.old.jsonl")); }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
            let _ = writeln!(f, "{{\"name\":\"{}\",\"start\":{:.0},\"end\":{:.0},\"secs\":{:.0},\"exit\":{},\"note\":\"{}\"}}", esc(name), start, end, end - start, code, esc(note));
        }
    }

    /// When the job will next be due (unix), for the app. 0 = now or unknown.
    fn next_due(&self, j: &Job, ctx: &Ctx) -> f64 {
        let st = self.state.get(&j.name).cloned().unwrap_or_default();
        match j.sched {
            Sched::Every(x) => if st.last_start > 0.0 { st.last_start + x } else { *self.first_seen.get(&j.name).unwrap_or(&ctx.now) },
            Sched::Daily(m) => {
                let local = ctx.now + ctx.local_offset as f64;
                let today = (local / 86400.0).floor() * 86400.0 - ctx.local_offset as f64 + m as f64 * 60.0;
                if st.last_start >= today { today + 86400.0 } else { today }
            }
            Sched::Once(t) => if st.last_start < t { t } else { 0.0 },
            _ => 0.0,
        }
    }

    fn write_status(&mut self, ctx: &Ctx) {
        use crate::json::esc;
        let items: Vec<String> = self.jobs.iter().map(|j| {
            let st = self.state.get(&j.name).cloned().unwrap_or_default();
            let running = self.running.get(&j.name).map_or(0.0, |r| r.started);
            format!("{{\"name\":\"{}\",\"run\":\"{}\",\"schedule\":\"{}\",\"when\":\"{}\",\"enabled\":{},\"running_since\":{:.0},\"last_start\":{:.0},\"last_secs\":{:.0},\"last_exit\":{},\"runs\":{},\"fails\":{},\"next\":{:.0},\"waiting\":\"{}\",\"from_task\":\"{}\"}}",
                esc(&j.name), esc(&j.run), esc(&sched_text(&j.sched)), when_text(&j.when), j.enabled, running, st.last_start, (st.last_end - st.last_start).max(0.0), st.last_exit, st.runs, st.fails, self.next_due(j, ctx),
                esc(self.waiting.get(&j.name).map(|s| s.as_str()).unwrap_or("")), esc(&j.from_task))
        }).collect();
        let errs: Vec<String> = self.errors.iter().map(|e| format!("\"{}\"", esc(e))).collect();
        let body = format!("{{\"jobs\":[{}],\"errors\":[{}]}}\n", items.join(","), errs.join(","));
        if body != self.last_status { crate::rules::write_atomic(&self.dir.join(STATUS_FILE), &body); self.last_status = body; }
    }

    pub fn running_names(&self) -> HashSet<String> { self.running.keys().cloned().collect() }
}

/// (image name, normalised command line) of every process; read only when a job is about to start.
fn running_cmdlines() -> Vec<(String, String)> {
    let mut buf = Vec::new();
    crate::sys::snapshot(&mut buf).into_iter().filter(|p| p.pid > 4).map(|p| { let c = crate::sys::cmdline(p.pid); (p.name.to_lowercase(), norm_cmd(&c)) }).filter(|(_, c)| !c.is_empty()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(now: f64) -> Ctx { Ctx { now, boot: 1000.0, gaming: false, cpu_pct: 20.0, idle_secs: 0.0, local_offset: 0 } }

    #[test]
    fn parse_and_write_roundtrip() {
        let t = "[NATV]\nrun = \"C:\\x\\powershell.exe\" -File \"X:\\a b.ps1\"\ndir = X:\\NATV\nschedule = every 15m\nwhen = not gaming\nmax = 1h\n\n[sem]\nrun = cmd.exe /c X:\\w.cmd\nschedule = daily 05:00\nwhen = idle\nenabled = no\nfrom_task = \\SEM coverage refresh\n";
        let (j, e) = parse(t);
        assert!(e.is_empty(), "{:?}", e);
        assert_eq!(j.len(), 2);
        assert_eq!(j[0].sched, Sched::Every(900.0));
        assert_eq!(j[0].max_secs, 3600.0);
        assert_eq!(j[1].sched, Sched::Daily(300));
        assert_eq!(j[1].when, When::Idle);
        assert!(!j[1].enabled);
        let (j2, e2) = parse(&write(&j));
        assert!(e2.is_empty());
        assert_eq!(j, j2);
    }

    #[test]
    fn bad_lines_are_reported_not_fatal() {
        let (j, e) = parse("[a]\nrun = x.exe\nschedule = sometimes\nwhen = later\n[b]\nschedule = every 1h\n");
        assert_eq!(j.len(), 1);
        assert_eq!(e.len(), 3, "{:?}", e);
    }

    #[test]
    fn every_is_due_after_interval_and_only_once() {
        let j = Job { sched: Sched::Every(900.0), ..Job::new("a", "x.exe") };
        let s = State { last_start: 10_000.0, ..Default::default() };
        assert!(due_since(&j, &s, &ctx(10_800.0), 0.0).is_none());
        assert_eq!(due_since(&j, &s, &ctx(10_900.0), 0.0), Some(10_900.0));
        // missed for hours: still one run, due since the first missed slot
        assert_eq!(due_since(&j, &s, &ctx(50_000.0), 0.0), Some(10_900.0));
    }

    #[test]
    fn daily_catches_up_once() {
        let j = Job { sched: Sched::Daily(5 * 60), ..Job::new("a", "x.exe") };
        let day = 86400.0 * 100.0;
        let ran_yesterday = State { last_start: day - 86400.0 + 5.0 * 3600.0, ..Default::default() };
        assert!(due_since(&j, &ran_yesterday, &ctx(day + 4.0 * 3600.0), 0.0).is_none(), "before 05:00");
        assert_eq!(due_since(&j, &ran_yesterday, &ctx(day + 9.0 * 3600.0), 0.0), Some(day + 5.0 * 3600.0), "missed 05:00, runs once");
        let ran_today = State { last_start: day + 9.0 * 3600.0, ..Default::default() };
        assert!(due_since(&j, &ran_today, &ctx(day + 10.0 * 3600.0), 0.0).is_none());
    }

    #[test]
    fn gates_wait_for_a_good_moment_but_not_forever() {
        let j = Job { when: When::NotGaming, latest_secs: 3600.0, ..Job::new("a", "x.exe") };
        let mut c = ctx(10_000.0);
        c.gaming = true;
        assert!(gate(&j, 9_000.0, &c, 0.0).unwrap_err().contains("game"));
        assert!(gate(&j, 6_000.0, &c, 0.0).is_ok(), "waited over an hour: runs anyway, quietly");
        c.gaming = false; c.cpu_pct = 95.0;
        assert!(gate(&j, 9_000.0, &c, 0.0).unwrap_err().contains("busy"));
        c.cpu_pct = 30.0;
        assert!(gate(&j, 9_000.0, &c, 9_990.0).unwrap_err().contains("just started"), "spread starts out");
        assert!(gate(&j, 9_000.0, &c, 0.0).is_ok());
        let idle = Job { when: When::Idle, ..j.clone() };
        assert!(gate(&idle, 9_000.0, &c, 0.0).unwrap_err().contains("step away"));
        c.idle_secs = 600.0;
        assert!(gate(&idle, 9_000.0, &c, 0.0).is_ok());
        let any = Job { when: When::Anytime, ..j };
        c.gaming = true; c.cpu_pct = 30.0;
        assert!(gate(&any, 9_000.0, &c, 0.0).is_ok(), "anytime jobs run during a game too, with no window");
    }

    #[test]
    fn command_lines_split_and_compare() {
        assert_eq!(split_cmd("\"C:\\a b\\p.exe\" -x \"y z\""), ("C:\\a b\\p.exe".to_string(), "-x \"y z\"".to_string()));
        assert_eq!(split_cmd("cmd.exe /c X:\\w.cmd"), ("cmd.exe".to_string(), "/c X:\\w.cmd".to_string()));
        assert_eq!(norm_cmd("\"C:\\P.exe\"   -m  http.server"), norm_cmd("c:\\p.exe -m http.server"));
    }

    #[test]
    fn once_runs_once() {
        let j = Job { sched: Sched::Once(20_000.0), ..Job::new("a", "x.exe") };
        assert!(due_since(&j, &State::default(), &ctx(19_000.0), 0.0).is_none());
        assert_eq!(due_since(&j, &State::default(), &ctx(20_500.0), 0.0), Some(20_000.0));
        let done = State { last_start: 20_600.0, ..Default::default() };
        assert!(due_since(&j, &done, &ctx(30_000.0), 0.0).is_none());
        let (p, e) = parse("[q]\nrun = x.exe\nschedule = once 1792700000\n");
        assert!(e.is_empty(), "{:?}", e);
        assert_eq!(p[0].sched, Sched::Once(1792700000.0));
    }

    #[test]
    fn durations() {
        assert_eq!(parse_secs("15m"), Some(900.0));
        assert_eq!(parse_secs("1h30m"), Some(5400.0));
        assert_eq!(parse_secs("none"), Some(0.0));
        assert_eq!(fmt_secs(5400.0), "90m");
        assert_eq!(fmt_secs(7200.0), "2h");
    }
}

/// End a job that ran past its limit, with everything it started.
#[cfg(windows)]
fn end_group(job: isize, _pid: u32) { unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(job as _, 1); } }
#[cfg(not(windows))]
fn end_group(group: isize, _pid: u32) { if group > 0 { unsafe { libc::kill(-(group as i32), libc::SIGKILL); } } }

/// Let go of the job's handle; whatever it left running on purpose keeps running.
#[cfg(windows)]
fn close_group(job: isize) { unsafe { windows_sys::Win32::Foundation::CloseHandle(job as _); } }
#[cfg(not(windows))]
fn close_group(_group: isize) {}
