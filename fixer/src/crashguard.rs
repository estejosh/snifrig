//! Crash guard: the last line of defense against the PC running out of memory.
//!
//! On 16 Nov 2026 a python.exe leaking 12 GB an hour, on top of a kernel token leak, took a 64 GB
//! PC to its commit limit and it died. The monitor saw it coming for 45 minutes and only wrote
//! alerts. This module acts, every two seconds, from numbers the fixer already has:
//! - warn: commit is at WARN_PCT or more and one app is growing fast. A card names the app and
//!   how long until memory runs out.
//! - freeze: commit is at FREEZE_PCT or more and still rising, or at SOON_PCT or more and the limit
//!   is under SOON_MIN minutes away. The fastest-growing app is suspended. Nothing is closed and
//!   nothing is lost: Resume undoes it, End closes it.
//! - no culprit: commit is climbing and no single app is growing (kernel leak, spawn loop). Said
//!   plainly, because freezing a random app would not help.
//! It overrides keep=yes ("don't slow it down" is not "let it crash the PC") but never touches the
//! window you are using, the never-touch list, snifrig itself, or an app you just resumed.
//! Pure: the caller reads the system and does the suspend.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

pub const WARN_PCT: f64 = 85.0;
pub const SOON_PCT: f64 = 88.0;
pub const FREEZE_PCT: f64 = 95.0;
/// Freeze early when the limit is closer than this.
pub const SOON_MIN: f64 = 10.0;
/// Commit counts as rising above this many percent of the limit per minute.
pub const RISING_PCT_MIN: f64 = 0.05;
/// An app is a culprit only if it grows at least this fast.
pub const MIN_GROWTH_MB_H: f64 = 300.0;
/// Warn about an app growing at least this fast.
pub const FAST_GROWTH_MB_H: f64 = 1000.0;
/// Only apps holding this much are tracked (keeps the guard cheap).
pub const TRACK_MB: f64 = 256.0;
pub const WINDOW_SECS: f64 = 180.0;
pub const MIN_SPAN_SECS: f64 = 30.0;
/// A grower must have been watched this long (a model load or a build spikes for a minute, then stops).
pub const GROWER_SPAN_SECS: f64 = 120.0;
/// ...and have grown in at least this share of its samples.
pub const STEPS_UP: f64 = 0.7;
/// Never freeze a process younger than this: loading a model, starting a build.
pub const MIN_AGE_SECS: f64 = 600.0;
/// After one copy of an app was frozen, other copies are only warned about for this long.
pub const SAME_APP_SECS: f64 = 7200.0;
pub const FREEZE_EVERY_SECS: f64 = 30.0;
pub const WARN_EVERY_SECS: f64 = 900.0;
/// After you resume an app, it is left alone this long.
pub const RESUMED_GRACE_SECS: f64 = 1800.0;
pub const MAX_FROZEN: usize = 3;
pub const FILE: &str = "frozen.txt";

#[derive(Clone, Debug, PartialEq)]
pub struct Seen { pub pid: u32, pub created: u64, pub name: String, pub private_mb: f64 }

/// FILETIME (100 ns since 1601) to unix seconds.
pub fn created_unix(created: u64) -> f64 { created as f64 / 1e7 - 11644473600.0 }

#[derive(Clone, Debug, PartialEq)]
pub enum State { Frozen, Resumed }

#[derive(Clone, Debug, PartialEq)]
pub struct Entry { pub state: State, pub pid: u32, pub created: u64, pub unix: f64, pub mb: f64, pub name: String }

#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Warn { name: String, pid: u32, mb: f64, growth_mb_h: f64, commit_pct: f64, eta_min: Option<f64> },
    Freeze { name: String, pid: u32, created: u64, mb: f64, growth_mb_h: f64, commit_pct: f64, eta_min: Option<f64> },
    /// Nothing snifrig may freeze is growing. `top`: the fastest grower it may not freeze (WSL's
    /// VM, the window in front, a system process), if there is one.
    NoCulprit { commit_pct: f64, eta_min: Option<f64>, top: Option<(String, f64)> },
}

#[derive(Default)]
pub struct Guard {
    hist: HashMap<(u32, u64), VecDeque<(f64, f64)>>,
    commit: VecDeque<(f64, f64)>,
    last_warn: f64,
    last_freeze: f64,
    pub entries: Vec<Entry>,
    /// Frozen entries whose process ended since the last tick (the caller removes their cards).
    pub gone: Vec<Entry>,
    /// App name (lowercase) -> when a copy of it was last frozen.
    frozen_names: std::collections::HashMap<String, f64>,
}

/// Growth (MB/h) of a sustained grower: watched GROWER_SPAN_SECS, rising in STEPS_UP of samples.
fn sustained(v: &VecDeque<(f64, f64)>) -> Option<f64> {
    let (t0, t1) = (v.front()?.0, v.back()?.0);
    if t1 - t0 < GROWER_SPAN_SECS || v.len() < 10 { return None; }
    let ups = v.iter().zip(v.iter().skip(1)).filter(|(a, b)| b.1 > a.1).count();
    if (ups as f64) < STEPS_UP * (v.len() - 1) as f64 { return None; }
    slope(v).map(|s| s * 3600.0)
}

/// Least-squares slope (units per second) of (t, v) points; None without MIN_SPAN_SECS of data.
fn slope(v: &VecDeque<(f64, f64)>) -> Option<f64> {
    if v.len() < 3 { return None; }
    let (t0, t1) = (v.front()?.0, v.back()?.0);
    if t1 - t0 < MIN_SPAN_SECS { return None; }
    let n = v.len() as f64;
    let mt = v.iter().map(|p| p.0).sum::<f64>() / n;
    let mv = v.iter().map(|p| p.1).sum::<f64>() / n;
    let (mut num, mut den) = (0.0, 0.0);
    for (t, x) in v { num += (t - mt) * (x - mv); den += (t - mt) * (t - mt); }
    if den <= 0.0 { None } else { Some(num / den) }
}

/// WSL's virtual machine (vmmemWSL, or vmmem on older Windows).
pub fn is_wsl(name: &str) -> bool { name.to_lowercase().starts_with("vmmem") }

/// Seconds since `last`; never-happened (0) counts as long ago.
fn since(last: f64, t: f64) -> f64 { if last == 0.0 { f64::INFINITY } else { t - last } }

/// Never frozen: the never-touch list, snifrig, and the VM and system hosts.
pub fn protected(name: &str) -> bool {
    let n = name.to_lowercase();
    n.is_empty() || n.starts_with("snifrig") || crate::policy::DENY_NAMES.contains(&n.as_str())
}

impl Guard {
    pub fn load(dir: &Path) -> Guard {
        let mut g = Guard::default();
        g.entries = parse(&std::fs::read_to_string(dir.join(FILE)).unwrap_or_default());
        g
    }

    pub fn reload(&mut self, dir: &Path) { self.entries = parse(&std::fs::read_to_string(dir.join(FILE)).unwrap_or_default()); }

    pub fn save(&self, dir: &Path) { crate::rules::write_atomic(&dir.join(FILE), &render(&self.entries)); }

    pub fn frozen(&self) -> impl Iterator<Item = &Entry> { self.entries.iter().filter(|e| e.state == State::Frozen) }

    pub fn mark_frozen(&mut self, e: Entry) {
        self.entries.retain(|x| !(x.pid == e.pid && x.created == e.created));
        self.entries.push(e);
    }

    /// One tick. `procs` is every process; `fg` the pid of the window in front.
    pub fn tick(&mut self, t: f64, commit_pct: f64, procs: &[Seen], fg: Option<u32>) -> Vec<Step> {
        let mut out = Vec::new();
        self.commit.push_back((t, commit_pct));
        while self.commit.front().map_or(false, |x| t - x.0 > WINDOW_SECS) { self.commit.pop_front(); }

        let alive: HashSet<(u32, u64)> = procs.iter().map(|p| (p.pid, p.created)).collect();
        self.hist.retain(|k, _| alive.contains(k));
        for p in procs {
            let k = (p.pid, p.created);
            if p.private_mb < TRACK_MB && !self.hist.contains_key(&k) { continue; }
            let h = self.hist.entry(k).or_default();
            h.push_back((t, p.private_mb));
            while h.front().map_or(false, |x| t - x.0 > WINDOW_SECS) { h.pop_front(); }
        }
        // Exited apps leave the list (frozen ones are reported in `gone`); resumed ones leave it after the grace period.
        self.gone = self.entries.iter().filter(|e| e.state == State::Frozen && !alive.contains(&(e.pid, e.created))).cloned().collect();
        self.entries.retain(|e| alive.contains(&(e.pid, e.created)) && !(e.state == State::Resumed && t - e.unix > RESUMED_GRACE_SECS));

        // Commit must have risen over a long window too: a model load is a step, not a climb.
        let long = self.commit.front().map_or(false, |x| t - x.0 >= GROWER_SPAN_SECS);
        let per_min = slope(&self.commit).filter(|_| long).map(|s| s * 60.0);
        let rising = per_min.map_or(false, |s| s > RISING_PCT_MIN);
        let eta_min = per_min.filter(|s| *s > RISING_PCT_MIN).map(|s| ((100.0 - commit_pct) / s).max(0.0));

        let skip: HashSet<(u32, u64)> = self.entries.iter().map(|e| (e.pid, e.created)).collect();
        let mut growers: Vec<(&Seen, f64)> = procs.iter()
            .filter(|p| Some(p.pid) != fg && !protected(&p.name) && !skip.contains(&(p.pid, p.created)))
            .filter(|p| t - created_unix(p.created) >= MIN_AGE_SECS)
            .filter_map(|p| Some((p, sustained(self.hist.get(&(p.pid, p.created))?)?)))
            .filter(|(_, g)| *g >= MIN_GROWTH_MB_H)
            .collect();
        growers.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let urgent = (commit_pct >= FREEZE_PCT && rising) || (commit_pct >= SOON_PCT && eta_min.map_or(false, |m| m <= SOON_MIN));
        if urgent && since(self.last_freeze, t) >= FREEZE_EVERY_SECS {
            if let Some((p, g)) = growers.first() {
                let again = self.frozen_names.get(&p.name.to_lowercase()).map_or(false, |x| t - x < SAME_APP_SECS);
                if again {
                    // A copy of this app was frozen recently and it is back: freezing it again and
                    // again is how 16-19 Nov went. Say so once instead.
                    if since(self.last_warn, t) >= WARN_EVERY_SECS {
                        self.last_warn = t;
                        out.push(Step::Warn { name: p.name.clone(), pid: p.pid, mb: p.private_mb, growth_mb_h: *g, commit_pct, eta_min });
                    }
                    return out;
                }
                if self.frozen().count() < MAX_FROZEN {
                    self.frozen_names.insert(p.name.to_lowercase(), t);
                    self.last_freeze = t;
                    out.push(Step::Freeze { name: p.name.clone(), pid: p.pid, created: p.created, mb: p.private_mb, growth_mb_h: *g, commit_pct, eta_min });
                    return out;
                }
            } else if since(self.last_warn, t) >= WARN_EVERY_SECS / 3.0 {
                self.last_warn = t;
                let top = procs.iter().filter(|p| !skip.contains(&(p.pid, p.created)))
                    .filter_map(|p| Some((p.name.clone(), slope(self.hist.get(&(p.pid, p.created))?)? * 3600.0)))
                    .filter(|(_, g)| *g >= MIN_GROWTH_MB_H)
                    .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
                out.push(Step::NoCulprit { commit_pct, eta_min, top });
                return out;
            }
        }
        if commit_pct >= WARN_PCT && since(self.last_warn, t) >= WARN_EVERY_SECS {
            if let Some((p, g)) = growers.iter().find(|(_, g)| *g >= FAST_GROWTH_MB_H) {
                self.last_warn = t;
                out.push(Step::Warn { name: p.name.clone(), pid: p.pid, mb: p.private_mb, growth_mb_h: *g, commit_pct, eta_min });
            }
        }
        out
    }
}

/// "frozen|resumed pid created unix mb name" per line.
pub fn parse(s: &str) -> Vec<Entry> {
    s.lines().filter_map(|l| {
        let mut it = l.splitn(6, ' ');
        let state = match it.next()? { "frozen" => State::Frozen, "resumed" => State::Resumed, _ => return None };
        Some(Entry { state, pid: it.next()?.parse().ok()?, created: it.next()?.parse().ok()?, unix: it.next()?.parse().ok()?, mb: it.next()?.parse().ok()?, name: it.next()?.trim().to_string() })
    }).collect()
}

pub fn render(e: &[Entry]) -> String {
    e.iter().map(|e| format!("{} {} {} {:.0} {:.0} {}\n", if e.state == State::Frozen { "frozen" } else { "resumed" }, e.pid, e.created, e.unix, e.mb, e.name)).collect()
}

/// Plain words for a step, for the card and the activity log.
pub fn say(s: &Step) -> String {
    let eta = |m: &Option<f64>| match m { Some(m) if *m < 120.0 => format!(" At this rate memory runs out in about {:.0} minutes.", m.max(1.0)), _ => String::new() };
    match s {
        Step::Warn { name, mb, growth_mb_h, commit_pct, eta_min, .. } => format!(
            "{} is growing {:.1} GB an hour (now {:.1} GB) and memory is {:.0}% committed.{} If it gets close, snifrig freezes it so the PC does not crash; nothing is closed.",
            name, growth_mb_h / 1024.0, mb / 1024.0, commit_pct, eta(eta_min)),
        Step::Freeze { name, mb, growth_mb_h, commit_pct, eta_min, .. } => format!(
            "snifrig froze {} to keep your PC from running out of memory. It was growing {:.1} GB an hour and holds {:.1} GB; memory was {:.0}% committed.{} Nothing was closed: Resume puts it back exactly where it was (it will keep growing), End closes it and frees the memory.",
            name, growth_mb_h / 1024.0, mb / 1024.0, commit_pct, eta(eta_min)),
        Step::NoCulprit { commit_pct, eta_min, top: Some((n, g)) } if is_wsl(n) => format!(
            "WSL is growing {:.1} GB an hour and memory is {:.0}% committed.{} snifrig cannot freeze a virtual machine. Shutting down WSL gives the memory back, but stops everything running in it.",
            g / 1024.0, commit_pct, eta(eta_min)),
        Step::NoCulprit { commit_pct, eta_min, top: Some((n, g)) } => format!(
            "{} is growing {:.1} GB an hour and memory is {:.0}% committed.{} snifrig does not freeze it (it is the window you are using, or part of Windows). Save your work and restart it.",
            n, g / 1024.0, commit_pct, eta(eta_min)),
        Step::NoCulprit { commit_pct, eta_min, top: None } => format!(
            "Memory is {:.0}% committed and climbing, but no single app is growing. That is usually Windows itself holding memory (a kernel leak, often from programs being started in a loop).{} Close what you can; a restart clears it.",
            commit_pct, eta(eta_min)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(pid: u32, name: &str, mb: f64) -> Seen { Seen { pid, created: pid as u64 * 10, name: name.into(), private_mb: mb } }

    /// The 16 Nov crash: python.exe at 20 GB growing 12 GB/h, commit rising about 1% a minute.
    fn crash_run(fg: Option<u32>, name: &str) -> Vec<Step> {
        let mut g = Guard::default();
        let mut steps = Vec::new();
        for i in 0..900 {
            let t = i as f64 * 2.0;
            let commit = 80.0 + t / 60.0;
            if commit >= 100.0 { break; }
            let procs = vec![seen(29348, name, 20480.0 + 12288.0 * t / 3600.0), seen(4000, "brave.exe", 3000.0), seen(5000, "explorer.exe", 400.0)];
            for s in g.tick(t, commit, &procs, fg) {
                if let Step::Freeze { pid, created, mb, name, .. } = &s {
                    g.mark_frozen(Entry { state: State::Frozen, pid: *pid, created: *created, unix: t, mb: *mb, name: name.clone() });
                }
                steps.push(s);
            }
        }
        steps
    }

    #[test]
    fn freezes_the_leaker_before_memory_runs_out() {
        let steps = crash_run(None, "python.exe");
        assert!(matches!(steps.iter().find(|s| matches!(s, Step::Warn { .. })), Some(Step::Warn { pid: 29348, .. })));
        let f = steps.iter().find_map(|s| match s { Step::Freeze { pid, commit_pct, .. } => Some((*pid, *commit_pct)), _ => None });
        let (pid, pct) = f.expect("froze something");
        assert_eq!(pid, 29348);
        assert!(pct < 95.0, "froze at {}", pct);
        assert_eq!(steps.iter().filter(|s| matches!(s, Step::Freeze { .. })).count(), 1);
    }

    #[test]
    fn never_freezes_the_window_in_front() {
        assert!(!crash_run(Some(29348), "python.exe").iter().any(|s| matches!(s, Step::Freeze { pid: 29348, .. })));
    }

    #[test]
    fn never_freezes_protected_names() {
        assert!(!crash_run(None, "explorer.exe").iter().any(|s| matches!(s, Step::Freeze { .. })));
        assert!(!crash_run(None, "snifrig-fix.exe").iter().any(|s| matches!(s, Step::Freeze { .. })));
    }

    #[test]
    fn no_culprit_when_nothing_grows() {
        let mut g = Guard::default();
        let mut steps = Vec::new();
        for i in 0..120 {
            let t = i as f64 * 2.0;
            steps.extend(g.tick(t, 94.0 + t / 120.0, &[seen(1, "brave.exe", 3000.0)], None));
        }
        assert!(steps.iter().any(|s| matches!(s, Step::NoCulprit { .. })));
        assert!(!steps.iter().any(|s| matches!(s, Step::Freeze { .. })));
    }

    #[test]
    fn names_wsl_when_it_is_the_grower() {
        let mut g = Guard::default();
        let mut top = None;
        for i in 0..120 {
            let t = i as f64 * 2.0;
            for s in g.tick(t, 94.0 + t / 120.0, &[seen(9, "vmmemWSL", 20000.0 + 20000.0 * t / 3600.0)], None) {
                if let Step::NoCulprit { top: tp, .. } = s { top = tp; }
            }
        }
        let (n, _) = top.expect("named a grower");
        assert!(is_wsl(&n));
    }

    /// 16-19 Nov: a LoRA trainer loading its model went from 0 to 12 GB in a minute, then stayed.
    #[test]
    fn model_load_is_not_a_leak() {
        let mut g = Guard::default();
        let created = ((0.0 + 11644473600.0) * 1e7) as u64; // started at t = 0
        let mut steps = Vec::new();
        for i in 0..600 {
            let t = i as f64 * 2.0;
            let mb = (t / 60.0).min(1.0) * 12288.0;
            let commit = 70.0 + (t / 60.0).min(1.0) * 19.0; // 70% -> 89%, then flat
            let p = Seen { pid: 65104, created, name: "python.exe".into(), private_mb: mb };
            steps.extend(g.tick(t, commit, &[p], None));
        }
        assert!(!steps.iter().any(|s| matches!(s, Step::Freeze { .. })), "{:?}", steps);
    }

    #[test]
    fn a_step_in_an_old_process_is_not_a_leak() {
        let mut g = Guard::default();
        let mut steps = Vec::new();
        for i in 0..300 {
            let t = i as f64 * 2.0;
            let mb = if t < 200.0 { 2000.0 } else { 14000.0 };
            let commit = if t < 200.0 { 80.0 } else { 92.0 };
            steps.extend(g.tick(t, commit, &[seen(7, "python.exe", mb)], None));
        }
        assert!(!steps.iter().any(|s| matches!(s, Step::Freeze { .. })), "{:?}", steps);
    }

    #[test]
    fn the_same_app_is_frozen_once_then_only_warned_about() {
        let mut g = Guard::default();
        let mut freezes = 0;
        for i in 0..1200 {
            let t = i as f64 * 2.0;
            let commit = (85.0 + (t % 600.0) / 60.0).min(99.0);
            // a new python.exe every 10 minutes, each leaking
            let pid = 1000 + (t / 600.0) as u32;
            let p = seen(pid, "python.exe", 4000.0 + 20000.0 * (t % 600.0) / 3600.0);
            for s in g.tick(t, commit, &[p], None) {
                if let Step::Freeze { pid, created, mb, name, .. } = &s {
                    freezes += 1;
                    g.mark_frozen(Entry { state: State::Frozen, pid: *pid, created: *created, unix: t, mb: *mb, name: name.clone() });
                }
            }
        }
        assert_eq!(freezes, 1);
    }

    #[test]
    fn steady_high_commit_freezes_nothing() {
        let mut g = Guard::default();
        let mut steps = Vec::new();
        for i in 0..200 {
            let t = i as f64 * 2.0;
            steps.extend(g.tick(t, 96.0, &[seen(1, "python.exe", 20000.0)], None));
        }
        assert!(steps.is_empty(), "{:?}", steps);
    }

    #[test]
    fn resumed_app_is_left_alone() {
        let mut g = Guard::default();
        g.mark_frozen(Entry { state: State::Resumed, pid: 29348, created: 293480, unix: 0.0, mb: 0.0, name: "python.exe".into() });
        let mut froze = false;
        for i in 0..600 {
            let t = i as f64 * 2.0;
            let c = 85.0 + t / 60.0;
            if c >= 100.0 { break; }
            for s in g.tick(t, c, &[seen(29348, "python.exe", 20000.0 + 12288.0 * t / 3600.0)], None) { froze |= matches!(s, Step::Freeze { .. }); }
        }
        assert!(!froze);
    }

    #[test]
    fn file_round_trip() {
        let e = vec![Entry { state: State::Frozen, pid: 7, created: 70, unix: 100.0, mb: 2048.0, name: "my app.exe".into() }];
        assert_eq!(parse(&render(&e)), e);
    }
}
