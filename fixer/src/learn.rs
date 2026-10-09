//! Learning: did a change help, and what does the user think of it. All local, no network.
//!
//! Every change the fixer or governor makes becomes an Episode: {id, unix, kind, action, name,
//! trigger, before}. After EVAL_AFTER_SECS the machine is measured again and the episode is scored:
//! improvement = relative drop in the metric that triggered it (total CPU, or commit / available
//! memory). success = improvement >= IMPROVE_OK, or the metric went from over its trigger
//! threshold to under it. Finished episodes are appended to learn.jsonl (cap 1 MB, rotated to
//! learn.jsonl.old) and rolled into learn-stats.json: per (kind, action, name) tries, successes
//! and mean improvement.
//!
//! User signals (undo, approve, dismiss, rule_added) are appended to learn-feedback.jsonl by
//! whichever process saw them (the CLI commands run in other processes than the watch loop); the
//! loop tails that file. All logic below is pure and testable without Windows; file helpers are
//! thin wrappers.
//!
//!   learn.jsonl         {"id":N,"unix":N,"kind":"governor|rule|fix","action":"..","name":"..","trigger":"cpu|mem",
//!                        "before":{..},"after":{..},"improvement":F,"success":bool,"undone":bool}
//!   learn-stats.json    one stat object per line: {"kind":..,"action":..,"name":..,"tries":N,"successes":N,"mean_imp":F}
//!   learn-feedback.jsonl {"unix":N,"signal":"undo|approve|dismiss|rule_added","kind":..,"action":..,"name":..,"trigger":..,"start":bool}

use crate::governor::Hints;
use crate::json::{esc, num_field, str_field};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const EVAL_AFTER_SECS: f64 = 180.0;
pub const IMPROVE_OK: f64 = 0.2;
pub const CPU_TRIGGER: f64 = 75.0;
pub const COMMIT_TRIGGER: f64 = 85.0;
pub const AVAIL_TRIGGER_MB: f64 = 2048.0;
pub const MIN_NAME_TRIES: u32 = 3;
pub const BLOCK_TRIES: u32 = 5;
pub const BLOCK_PROB: f64 = 0.15;
pub const PROTECT_UNDOS: usize = 2;
pub const PROTECT_WINDOW_SECS: f64 = 14.0 * 86400.0;
const KEEP_FEEDBACK_SECS: f64 = 90.0 * 86400.0;
const LEARN_CAP: u64 = 1024 * 1024;
const FEEDBACK_CAP: u64 = 256 * 1024;
const MAX_OPEN: usize = 64;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Metrics {
    pub total_cpu_pct: f64,
    pub commit_pct: f64,
    pub avail_mb: f64,
    /// CPU (share of one core) of the foreground app, if known.
    pub fg_cpu_pct: Option<f64>,
}

impl Metrics {
    fn json(&self) -> String {
        let fg = self.fg_cpu_pct.map_or(String::new(), |f| format!(",\"fg_cpu_pct\":{:.1}", f));
        format!("{{\"total_cpu_pct\":{:.1},\"commit_pct\":{:.1},\"avail_mb\":{:.0}{}}}", self.total_cpu_pct, self.commit_pct, self.avail_mb, fg)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Trigger { Cpu, Mem }

impl Trigger {
    pub fn as_str(&self) -> &'static str { match self { Trigger::Cpu => "cpu", Trigger::Mem => "mem" } }
    pub fn parse(s: &str) -> Trigger { if s == "mem" { Trigger::Mem } else { Trigger::Cpu } }
}

/// Which metric a fix was meant to improve. Trim is about memory; so is anything whose alert
/// talks about memory; everything else is about CPU.
pub fn trigger_for(action_label: &str, msg: &str) -> Trigger {
    let m = msg.to_lowercase();
    if action_label == "trim" || ["memory", "commit", "leak", "ram", "private bytes"].iter().any(|w| m.contains(w)) { Trigger::Mem } else { Trigger::Cpu }
}

/// Action label without its argument ("restart-service:foo" -> "restart-service") so tries aggregate.
pub fn action_kind(label: &str) -> String { label.split(':').next().unwrap_or(label).to_string() }

/// Pure scoring: (improvement clamped to -1..1, success).
pub fn score(t: Trigger, b: &Metrics, a: &Metrics) -> (f64, bool) {
    match t {
        Trigger::Cpu => {
            let imp = ((b.total_cpu_pct - a.total_cpu_pct) / b.total_cpu_pct.max(1.0)).clamp(-1.0, 1.0);
            let under = b.total_cpu_pct >= CPU_TRIGGER && a.total_cpu_pct < CPU_TRIGGER;
            (imp, imp >= IMPROVE_OK || under)
        }
        Trigger::Mem => {
            let c = (b.commit_pct - a.commit_pct) / b.commit_pct.max(1.0);
            let m = (a.avail_mb - b.avail_mb) / b.avail_mb.max(1.0);
            let imp = c.max(m).clamp(-1.0, 1.0);
            let under = (b.commit_pct >= COMMIT_TRIGGER && a.commit_pct < COMMIT_TRIGGER) || (b.avail_mb < AVAIL_TRIGGER_MB && a.avail_mb >= AVAIL_TRIGGER_MB);
            (imp, imp >= IMPROVE_OK || under)
        }
    }
}

#[derive(Clone, Debug)]
pub struct Episode {
    pub id: u64,
    pub unix: f64,
    pub kind: String,
    pub action: String,
    pub name: String,
    pub trigger: Trigger,
    pub before: Metrics,
    pub after: Option<Metrics>,
    pub improvement: f64,
    pub success: bool,
    pub undone: bool,
}

impl Episode {
    fn json(&self) -> String {
        let after = self.after.as_ref().map_or(String::new(), |a| format!(",\"after\":{}", a.json()));
        format!("{{\"id\":{},\"unix\":{:.0},\"kind\":\"{}\",\"action\":\"{}\",\"name\":\"{}\",\"trigger\":\"{}\",\"before\":{}{},\"improvement\":{:.3},\"success\":{},\"undone\":{}}}\n",
            self.id, self.unix, esc(&self.kind), esc(&self.action), esc(&self.name), self.trigger.as_str(), self.before.json(), after, self.improvement, self.success, self.undone)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Feedback {
    pub unix: f64,
    /// "undo" | "approve" | "dismiss" | "rule_added"
    pub signal: String,
    pub kind: String,
    pub action: String,
    /// Lowercase image name (a glob pattern for rule_added).
    pub name: String,
    pub trigger: String,
    /// An approve that really executed a fix: the loop starts an episode for it.
    pub start: bool,
}

impl Feedback {
    pub fn new(signal: &str, kind: &str, action: &str, name: &str) -> Feedback {
        Feedback { unix: crate::now_unix(), signal: signal.into(), kind: kind.into(), action: action.into(), name: name.to_lowercase(), trigger: "cpu".into(), start: false }
    }
    pub fn to_line(&self) -> String {
        format!("{{\"unix\":{:.0},\"signal\":\"{}\",\"kind\":\"{}\",\"action\":\"{}\",\"name\":\"{}\",\"trigger\":\"{}\",\"start\":{}}}\n",
            self.unix, esc(&self.signal), esc(&self.kind), esc(&self.action), esc(&self.name), esc(&self.trigger), self.start)
    }
    pub fn parse(line: &str) -> Option<Feedback> {
        Some(Feedback {
            unix: num_field(line, "unix")?,
            signal: str_field(line, "signal")?,
            kind: str_field(line, "kind").unwrap_or_default(),
            action: str_field(line, "action").unwrap_or_default(),
            name: str_field(line, "name").unwrap_or_default().to_lowercase(),
            trigger: str_field(line, "trigger").unwrap_or_default(),
            start: line.contains("\"start\":true"),
        })
    }
}

#[derive(Clone, Debug, Default)]
struct Stat { tries: u32, successes: u32, mean_imp: f64 }

type SKey = (String, String, String);

fn skey(kind: &str, action: &str, name: &str) -> SKey { (kind.to_lowercase(), action.to_lowercase(), name.to_lowercase()) }

pub struct Learner {
    stats: HashMap<SKey, Stat>,
    open: Vec<Episode>,
    feedback: Vec<Feedback>,
    next_id: u64,
    now: f64,
    dir: Option<PathBuf>,
    fb_offset: u64,
    notes: Vec<String>,
    noted: HashSet<String>,
    dirty: bool,
}

impl Learner {
    /// In-memory learner (tests, or when no data dir is wanted).
    pub fn new() -> Learner {
        Learner { stats: HashMap::new(), open: Vec::new(), feedback: Vec::new(), next_id: 1, now: 0.0, dir: None, fb_offset: 0, notes: Vec::new(), noted: HashSet::new(), dirty: false }
    }

    /// Load learn-stats.json and learn-feedback.jsonl from `dir`, and persist there from now on.
    pub fn load(dir: &Path) -> Learner {
        let mut l = Learner::new();
        l.dir = Some(dir.to_path_buf());
        l.now = crate::now_unix();
        for line in std::fs::read_to_string(dir.join("learn-stats.json")).unwrap_or_default().lines() {
            let line = line.trim().trim_end_matches(',');
            if !line.contains("\"tries\":") { continue; }
            let (Some(k), Some(a), Some(n)) = (str_field(line, "kind"), str_field(line, "action"), str_field(line, "name")) else { continue };
            l.stats.insert(skey(&k, &a, &n), Stat {
                tries: num_field(line, "tries").unwrap_or(0.0) as u32,
                successes: num_field(line, "successes").unwrap_or(0.0) as u32,
                mean_imp: num_field(line, "mean_imp").unwrap_or(0.0),
            });
        }
        let fp = dir.join("learn-feedback.jsonl");
        let text = std::fs::read_to_string(&fp).unwrap_or_default();
        l.fb_offset = text.len() as u64;
        for line in text.lines() {
            if let Some(f) = Feedback::parse(line) {
                if l.now - f.unix <= KEEP_FEEDBACK_SECS { l.feedback.push(f); }
            }
        }
        if text.len() as u64 > FEEDBACK_CAP { // shrink to what still matters
            let body: String = l.feedback.iter().map(|f| f.to_line()).collect();
            crate::rules::write_atomic(&fp, &body);
            l.fb_offset = body.len() as u64;
        }
        l
    }

    // ---- recording changes ----

    /// Start tracking a change. Skips duplicates of an episode still open for the same thing.
    pub fn start(&mut self, kind: &str, action: &str, name: &str, trigger: Trigger, now: f64, before: Metrics) {
        self.now = self.now.max(now);
        let (k, a, n) = skey(kind, action, name);
        if self.open.iter().any(|e| e.kind == k && e.action == a && e.name == n) || self.open.len() >= MAX_OPEN { return; }
        let id = self.next_id;
        self.next_id += 1;
        self.open.push(Episode { id, unix: now, kind: k, action: a, name: n, trigger, before, after: None, improvement: 0.0, success: false, undone: false });
    }

    pub fn open_count(&self) -> usize { self.open.len() }

    /// Close episodes whose time is up: measure, score, store. Returns the finished ones.
    pub fn tick(&mut self, now: f64, m: &Metrics) -> Vec<Episode> {
        self.now = self.now.max(now);
        let (due, keep): (Vec<Episode>, Vec<Episode>) = std::mem::take(&mut self.open).into_iter().partition(|e| now - e.unix >= EVAL_AFTER_SECS);
        self.open = keep;
        let mut done = Vec::new();
        for mut e in due {
            let (imp, ok) = score(e.trigger, &e.before, m);
            e.after = Some(m.clone());
            e.improvement = imp;
            e.success = ok;
            self.finish(&e);
            done.push(e);
        }
        if self.dirty { self.flush(); }
        done
    }

    fn finish(&mut self, e: &Episode) {
        let s = self.stats.entry((e.kind.clone(), e.action.clone(), e.name.clone())).or_default();
        s.tries += 1;
        if e.success { s.successes += 1; }
        s.mean_imp += (e.improvement - s.mean_imp) / s.tries as f64;
        self.dirty = true;
        if let Some(d) = &self.dir { append_episode(d, e); }
    }

    /// The user undid it: any episode still open for this name counts as a failure.
    fn fail_open(&mut self, name: &str) {
        let (hit, keep): (Vec<Episode>, Vec<Episode>) = std::mem::take(&mut self.open).into_iter().partition(|e| e.name == name);
        self.open = keep;
        for mut e in hit {
            e.undone = true;
            e.improvement = 0.0;
            e.success = false;
            self.finish(&e);
        }
        if self.dirty { self.flush(); }
    }

    fn flush(&mut self) {
        self.dirty = false;
        let Some(d) = &self.dir else { return };
        let mut keys: Vec<&SKey> = self.stats.keys().collect();
        keys.sort();
        let lines: Vec<String> = keys.iter().map(|k| {
            let s = &self.stats[*k];
            format!("{{\"kind\":\"{}\",\"action\":\"{}\",\"name\":\"{}\",\"tries\":{},\"successes\":{},\"mean_imp\":{:.3}}}", esc(&k.0), esc(&k.1), esc(&k.2), s.tries, s.successes, s.mean_imp)
        }).collect();
        crate::rules::write_atomic(&d.join("learn-stats.json"), &format!("{{\"unix\":{:.0},\"stats\":[\n{}\n]}}\n", self.now, lines.join(",\n")));
    }

    // ---- questions ----

    pub fn tries(&self, kind: &str, action: &str, name: &str) -> u32 { self.stats.get(&skey(kind, action, name)).map_or(0, |s| s.tries) }

    pub fn mean_improvement(&self, kind: &str, action: &str, name: &str) -> Option<f64> {
        self.stats.get(&skey(kind, action, name)).filter(|s| s.tries > 0).map(|s| s.mean_imp)
    }

    fn aggregate(&self, kind: &str, action: &str) -> (u32, u32) {
        let (k, a) = (kind.to_lowercase(), action.to_lowercase());
        self.stats.iter().filter(|(key, _)| key.0 == k && key.1 == a).fold((0, 0), |(t, s), (_, v)| (t + v.tries, s + v.successes))
    }

    /// Laplace-smoothed chance that this action on this name helps; the (kind, action) aggregate
    /// stands in while the name has fewer than MIN_NAME_TRIES tries.
    pub fn success_prob(&self, kind: &str, action: &str, name: &str) -> f64 {
        let (t, s) = match self.stats.get(&skey(kind, action, name)) {
            Some(st) if st.tries >= MIN_NAME_TRIES => (st.tries, st.successes),
            _ => self.aggregate(kind, action),
        };
        (s as f64 + 1.0) / (t as f64 + 2.0)
    }

    // ---- feedback ----

    pub fn add_feedback(&mut self, f: Feedback) {
        self.now = self.now.max(f.unix);
        if f.signal == "undo" { self.fail_open(&f.name); }
        self.feedback.push(f);
        let cut = self.now - KEEP_FEEDBACK_SECS;
        self.feedback.retain(|x| x.unix >= cut);
    }

    pub fn user_protects_at(&self, name: &str, now: f64) -> bool {
        let n = name.to_lowercase();
        let recent = |f: &&Feedback| now - f.unix <= PROTECT_WINDOW_SECS && f.unix <= now;
        let undos = self.feedback.iter().filter(recent).filter(|f| f.signal == "undo" && f.name == n).count();
        let rules = self.feedback.iter().filter(recent).filter(|f| f.signal == "rule_added" && crate::rules::glob_match(&f.name, &n)).count();
        undos >= PROTECT_UNDOS && undos > rules
    }

    /// The user undid this name at least twice in 14 days and has not answered with a rule.
    pub fn user_protects(&self, name: &str) -> bool { self.user_protects_at(name, self.now) }

    /// approve / (approve + dismiss) for this kind of fix; 0.5 with no signals yet.
    pub fn user_trusts(&self, kind: &str, action: &str) -> f64 {
        let (k, a) = (kind.to_lowercase(), action_kind(action).to_lowercase());
        let count = |sig: &str| self.feedback.iter().filter(|f| f.signal == sig && f.kind.to_lowercase() == k && action_kind(&f.action).to_lowercase() == a).count() as f64;
        let (ap, di) = (count("approve"), count("dismiss"));
        if ap + di == 0.0 { 0.5 } else { ap / (ap + di) }
    }

    /// What the governor should know this tick. Also queues a one-time note per blocked name.
    pub fn hints(&mut self) -> Hints {
        let mut h = Hints::default();
        let (t, s) = self.aggregate("governor", "demote");
        h.default_prob = (s as f64 + 1.0) / (t as f64 + 2.0);
        let mut new_notes = Vec::new();
        for (k, st) in &self.stats {
            if k.0 != "governor" || k.1 != "demote" || k.2.is_empty() || st.tries < MIN_NAME_TRIES { continue; }
            let p = (st.successes as f64 + 1.0) / (st.tries as f64 + 2.0);
            h.prob.insert(k.2.clone(), p);
            if st.tries >= BLOCK_TRIES && p < BLOCK_PROB {
                h.blocked.insert(k.2.clone());
                if !self.noted.contains(&k.2) {
                    new_notes.push((k.2.clone(), format!("not slowing {} down any more: it did not help in {} of {} tries", k.2, st.tries - st.successes, st.tries)));
                }
            }
        }
        for (n, msg) in new_notes { self.noted.insert(n); self.notes.push(msg); }
        let names: HashSet<&str> = self.feedback.iter().filter(|f| f.signal == "undo").map(|f| f.name.as_str()).collect();
        for n in names {
            if self.user_protects(n) { h.protect.insert(n.to_string()); }
        }
        h
    }

    /// Messages for the ledger (why a name is no longer demoted), each given once.
    pub fn take_notes(&mut self) -> Vec<String> { std::mem::take(&mut self.notes) }

    // ---- files shared with the CLI processes ----

    /// Read feedback lines other processes appended; approves that ran a fix start an episode.
    pub fn poll_feedback(&mut self, now: f64, m: &Metrics) {
        let Some(d) = self.dir.clone() else { return };
        let text = std::fs::read_to_string(d.join("learn-feedback.jsonl")).unwrap_or_default();
        if (text.len() as u64) < self.fb_offset { self.fb_offset = 0; }
        let new = text.get(self.fb_offset as usize..).unwrap_or("").to_string();
        self.fb_offset = text.len() as u64;
        for line in new.lines() {
            let Some(f) = Feedback::parse(line) else { continue };
            if f.signal == "approve" && f.start { self.start("fix", &action_kind(&f.action), &f.name, Trigger::parse(&f.trigger), now, m.clone()); }
            self.add_feedback(f);
        }
    }
}

/// Append one user signal for the watch loop to pick up (safe from any process).
pub fn append_feedback(dir: &Path, f: &Feedback) {
    let _ = std::fs::create_dir_all(dir);
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("learn-feedback.jsonl")) {
        let _ = file.write_all(f.to_line().as_bytes());
    }
}

fn append_episode(dir: &Path, e: &Episode) {
    let path = dir.join("learn.jsonl");
    if std::fs::metadata(&path).map(|m| m.len() > LEARN_CAP).unwrap_or(false) {
        let _ = std::fs::rename(&path, dir.join("learn.jsonl.old"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(e.json().as_bytes());
    }
}

#[cfg(test)]
mod learn_tests {
    use super::*;

    fn m(cpu: f64, commit: f64, avail: f64) -> Metrics { Metrics { total_cpu_pct: cpu, commit_pct: commit, avail_mb: avail, fg_cpu_pct: None } }

    #[test]
    fn cpu_scoring() {
        let (imp, ok) = score(Trigger::Cpu, &m(100.0, 50.0, 8000.0), &m(70.0, 50.0, 8000.0));
        assert!((imp - 0.3).abs() < 1e-9 && ok);
        let (_, ok) = score(Trigger::Cpu, &m(80.0, 50.0, 8000.0), &m(74.0, 50.0, 8000.0));
        assert!(ok, "back under the threshold counts even with a small drop");
        let (imp, ok) = score(Trigger::Cpu, &m(90.0, 50.0, 8000.0), &m(85.0, 50.0, 8000.0));
        assert!(imp < 0.2 && !ok);
        let (imp, ok) = score(Trigger::Cpu, &m(50.0, 50.0, 8000.0), &m(70.0, 50.0, 8000.0));
        assert!(imp < 0.0 && !ok);
    }

    #[test]
    fn memory_scoring() {
        let (_, ok) = score(Trigger::Mem, &m(10.0, 90.0, 1000.0), &m(10.0, 70.0, 1000.0));
        assert!(ok, "commit dropped 22%");
        let (_, ok) = score(Trigger::Mem, &m(10.0, 60.0, 1500.0), &m(10.0, 60.0, 2100.0));
        assert!(ok, "available memory rose 40%");
        let (_, ok) = score(Trigger::Mem, &m(10.0, 60.0, 5000.0), &m(10.0, 59.0, 5100.0));
        assert!(!ok);
    }

    #[test]
    fn trigger_choice() {
        assert_eq!(trigger_for("trim", ""), Trigger::Mem);
        assert_eq!(trigger_for("terminate", "possible memory leak in x"), Trigger::Mem);
        assert_eq!(trigger_for("lower-priority", "CPU pegged"), Trigger::Cpu);
        assert_eq!(action_kind("restart-service:foo"), "restart-service");
    }

    #[test]
    fn episode_closes_after_180s_and_updates_stats() {
        let mut l = Learner::new();
        l.start("governor", "demote", "FFmpeg.exe", Trigger::Cpu, 1000.0, m(100.0, 50.0, 8000.0));
        l.start("governor", "demote", "ffmpeg.exe", Trigger::Cpu, 1001.0, m(100.0, 50.0, 8000.0)); // duplicate
        assert_eq!(l.open_count(), 1);
        assert!(l.tick(1100.0, &m(40.0, 50.0, 8000.0)).is_empty());
        let done = l.tick(1180.0, &m(40.0, 50.0, 8000.0));
        assert_eq!(done.len(), 1);
        assert!(done[0].success && (done[0].improvement - 0.6).abs() < 1e-9);
        assert_eq!(l.tries("governor", "demote", "ffmpeg.exe"), 1);
        assert!((l.mean_improvement("governor", "demote", "FFMPEG.EXE").unwrap() - 0.6).abs() < 1e-9);
    }

    #[test]
    fn laplace_and_aggregate_fallback() {
        let mut l = Learner::new();
        assert!((l.success_prob("governor", "demote", "a.exe") - 0.5).abs() < 1e-9);
        // 4 tries of a.exe: 3 successes, 1 failure -> (3+1)/(4+2)
        for (i, ok) in [true, true, true, false].iter().enumerate() {
            l.start("governor", "demote", "a.exe", Trigger::Cpu, i as f64 * 1000.0, m(100.0, 0.0, 0.0));
            l.tick(i as f64 * 1000.0 + 200.0, &m(if *ok { 10.0 } else { 100.0 }, 0.0, 0.0));
        }
        assert!((l.success_prob("governor", "demote", "a.exe") - 4.0 / 6.0).abs() < 1e-9);
        // b.exe has no tries: falls back to the aggregate (3+1)/(4+2)
        assert!((l.success_prob("governor", "demote", "b.exe") - 4.0 / 6.0).abs() < 1e-9);
        // a different action is not mixed in
        assert!((l.success_prob("fix", "trim", "a.exe") - 0.5).abs() < 1e-9);
    }

    #[test]
    fn name_with_few_tries_uses_aggregate_not_its_own() {
        let mut l = Learner::new();
        for i in 0..2 { // 2 failures of x.exe
            l.start("governor", "demote", "x.exe", Trigger::Cpu, i as f64 * 1000.0, m(100.0, 0.0, 0.0));
            l.tick(i as f64 * 1000.0 + 200.0, &m(100.0, 0.0, 0.0));
        }
        assert!((l.success_prob("governor", "demote", "x.exe") - 1.0 / 4.0).abs() < 1e-9); // aggregate (0+1)/(2+2)
    }

    #[test]
    fn five_failures_block_the_name() {
        let mut l = Learner::new();
        for i in 0..5 {
            l.start("governor", "demote", "stubborn.exe", Trigger::Cpu, i as f64 * 1000.0, m(100.0, 0.0, 0.0));
            l.tick(i as f64 * 1000.0 + 200.0, &m(100.0, 0.0, 0.0));
        }
        let h = l.hints();
        assert!(h.blocked.contains("stubborn.exe"));
        assert_eq!(l.take_notes().len(), 1);
        l.hints();
        assert!(l.take_notes().is_empty(), "the reason is logged once");
    }

    fn fb(sig: &str, name: &str, unix: f64) -> Feedback { Feedback { unix, signal: sig.into(), kind: "fix".into(), action: "trim".into(), name: name.into(), trigger: "cpu".into(), start: false } }

    #[test]
    fn protects_after_two_undos_in_14_days() {
        let day = 86400.0;
        let mut l = Learner::new();
        l.add_feedback(fb("undo", "game.exe", 1000.0));
        assert!(!l.user_protects_at("game.exe", 1000.0 + day));
        l.add_feedback(fb("undo", "game.exe", 1000.0 + day));
        assert!(l.user_protects_at("Game.exe", 1000.0 + 2.0 * day));
        assert!(!l.user_protects_at("game.exe", 1000.0 + 20.0 * day), "outside the 14-day window");
        // undo count must exceed rule_added count
        l.add_feedback(fb("rule_added", "game*.exe", 1000.0 + 2.0 * day));
        assert!(l.user_protects_at("game.exe", 1000.0 + 3.0 * day));
        l.add_feedback(fb("rule_added", "game.exe", 1000.0 + 2.5 * day));
        assert!(!l.user_protects_at("game.exe", 1000.0 + 3.0 * day));
        assert!(l.hints().protect.is_empty());
    }

    #[test]
    fn undo_fails_open_episode() {
        let mut l = Learner::new();
        l.start("governor", "demote", "x.exe", Trigger::Cpu, 0.0, m(100.0, 0.0, 0.0));
        l.add_feedback(fb("undo", "x.exe", 10.0));
        assert_eq!(l.open_count(), 0);
        assert_eq!(l.tries("governor", "demote", "x.exe"), 1);
        assert!(l.mean_improvement("governor", "demote", "x.exe").unwrap().abs() < 1e-9);
    }

    #[test]
    fn trust_from_approve_and_dismiss() {
        let mut l = Learner::new();
        assert!((l.user_trusts("fix", "trim") - 0.5).abs() < 1e-9);
        for _ in 0..3 { l.add_feedback(fb("approve", "a.exe", 1.0)); }
        l.add_feedback(fb("dismiss", "b.exe", 2.0));
        assert!((l.user_trusts("fix", "trim") - 0.75).abs() < 1e-9);
        assert!((l.user_trusts("fix", "terminate") - 0.5).abs() < 1e-9);
    }

    #[test]
    fn feedback_line_round_trips() {
        let mut f = fb("approve", "a b.exe", 123.0);
        f.start = true;
        f.trigger = "mem".into();
        assert_eq!(Feedback::parse(&f.to_line()), Some(f));
    }
}
