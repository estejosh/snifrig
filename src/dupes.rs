//! Duplicate-instance detector and full-VRAM alert. Runs every 5 minutes, not every cycle.
//!
//! A duplicate is a group of big processes (over 500 MB private) with the same image name and the
//! same full command line, all older than 2 minutes. The "idle twin" is the member that owns no
//! listening TCP port while another member does (or, if none listen, the one that burned no CPU
//! since the previous check). Command lines are read only for big processes, then reduced to
//! program and script names in the message (arguments can hold secrets).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::{cmdline, json_num, script_names, Proc};

const EVERY_SECS: f64 = 300.0;
const REALERT_SECS: f64 = 2.0 * 3600.0;
const MIN_PRIVATE: u64 = 500 << 20;
const MIN_AGE_SECS: f64 = 120.0;
/// 100 ns units of CPU time. Under this since the last check counts as "no CPU".
const IDLE_CPU: u64 = 5_000_000;
/// With a listening twin present, an idle candidate may still use up to this much CPU.
const IDLE_CPU_LISTENER: u64 = 20_000_000;
const EPOCH_DIFF_100NS: u64 = 116_444_736_000_000_000;

#[derive(Clone, Debug)]
pub struct Cand {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
    pub private: u64,
    pub created: u64, // FILETIME
    pub cpu: u64,
    pub cmd: String,
}

#[derive(Debug, PartialEq)]
pub struct Dup {
    pub name: String,
    pub idle: u32,
    pub other: u32,
    pub idle_listens: bool,
    pub other_listens: bool,
    /// CPU time the idle twin used since the previous check, if it was known.
    pub idle_delta: Option<u64>,
    /// True only when the evidence separates the twins: one listens and the other does not, or
    /// one used CPU since the last check while the other used none. Otherwise it is a guess, and
    /// the alert says "cannot tell which" and is report-only.
    pub sure: bool,
}

/// Groups candidates by (lowercase name, identical command line); keeps groups of two or more.
pub fn group(c: &[Cand]) -> Vec<Vec<&Cand>> {
    let mut m: HashMap<(String, &str), Vec<&Cand>> = HashMap::new();
    for x in c {
        if x.cmd.is_empty() { continue; }
        m.entry((x.name.to_lowercase(), x.cmd.as_str())).or_default().push(x);
    }
    let mut v: Vec<Vec<&Cand>> = m.into_values().filter(|g| g.len() >= 2).collect();
    for g in v.iter_mut() { g.sort_by_key(|x| x.pid); }
    v.sort_by_key(|g| g[0].pid);
    v
}

/// Picks the idle twin of one group, or None if no member is clearly idle.
pub fn idle_twin(g: &[&Cand], listening: &HashSet<u32>, prev: &HashMap<u32, u64>) -> Option<Dup> {
    let delta = |x: &Cand| prev.get(&x.pid).map(|&p| x.cpu.saturating_sub(p));
    let listeners: Vec<&&Cand> = g.iter().filter(|x| listening.contains(&x.pid)).collect();
    let quiet: Vec<&&Cand> = g.iter().filter(|x| !listening.contains(&x.pid)).collect();
    if !listeners.is_empty() && !quiet.is_empty() {
        // Worker model (one is the parent of the other): not a leftover duplicate.
        let related = |a: &Cand, b: &Cand| a.ppid == b.pid || b.ppid == a.pid;
        let mut best: Option<&&Cand> = None;
        for q in &quiet {
            if listeners.iter().any(|l| related(q, l)) { continue; }
            if delta(q).map_or(false, |d| d > IDLE_CPU_LISTENER) { continue; }
            // Several candidates: the newest one is the most likely leftover.
            if best.map_or(true, |b| q.created > b.created) { best = Some(q); }
        }
        let q = best?;
        return Some(Dup { name: q.name.clone(), idle: q.pid, other: listeners[0].pid, idle_listens: false, other_listens: true, idle_delta: delta(q), sure: true });
    }
    // Nobody, or everybody, listens (Windows lets two copies bind the same port with
    // SO_REUSEADDR, so both can show up as owners): the lowest CPU delta wins, ties go to the
    // copy with less CPU in its lifetime, and only if the delta is about zero.
    let mut best: Option<(&&Cand, u64)> = None;
    for x in g {
        if let Some(d) = delta(x) {
            if best.map_or(true, |(b, bd)| (d, x.cpu) < (bd, b.cpu)) { best = Some((x, d)); }
        }
    }
    let (q, d) = best?;
    if d > IDLE_CPU { return None; }
    // Parent and child with one command line is a launcher shim, not a duplicate.
    let other = g.iter().find(|x| x.pid != q.pid && x.ppid != q.pid && q.ppid != x.pid)?;
    // Sure only if the other copy demonstrably did work while this one did not.
    let sure = delta(other).map_or(false, |od| od > IDLE_CPU);
    Some(Dup { name: q.name.clone(), idle: q.pid, other: other.pid, idle_listens: listening.contains(&q.pid), other_listens: listening.contains(&other.pid), idle_delta: Some(d), sure })
}

/// Pure: all duplicates among the candidates.
pub fn find(c: &[Cand], listening: &HashSet<u32>, prev: &HashMap<u32, u64>) -> Vec<Dup> {
    group(c).iter().filter_map(|g| idle_twin(g, listening, prev)).collect()
}

/// PIDs that own a listening TCP socket, IPv4 and IPv6.
pub fn listening_pids() -> HashSet<u32> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{GetExtendedTcpTable, TCP_TABLE_OWNER_PID_LISTENER};
    let mut out = HashSet::new();
    for (af, row, pid_off) in [(2u32, 24usize, 20usize), (23u32, 56usize, 52usize)] {
        let mut size = 0u32;
        let mut buf: Vec<u8> = Vec::new();
        for _ in 0..4 {
            let r = unsafe { GetExtendedTcpTable(buf.as_mut_ptr() as *mut _, &mut size, 0, af, TCP_TABLE_OWNER_PID_LISTENER, 0) };
            if r == 0 {
                if buf.len() >= 4 {
                    let n = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
                    for i in 0..n {
                        let o = 4 + i * row + pid_off;
                        if o + 4 > buf.len() { break; }
                        out.insert(u32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]));
                    }
                }
                break;
            }
            if r != 122 { break; } // ERROR_INSUFFICIENT_BUFFER
            buf = vec![0u8; size as usize + 1024];
            size = buf.len() as u32;
        }
    }
    out
}

/// HH:MM local time of a FILETIME.
fn hhmm(ft: u64) -> String {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    unsafe {
        let f = FILETIME { dwLowDateTime: ft as u32, dwHighDateTime: (ft >> 32) as u32 };
        let (mut u, mut l): (SYSTEMTIME, SYSTEMTIME) = (core::mem::zeroed(), core::mem::zeroed());
        if FileTimeToSystemTime(&f, &mut u) == 0 || SystemTimeToTzSpecificLocalTime(core::ptr::null(), &u, &mut l) == 0 {
            return "?".into();
        }
        format!("{:02}:{:02}", l.wHour, l.wMinute)
    }
}

/// Builds the plain-language alert for one finding.
pub fn message(d: &Dup, c: &[Cand], parent: &str, started: &str) -> String {
    let idle = c.iter().find(|x| x.pid == d.idle);
    let gb = idle.map(|x| x.private as f64 / 1073741824.0).unwrap_or(0.0);
    let quiet = d.idle_delta.map_or(false, |x| x <= IDLE_CPU);
    let state = match (d.idle_listens, quiet) {
        (false, true) => "no listening port, no CPU",
        (false, false) => "no listening port",
        (true, _) => "no CPU, and it only shares the listening port",
    };
    let shown = idle.map(|x| script_names(&x.cmd)).unwrap_or_default();
    format!(
        "{} is running twice with the same command line (pids {} and {}, started {}). pid {} is idle ({}) and holds {:.1} GB RAM. Command: {}. Launched by: {}. It is probably a leftover duplicate; ending it frees the memory.",
        d.name, d.other, d.idle, started, d.idle, state, gb, shown, parent)
}

/// Both copies look idle (or both listen): say so plainly instead of guessing which one to end.
pub fn unsure_message(d: &Dup, c: &[Cand], parent: &str, started: &str) -> String {
    let gb = |pid: u32| c.iter().find(|x| x.pid == pid).map(|x| x.private as f64 / 1073741824.0).unwrap_or(0.0);
    let shown = c.iter().find(|x| x.pid == d.idle).map(|x| script_names(&x.cmd)).unwrap_or_default();
    format!(
        "{} is running twice with the same command line (pids {} and {}, started {}), holding {:.1} GB and {:.1} GB RAM. Neither did any work since the last check, so snifrig cannot tell yet which one is serving. Command: {}. Launched by: {}. One of them is probably a leftover duplicate.",
        d.name, d.other.min(d.idle), d.other.max(d.idle), started, gb(d.other.min(d.idle)), gb(d.other.max(d.idle)), shown, parent)
}

pub struct Dupes {
    last: f64,
    prev_cpu: HashMap<u32, u64>,
    cool: HashMap<String, f64>,
}

impl Dupes {
    pub fn new() -> Self { Dupes { last: 0.0, prev_cpu: HashMap::new(), cool: HashMap::new() } }

    /// Call every cycle; does real work every 5 minutes. Returns (key, message) alerts.
    pub fn check(&mut self, t: f64, dir: &Path, procs: &[Proc]) -> Vec<(String, String)> {
        if t - self.last < EVERY_SECS { return Vec::new(); }
        self.last = t;
        let mut out = self.dupes(t, procs);
        out.extend(self.vram(t, dir));
        out
    }

    fn dupes(&mut self, t: f64, procs: &[Proc]) -> Vec<(String, String)> {
        let ft_now = (t * 1e7) as u64 + EPOCH_DIFF_100NS;
        let cands: Vec<Cand> = procs.iter()
            .filter(|p| p.pid > 4 && p.private > MIN_PRIVATE && ft_now.saturating_sub(p.created) as f64 / 1e7 > MIN_AGE_SECS)
            .filter_map(|p| {
                let cmd = cmdline(p.pid)?;
                Some(Cand { pid: p.pid, ppid: p.ppid, name: p.name.clone(), private: p.private, created: p.created, cpu: p.cpu, cmd })
            }).collect();
        let found = if group(&cands).is_empty() { Vec::new() } else { find(&cands, &listening_pids(), &self.prev_cpu) };
        self.prev_cpu = procs.iter().map(|p| (p.pid, p.cpu)).collect();
        let mut out = Vec::new();
        for d in found {
            let key = if d.sure { format!("dup:{}#{}", d.name, d.idle) } else { format!("dupq:{}#{}+{}", d.name, d.other.min(d.idle), d.other.max(d.idle)) };
            if t - self.cool.get(&key).copied().unwrap_or(0.0) < REALERT_SECS { continue; }
            self.cool.insert(key.clone(), t);
            let started = cands.iter().find(|x| x.pid == d.idle).map(|x| hhmm(x.created)).unwrap_or_default();
            let ppid = cands.iter().find(|x| x.pid == d.idle).map(|x| x.ppid).unwrap_or(0);
            let parent = match procs.iter().find(|p| p.pid == ppid) {
                Some(p) => { let s = cmdline(p.pid).map(|c| script_names(&c)).unwrap_or_default(); if s.is_empty() { p.name.clone() } else { s } }
                None => "a process that has already exited".to_string(),
            };
            let msg = if d.sure { message(&d, &cands, &parent, &started) } else { unsure_message(&d, &cands, &parent, &started) };
            out.push((key, msg));
        }
        out
    }

    fn vram(&mut self, t: f64, dir: &Path) -> Vec<(String, String)> {
        let s = match std::fs::read_to_string(dir.join("gpu.json")) { Ok(s) => s, Err(_) => return Vec::new() };
        let age = t - json_num(&s, "unix") as f64;
        if age > 900.0 { return Vec::new(); }
        let Some(msg) = vram_message(json_num(&s, "used_mb"), json_num(&s, "total_mb")) else { return Vec::new() };
        if t - self.cool.get("x:vram").copied().unwrap_or(0.0) < REALERT_SECS { return Vec::new(); }
        self.cool.insert("x:vram".into(), t);
        vec![("x:vram".into(), msg)]
    }
}

/// Pure: the alert text when graphics memory is at least 95% full.
pub fn vram_message(used_mb: u64, total_mb: u64) -> Option<String> {
    if total_mb == 0 || used_mb * 100 < total_mb * 95 { return None; }
    Some(format!("Graphics memory is full ({:.1} of {:.0} GB). Apps spill into system RAM, which slows them down. Close or restart the biggest graphics users.",
        used_mb as f64 / 1024.0, total_mb as f64 / 1024.0))
}

/// Debug entry point for a live dry run: two samples, prints findings, never acts.
#[cfg(test)]
pub fn dry_run() -> Vec<String> {
    use crate::read_procs;
    let mut buf = vec![0u8; 1 << 20];
    let mut d = Dupes::new();
    let a = read_procs(&mut buf).unwrap_or_default();
    let t = crate::now();
    d.prev_cpu = a.iter().map(|p| (p.pid, p.cpu)).collect();
    std::thread::sleep(std::time::Duration::from_secs(3));
    let b = read_procs(&mut buf).unwrap_or_default();
    let mut out: Vec<String> = d.dupes(t + 3.0, &b).into_iter().map(|(k, m)| format!("{} | {}", k, m)).collect();
    let big = b.iter().filter(|p| p.private > MIN_PRIVATE).count();
    out.push(format!("(scanned {} processes over 500 MB; {} listening pids)", big, listening_pids().len()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(pid: u32, ppid: u32, name: &str, cmd: &str, cpu: u64, created: u64) -> Cand {
        Cand { pid, ppid, name: name.into(), private: 10 << 30, created, cpu, cmd: cmd.into() }
    }

    #[test]
    fn both_listening_and_idle_is_not_sure() {
        // The real llama-server case: same command, both bound to the port, neither did work.
        let g = vec![c(43988, 34040, "llama-server.exe", "x -m m --port 18090", 1_000, 1), c(38736, 34040, "llama-server.exe", "x -m m --port 18090", 900, 1)];
        let listening: HashSet<u32> = [43988, 38736].into_iter().collect();
        let prev: HashMap<u32, u64> = [(43988, 1_000), (38736, 900)].into_iter().collect();
        let d = find(&g, &listening, &prev).pop().expect("pair found");
        assert!(!d.sure, "a tie must not name an idle twin as certain");
    }

    #[test]
    fn serving_copy_makes_it_sure() {
        let g = vec![c(43988, 34040, "llama-server.exe", "x -m m --port 18090", 1_000, 1), c(38736, 34040, "llama-server.exe", "x -m m --port 18090", 900 + 7_000_000, 1)];
        let listening: HashSet<u32> = [43988, 38736].into_iter().collect();
        let prev: HashMap<u32, u64> = [(43988, 1_000), (38736, 900)].into_iter().collect();
        let d = find(&g, &listening, &prev).pop().expect("pair found");
        assert!(d.sure);
        assert_eq!(d.idle, 43988);
    }
    const CL: &str = r"X:\llama.cpp-bin\llama-server.exe -m m.gguf --port 18090";

    #[test]
    fn groups_identical_cmdlines_only() {
        let v = vec![c(1, 9, "a.exe", CL, 0, 0), c(2, 9, "a.exe", CL, 0, 0), c(3, 9, "a.exe", "other", 0, 0), c(4, 9, "b.exe", CL, 0, 0)];
        let g = group(&v);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].iter().map(|x| x.pid).collect::<Vec<_>>(), vec![1, 2]);
    }

    #[test]
    fn singletons_and_empty_cmdlines_ignored() {
        let v = vec![c(1, 9, "a.exe", "x", 0, 0), c(2, 9, "a.exe", "", 0, 0), c(3, 9, "a.exe", "", 0, 0)];
        assert!(group(&v).is_empty());
    }

    #[test]
    fn non_listening_twin_is_idle() {
        let v = vec![c(100, 9, "llama-server.exe", CL, 0, 1), c(200, 9, "llama-server.exe", CL, 0, 1)];
        let l: HashSet<u32> = [100].into_iter().collect();
        let d = find(&v, &l, &HashMap::new());
        assert_eq!(d.len(), 1);
        assert_eq!((d[0].idle, d[0].other), (200, 100));
    }

    #[test]
    fn parent_child_pair_is_not_a_duplicate() {
        let v = vec![c(100, 9, "w.exe", CL, 0, 1), c(200, 100, "w.exe", CL, 0, 1)];
        let l: HashSet<u32> = [100].into_iter().collect();
        assert!(find(&v, &l, &HashMap::new()).is_empty());
    }

    #[test]
    fn busy_non_listener_is_left_alone() {
        let v = vec![c(100, 9, "w.exe", CL, 0, 1), c(200, 9, "w.exe", CL, 900_000_000, 1)];
        let l: HashSet<u32> = [100].into_iter().collect();
        let prev: HashMap<u32, u64> = [(100, 0), (200, 0)].into_iter().collect();
        assert!(find(&v, &l, &prev).is_empty());
    }

    #[test]
    fn no_listeners_picks_lowest_delta_when_about_zero() {
        let v = vec![c(100, 9, "w.exe", CL, 5_000_000_000, 1), c(200, 9, "w.exe", CL, 1_000_100, 1)];
        let prev: HashMap<u32, u64> = [(100, 1_000_000_000), (200, 1_000_000)].into_iter().collect();
        let d = find(&v, &HashSet::new(), &prev);
        assert_eq!(d.len(), 1);
        assert_eq!((d[0].idle, d[0].other), (200, 100));
    }

    #[test]
    fn no_listeners_needs_previous_sample_and_near_zero_delta() {
        let v = vec![c(100, 9, "w.exe", CL, 50, 1), c(200, 9, "w.exe", CL, 60, 1)];
        assert!(find(&v, &HashSet::new(), &HashMap::new()).is_empty());
        let prev: HashMap<u32, u64> = [(100, 0), (200, 0)].into_iter().collect();
        let busy = vec![c(100, 9, "w.exe", CL, 900_000_000, 1), c(200, 9, "w.exe", CL, 800_000_000, 1)];
        assert!(find(&busy, &HashSet::new(), &prev).is_empty());
    }

    #[test]
    fn both_listening_without_cpu_history_is_left_alone() {
        let v = vec![c(100, 9, "w.exe", CL, 0, 1), c(200, 9, "w.exe", CL, 0, 1)];
        let l: HashSet<u32> = [100, 200].into_iter().collect();
        assert!(find(&v, &l, &HashMap::new()).is_empty());
    }

    #[test]
    fn both_bound_to_same_port_idle_one_is_chosen_by_cpu() {
        // Windows allows two copies to bind one port; the one with no CPU is the leftover.
        let v = vec![c(100, 9, "w.exe", CL, 9_000_000_000, 1), c(200, 9, "w.exe", CL, 700_000_000, 1)];
        let l: HashSet<u32> = [100, 200].into_iter().collect();
        let prev: HashMap<u32, u64> = [(100, 8_000_000_000), (200, 700_000_000)].into_iter().collect();
        let d = find(&v, &l, &prev);
        assert_eq!(d.len(), 1);
        assert_eq!((d[0].idle, d[0].other, d[0].idle_listens), (200, 100, true));
    }

    #[test]
    fn launcher_shim_parent_child_is_not_a_duplicate() {
        let v = vec![c(100, 9, "python.exe", CL, 0, 1), c(200, 100, "python.exe", CL, 0, 1)];
        let prev: HashMap<u32, u64> = [(100, 0), (200, 0)].into_iter().collect();
        assert!(find(&v, &HashSet::new(), &prev).is_empty());
    }

    #[test]
    fn message_names_pids_memory_and_parent() {
        let v = vec![c(100, 9, "llama-server.exe", CL, 0, 1), c(200, 9, "llama-server.exe", CL, 0, 1)];
        let d = idle_twin(&group(&v)[0], &[100].into_iter().collect(), &HashMap::new()).unwrap();
        let m = message(&d, &v, "pythonw.exe gpu_arbiter.py", "14:05");
        assert!(m.contains("llama-server.exe is running twice") && m.contains("pids 100 and 200, started 14:05"), "{}", m);
        assert!(m.contains("pid 200 is idle (no listening port)") && m.contains("10.0 GB") && m.contains("pythonw.exe gpu_arbiter.py"), "{}", m);
    }

    #[test]
    fn listener_pids_include_something_on_a_real_machine() {
        let l = listening_pids();
        // PID 4 (System) or any service listens on essentially every Windows box.
        assert!(!l.is_empty());
    }

    #[test]
    fn vram_threshold_is_95_percent() {
        assert!(vram_message(24 * 1024 * 95 / 100 - 1, 24 * 1024).is_none());
        let m = vram_message(24473, 24576).unwrap();
        assert!(m.contains("23.9 of 24 GB"), "{}", m);
        assert!(vram_message(5, 0).is_none());
    }

    #[test]
    #[ignore]
    fn live_dry_run() {
        for l in dry_run() { println!("FINDING {}", l); }
    }
}
