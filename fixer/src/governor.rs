//! The governor: keeps the PC responsive when the CPU is pegged, like Process Lasso's
//! ProBalance, but gentler and self-explaining.
//!
//! Every tick (1 s) it measures CPU per process. When the machine is busy (total >= BUSY_PCT
//! for BUSY_TICKS ticks), it demotes the biggest background CPU users one step at a time:
//!   level 1: efficiency mode (EcoQoS) + low I/O priority
//!   level 2: below-normal priority
//!   level 3: idle priority + low memory priority
//! It never touches the foreground app (or other processes with its image name), the
//! never-touch list (policy::denied), our own processes, or anything a user rule pins.
//! When the machine is calm (total < CALM_PCT for CALM_TICKS ticks) it restores one step per
//! RESTORE_EVERY ticks; a demoted process that comes to the foreground is restored at once.
//! Every change is recorded (ledger) with the reason in plain words, and every change is
//! re-checked against pid + create time + name right before it is made.
//!
//! Pure decision logic lives in `decide` so it can be unit-tested without Windows.

use crate::policy;
use crate::sys::{self, Proc};
use crate::{Action, Target};
use std::collections::{HashMap, HashSet};

pub const TICK_SECS: f64 = 1.0;
pub const BUSY_PCT: f64 = 75.0;
pub const BUSY_TICKS: u32 = 3;
pub const CALM_PCT: f64 = 50.0;
pub const CALM_TICKS: u32 = 10;
pub const RESTORE_EVERY: u32 = 5;
/// A process must use at least this share of ONE core to be worth demoting.
pub const MIN_CORE_PCT: f64 = 25.0;
pub const MAX_DEMOTIONS_PER_TICK: usize = 2;
pub const MAX_LEVEL: u8 = 3;

type Key = (u32, u64); // pid, create time: survives pid reuse

#[derive(Clone, Debug, PartialEq)]
pub struct Demotion {
    pub pid: u32,
    pub created: u64,
    pub name: String,
    pub level: u8,
    pub orig_priority: u32,
    pub since: f64,
    pub reason: String,
}

/// What the governor will do this tick. Pure output of `decide`.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Demote { key: Key, name: String, to_level: u8, reason: String },
    Restore { key: Key, name: String, to_level: u8, reason: String },
    Forget { key: Key },
}

/// One process as seen this tick.
#[derive(Clone, Debug)]
pub struct Seen { pub key: Key, pub name: String, pub core_pct: f64, pub foreground: bool, pub protected: bool }

/// Counters that carry between ticks.
#[derive(Clone, Debug, Default)]
pub struct Mood { pub busy_ticks: u32, pub calm_ticks: u32 }

impl Mood {
    pub fn update(&mut self, total_pct: f64) {
        if total_pct >= BUSY_PCT { self.busy_ticks += 1; } else { self.busy_ticks = 0; }
        if total_pct < CALM_PCT { self.calm_ticks += 1; } else { self.calm_ticks = 0; }
    }
    pub fn busy(&self) -> bool { self.busy_ticks >= BUSY_TICKS }
    pub fn calm(&self) -> bool { self.calm_ticks >= CALM_TICKS }
}

/// What the learner knows, as plain data so `decide_with` stays pure. Names are lowercase.
#[derive(Clone, Debug)]
pub struct Hints {
    /// Names the user keeps undoing: never demoted.
    pub protect: HashSet<String>,
    /// Names demoting did not help for (enough tries, low success chance): never demoted.
    pub blocked: HashSet<String>,
    /// Chance that demoting this name helps; others get `default_prob`.
    pub prob: HashMap<String, f64>,
    pub default_prob: f64,
}

impl Default for Hints {
    fn default() -> Self { Hints { protect: HashSet::new(), blocked: HashSet::new(), prob: HashMap::new(), default_prob: 1.0 } }
}

/// Pure policy without learning hints.
pub fn decide(total_pct: f64, mood: &Mood, seen: &[Seen], demoted: &HashMap<Key, Demotion>) -> Vec<Step> {
    decide_with(total_pct, mood, seen, demoted, &Hints::default())
}

/// Pure policy. `seen` is every live process this tick; `demoted` the current demotions.
pub fn decide_with(total_pct: f64, mood: &Mood, seen: &[Seen], demoted: &HashMap<Key, Demotion>, hints: &Hints) -> Vec<Step> {
    let mut out = Vec::new();
    let live: HashMap<Key, &Seen> = seen.iter().map(|s| (s.key, s)).collect();
    let fg_names: Vec<&str> = seen.iter().filter(|s| s.foreground).map(|s| s.name.as_str()).collect();

    // Restores first: gone, foreground, or calm.
    for (k, d) in demoted {
        match live.get(k) {
            None => out.push(Step::Forget { key: *k }),
            Some(s) if s.foreground || fg_names.contains(&s.name.as_str()) =>
                out.push(Step::Restore { key: *k, name: d.name.clone(), to_level: 0, reason: format!("{} is now the app you are using", d.name) }),
            Some(_) if mood.calm() && mood.calm_ticks % RESTORE_EVERY == 0 =>
                out.push(Step::Restore { key: *k, name: d.name.clone(), to_level: d.level - 1, reason: format!("CPU is calm again ({:.0}%)", total_pct) }),
            _ => {}
        }
    }
    if !mood.busy() { return out; }

    // Demote the biggest background users, one step each, a couple per tick.
    let mut cands: Vec<&Seen> = seen.iter()
        .filter(|s| !s.protected && !s.foreground && !fg_names.contains(&s.name.as_str()))
        .filter(|s| s.core_pct >= MIN_CORE_PCT)
        .filter(|s| { let n = s.name.to_lowercase(); !hints.protect.contains(&n) && !hints.blocked.contains(&n) })
        .filter(|s| demoted.get(&s.key).map_or(true, |d| d.level < MAX_LEVEL))
        .collect();
    // Biggest user first, weighted by how often demoting that name has helped.
    let weight = |s: &Seen| s.core_pct * hints.prob.get(&s.name.to_lowercase()).copied().unwrap_or(hints.default_prob);
    cands.sort_by(|a, b| weight(b).partial_cmp(&weight(a)).unwrap_or(std::cmp::Ordering::Equal));
    for s in cands.into_iter().take(MAX_DEMOTIONS_PER_TICK) {
        let lvl = demoted.get(&s.key).map_or(0, |d| d.level) + 1;
        out.push(Step::Demote { key: s.key, name: s.name.clone(), to_level: lvl,
            reason: format!("CPU at {:.0}% and {} was using {:.0}% of a core in the background", total_pct, s.name, s.core_pct) });
    }
    out
}

/// Apply one level change to a live process. Level 0 = original state.
fn apply_level(pid: u32, from: u8, to: u8, orig_priority: u32) -> Result<(), String> {
    // Going up the ladder.
    if from < 1 && to >= 1 { sys::set_efficiency(pid, true)?; let _ = sys::set_io_priority(pid, 1); }
    if from < 2 && to >= 2 { sys::set_priority(pid, sys::BELOW_NORMAL)?; }
    if from < 3 && to >= 3 { sys::set_priority(pid, sys::IDLE)?; let _ = sys::set_memory_priority(pid, 2); }
    // Coming down.
    if from >= 3 && to < 3 { let _ = sys::set_memory_priority(pid, 5); sys::set_priority(pid, if to >= 2 { sys::BELOW_NORMAL } else { orig_priority })?; }
    if from >= 2 && to < 2 { sys::set_priority(pid, orig_priority)?; }
    if from >= 1 && to < 1 { let _ = sys::set_io_priority(pid, 2); sys::set_efficiency(pid, false)?; }
    Ok(())
}

pub struct Governor {
    buf: Vec<u8>,
    prev: HashMap<Key, (u64, f64)>, // cpu time, wall time
    pub mood: Mood,
    pub demoted: HashMap<Key, Demotion>,
    protected_cache: HashMap<Key, bool>,
    cores: f64,
    extra_deny: Vec<String>,
    pinned: Vec<String>, // image names a user rule manages; the governor leaves them alone
    rules: Vec<crate::rules::Rule>, // user rules; any process a rule covers (wildcards too) is pinned
    undone: HashSet<Key>, // processes the user restored by hand; left alone from then on
    last_procs: Vec<Proc>, // the snapshot of the latest tick, for the rule enforcer
    /// Total CPU use measured on the latest tick (0-100).
    pub total_pct: f64,
    /// CPU (share of one core) of the foreground app on the latest tick, if there is one.
    pub fg_cpu_pct: Option<f64>,
    /// Busiest background-or-not processes of the latest tick (lowercase name, core %), biggest first, at most 5.
    pub top: Vec<(String, f64)>,
    /// What the learner suggests; refreshed by the watch loop.
    pub hints: Hints,
    /// Whether current demotions were really applied (false = dry run bookkeeping only).
    pub acting: bool,
}

/// One change made, for the ledger and the tray.
pub struct Change { pub pid: u32, pub name: String, pub from: u8, pub to: u8, pub reason: String, pub result: Result<(), String> }

impl Governor {
    pub fn new(extra_deny: Vec<String>, pinned: Vec<String>) -> Self {
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64;
        Governor { buf: Vec::new(), prev: HashMap::new(), mood: Mood::default(), demoted: HashMap::new(),
            protected_cache: HashMap::new(), cores, extra_deny, pinned, rules: Vec::new(), undone: HashSet::new(), last_procs: Vec::new(), total_pct: 0.0, fg_cpu_pct: None, top: Vec::new(), hints: Hints::default(), acting: false }
    }

    fn protected(&mut self, p: &Proc) -> bool {
        let key = (p.pid, p.created);
        if let Some(&b) = self.protected_cache.get(&key) { return b; }
        let name = p.name.to_lowercase();
        let mut b = self.pinned.iter().any(|x| *x == name) || crate::rules::is_pinned(&self.rules, &name);
        if !b {
            // Only read the command line for real candidates; it is the expensive part.
            let t = Target { pid: p.pid, name: p.name.clone(), age_secs: f64::MAX, cmdline: sys::cmdline(p.pid), service: None, private_mb: 0.0 };
            b = policy::denied(&t, &Action::LowerPriority, &self.extra_deny).is_some();
        }
        self.protected_cache.insert(key, b);
        b
    }

    /// One tick: measure, decide, act. Returns the changes made (empty most of the time).
    /// `act` = false logs decisions without changing anything (dry run / paused).
    pub fn tick(&mut self, now: f64, act: bool) -> Vec<Change> {
        let procs = sys::snapshot(&mut self.buf);
        let fg = sys::foreground_pid();
        let mut seen = Vec::with_capacity(procs.len());
        let mut busy_sum = 0.0;
        let mut next_prev = HashMap::with_capacity(procs.len());
        for p in &procs {
            let key = (p.pid, p.created);
            let core_pct = match self.prev.get(&key) {
                Some(&(c0, t0)) if now > t0 => (p.cpu.saturating_sub(c0) as f64 / 1e7) / (now - t0) * 100.0,
                _ => 0.0,
            };
            next_prev.insert(key, (p.cpu, now));
            if p.pid == 0 { continue; } // idle
            busy_sum += core_pct;
            let foreground = Some(p.pid) == fg;
            // Cheap filter before the protected check, which may read a command line.
            let protected = if core_pct >= MIN_CORE_PCT || self.demoted.contains_key(&key) { self.protected(p) } else { true };
            let protected = protected || self.undone.contains(&key);
            seen.push(Seen { key, name: p.name.clone(), core_pct, foreground, protected });
        }
        self.prev = next_prev;
        self.protected_cache.retain(|k, _| self.prev.contains_key(k));
        self.undone.retain(|k| self.prev.contains_key(k));
        let total_pct = (busy_sum / self.cores).min(100.0);
        self.total_pct = total_pct;
        self.mood.update(total_pct);
        self.fg_cpu_pct = seen.iter().find(|s| s.foreground).map(|s| s.core_pct);
        let mut top: Vec<(String, f64)> = seen.iter().filter(|s| s.core_pct >= 5.0).map(|s| (s.name.to_lowercase(), s.core_pct)).collect();
        top.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        top.truncate(5);
        self.top = top;

        let mut changes = Vec::new();
        for step in decide_with(total_pct, &self.mood, &seen, &self.demoted, &self.hints) {
            match step {
                Step::Forget { key } => { self.demoted.remove(&key); }
                Step::Demote { key, name, to_level, reason } => {
                    let from = self.demoted.get(&key).map_or(0, |d| d.level);
                    let orig = self.demoted.get(&key).map(|d| d.orig_priority).or_else(|| sys::get_priority(key.0)).unwrap_or(sys::NORMAL);
                    let result = if !act { Ok(()) } else if !sys::image_matches(key.0, key.1, &name) { Err("pid reused".into()) } else { apply_level(key.0, from, to_level, orig) };
                    if result.is_ok() { // dry run tracks levels too, so it logs each step once, not every tick
                        let e = self.demoted.entry(key).or_insert(Demotion { pid: key.0, created: key.1, name: name.clone(), level: 0, orig_priority: orig, since: now, reason: reason.clone() });
                        e.level = to_level; e.reason = reason.clone();
                    }
                    changes.push(Change { pid: key.0, name, from, to: to_level, reason, result });
                }
                Step::Restore { key, name, to_level, reason } => {
                    let Some(d) = self.demoted.get(&key).cloned() else { continue };
                    let result = if !act { Ok(()) } else if !sys::image_matches(key.0, key.1, &name) { Err("pid reused".into()) } else { apply_level(key.0, d.level, to_level, d.orig_priority) };
                    if to_level == 0 { self.demoted.remove(&key); } else if let Some(e) = self.demoted.get_mut(&key) { e.level = to_level; }
                    changes.push(Change { pid: key.0, name, from: d.level, to: to_level, reason, result });
                }
            }
        }
        self.last_procs = procs;
        changes
    }

    /// Replace the user rules (processes they cover become pinned).
    pub fn set_rules(&mut self, rules: Vec<crate::rules::Rule>) { self.rules = rules; self.protected_cache.clear(); }

    /// The process snapshot taken by the latest tick.
    pub fn procs(&self) -> &[Proc] { &self.last_procs }

    /// Restore every demotion of this pid fully, and leave it alone from now on.
    pub fn undo(&mut self, pid: u32, act: bool) -> Vec<Change> {
        let keys: Vec<Key> = self.demoted.keys().filter(|k| k.0 == pid).cloned().collect();
        let mut out = Vec::new();
        for k in keys {
            let Some(d) = self.demoted.remove(&k) else { continue };
            self.undone.insert(k);
            let result = if !act { Ok(()) } else if !sys::image_matches(k.0, k.1, &d.name) { Err("pid reused".into()) } else { apply_level(k.0, d.level, 0, d.orig_priority) };
            out.push(Change { pid, name: d.name, from: d.level, to: 0, reason: "you asked to undo it".into(), result });
        }
        out
    }

    /// Put everything back, e.g. on exit or pause.
    pub fn restore_all(&mut self, act: bool) -> Vec<Change> {
        let all: Vec<(Key, Demotion)> = self.demoted.drain().collect();
        all.into_iter().map(|(k, d)| {
            let result = if !act { Ok(()) } else if sys::image_matches(k.0, k.1, &d.name) { apply_level(k.0, d.level, 0, d.orig_priority) } else { Ok(()) };
            Change { pid: k.0, name: d.name, from: d.level, to: 0, reason: "governor stopped or paused".into(), result }
        }).collect()
    }
}

#[cfg(test)]
mod gov_tests {
    use super::*;
    fn s(pid: u32, name: &str, pct: f64) -> Seen { Seen { key: (pid, 1), name: name.into(), core_pct: pct, foreground: false, protected: false } }

    #[test]
    fn quiet_machine_does_nothing() {
        let m = Mood { busy_ticks: 0, calm_ticks: 0 };
        assert!(decide(40.0, &m, &[s(1, "ffmpeg.exe", 400.0)], &HashMap::new()).is_empty());
    }

    #[test]
    fn busy_demotes_biggest_background_first_one_step() {
        let m = Mood { busy_ticks: BUSY_TICKS, calm_ticks: 0 };
        let seen = vec![s(1, "ffmpeg.exe", 450.0), s(2, "comet.exe", 70.0), s(3, "small.exe", 10.0)];
        let steps = decide(95.0, &m, &seen, &HashMap::new());
        assert_eq!(steps.len(), 2);
        assert!(matches!(&steps[0], Step::Demote { key: (1, 1), to_level: 1, .. }));
        assert!(matches!(&steps[1], Step::Demote { key: (2, 1), to_level: 1, .. }));
    }

    #[test]
    fn never_touches_foreground_name_or_protected() {
        let m = Mood { busy_ticks: BUSY_TICKS, calm_ticks: 0 };
        let mut fg = s(1, "comet.exe", 80.0); fg.foreground = true;
        let other_tab = s(2, "comet.exe", 90.0);
        let mut prot = s(3, "dwm.exe", 90.0); prot.protected = true;
        let steps = decide(95.0, &m, &[fg, other_tab, prot], &HashMap::new());
        assert!(steps.is_empty(), "{:?}", steps);
    }

    #[test]
    fn foreground_restores_at_once_and_calm_restores_stepwise() {
        let mut d = HashMap::new();
        d.insert((1, 1), Demotion { pid: 1, created: 1, name: "ffmpeg.exe".into(), level: 3, orig_priority: sys::NORMAL, since: 0.0, reason: String::new() });
        let mut fg = s(1, "ffmpeg.exe", 300.0); fg.foreground = true;
        let steps = decide(90.0, &Mood { busy_ticks: 5, calm_ticks: 0 }, &[fg], &d);
        assert!(matches!(&steps[0], Step::Restore { to_level: 0, .. }));
        let steps = decide(20.0, &Mood { busy_ticks: 0, calm_ticks: CALM_TICKS + (RESTORE_EVERY - CALM_TICKS % RESTORE_EVERY) % RESTORE_EVERY }, &[s(1, "ffmpeg.exe", 5.0)], &d);
        assert!(matches!(&steps[0], Step::Restore { to_level: 2, .. }));
    }

    #[test]
    fn hints_protect_block_and_weight() {
        let m = Mood { busy_ticks: BUSY_TICKS, calm_ticks: 0 };
        let seen = vec![s(1, "A.exe", 300.0), s(2, "b.exe", 200.0), s(3, "c.exe", 100.0)];
        let mut h = Hints::default();
        h.protect.insert("a.exe".into());
        let steps = decide_with(95.0, &m, &seen, &HashMap::new(), &h);
        assert!(matches!(&steps[0], Step::Demote { key: (2, 1), .. }) && matches!(&steps[1], Step::Demote { key: (3, 1), .. }));
        let mut h = Hints::default();
        h.blocked.insert("b.exe".into());
        let steps = decide_with(95.0, &m, &seen, &HashMap::new(), &h);
        assert!(!steps.iter().any(|x| matches!(x, Step::Demote { key: (2, 1), .. })));
        // a never-helpful big user (0.2) is tried after a smaller one that always helps (0.9)
        let mut h = Hints::default();
        h.prob.insert("a.exe".into(), 0.2);
        h.prob.insert("c.exe".into(), 0.9);
        let steps = decide_with(95.0, &m, &seen, &HashMap::new(), &h);
        assert!(matches!(&steps[0], Step::Demote { key: (2, 1), .. }), "b 200*1.0 first");
        assert!(matches!(&steps[1], Step::Demote { key: (3, 1), .. }), "c 100*0.9 beats a 300*0.2");
    }

    #[test]
    fn gone_process_is_forgotten_and_max_level_not_exceeded() {
        let mut d = HashMap::new();
        d.insert((9, 1), Demotion { pid: 9, created: 1, name: "gone.exe".into(), level: 1, orig_priority: sys::NORMAL, since: 0.0, reason: String::new() });
        d.insert((1, 1), Demotion { pid: 1, created: 1, name: "ffmpeg.exe".into(), level: MAX_LEVEL, orig_priority: sys::NORMAL, since: 0.0, reason: String::new() });
        let steps = decide(95.0, &Mood { busy_ticks: BUSY_TICKS, calm_ticks: 0 }, &[s(1, "ffmpeg.exe", 400.0)], &d);
        assert!(steps.contains(&Step::Forget { key: (9, 1) }));
        assert!(!steps.iter().any(|x| matches!(x, Step::Demote { key: (1, 1), .. })));
    }
}
