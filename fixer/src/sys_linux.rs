//! Linux system calls the governor needs, same contract as sys_windows.rs: nothing panics,
//! failures return Err/None, and nothing needs root for your own processes.
//! Units match Windows so the shared code is unchanged: `created` and `cpu` are 100 ns ticks
//! (`created` counted from 1601 like a Windows FILETIME), priority classes are the Windows values
//! mapped to nice levels (IDLE 19, BELOW_NORMAL 10, NORMAL 0).
//! Linux applies nice, I/O priority and affinity per thread, so each is set on every thread.
//! Not here yet (later steps): which window is in front, full screen, CPU caps (cgroups).

use std::collections::HashSet;
use std::fs;

#[derive(Clone, Debug, Default)]
pub struct Proc { pub pid: u32, pub ppid: u32, pub name: String, pub created: u64, pub cpu: u64 }

pub const IDLE: u32 = 0x40;
pub const BELOW_NORMAL: u32 = 0x4000;
pub const NORMAL: u32 = 0x20;

const UNIX_TO_1601: f64 = 11_644_473_600.0;

fn clk_tck() -> f64 {
    let t = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if t > 0 { t as f64 } else { 100.0 }
}

/// When Linux started (unix seconds), from /proc/stat btime.
pub fn boot_unix() -> f64 {
    fs::read_to_string("/proc/stat").ok().and_then(|s| s.lines().find_map(|l| l.strip_prefix("btime ")?.trim().parse().ok())).unwrap_or(0.0)
}

/// The fields of /proc/<pid>/stat after the name: (ppid, start ticks since boot, utime+stime ticks).
fn stat(pid: u32) -> Option<(String, u32, u64, u64)> {
    let s = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (a, b) = (s.find('(')?, s.rfind(')')?);
    let comm = s[a + 1..b].to_string();
    let f: Vec<&str> = s[b + 2..].split_whitespace().collect();
    // f[0] state, f[1] ppid, f[11] utime, f[12] stime, f[19] starttime
    Some((comm, f.get(1)?.parse().ok()?, f.get(19)?.parse().ok()?, f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?))
}

/// The program's file name, as a person would recognise it (comm is cut at 15 characters).
fn name_of(pid: u32, comm: &str) -> String {
    if let Ok(p) = fs::read_link(format!("/proc/{pid}/exe")) {
        if let Some(n) = p.file_name().and_then(|n| n.to_str()) { return n.trim_end_matches(" (deleted)").to_string(); }
    }
    if comm.len() == 15 {
        if let Ok(c) = fs::read(format!("/proc/{pid}/cmdline")) {
            if let Some(a0) = c.split(|b| *b == 0).next().and_then(|a| std::str::from_utf8(a).ok()) {
                let n = a0.rsplit('/').next().unwrap_or(a0);
                if n.starts_with(comm) { return n.to_string(); }
            }
        }
    }
    comm.to_string()
}

fn created_of(start_ticks: u64, boot: f64, tck: f64) -> u64 { ((boot + start_ticks as f64 / tck + UNIX_TO_1601) * 1e7) as u64 }

/// Every process. The buffer is unused on Linux (kept so callers are the same everywhere).
pub fn snapshot(_buf: &mut Vec<u8>) -> Vec<Proc> {
    let (boot, tck) = (boot_unix(), clk_tck());
    let Ok(rd) = fs::read_dir("/proc") else { return Vec::new() };
    let mut out = Vec::with_capacity(512);
    for e in rd.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        let Some((comm, ppid, start, cpu)) = stat(pid) else { continue };
        out.push(Proc { pid, ppid, name: name_of(pid, &comm), created: created_of(start, boot, tck), cpu: (cpu as f64 / tck * 1e7) as u64 });
    }
    out
}

/// One process, without reading all of /proc.
pub fn snapshot_one(pid: u32) -> Option<Proc> {
    let (comm, ppid, start, cpu) = stat(pid)?;
    let tck = clk_tck();
    Some(Proc { pid, ppid, name: name_of(pid, &comm), created: created_of(start, boot_unix(), tck), cpu: (cpu as f64 / tck * 1e7) as u64 })
}

fn meminfo() -> std::collections::HashMap<String, f64> {
    fs::read_to_string("/proc/meminfo").unwrap_or_default().lines().filter_map(|l| {
        let (k, v) = l.split_once(':')?;
        Some((k.to_string(), v.split_whitespace().next()?.parse::<f64>().ok()? / 1024.0))
    }).collect()
}

/// (memory in use %, counting swap, like Windows commit; available MB).
pub fn memory_status() -> Option<(f64, f64)> {
    let m = meminfo();
    let (total, avail) = (*m.get("MemTotal")?, *m.get("MemAvailable")?);
    let (st, sf) = (m.get("SwapTotal").copied().unwrap_or(0.0), m.get("SwapFree").copied().unwrap_or(0.0));
    if total <= 0.0 { return None; }
    Some((((total - avail) + (st - sf)) / (total + st) * 100.0, avail))
}

/// Seconds to add to unix time to get local time (daylight saving included).
pub fn local_offset_secs() -> i64 {
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() { 0 } else { tm.tm_gmtoff as i64 }
    }
}

/// Seconds since your last input, from the login session's idle hint (GNOME, KDE and most
/// desktops set it). 0 when unknown, which reads as "you are here": the careful answer.
pub fn idle_secs() -> f64 {
    use std::sync::Mutex;
    static CACHE: Mutex<(f64, f64)> = Mutex::new((0.0, 0.0));
    let now = crate::now_unix();
    if let Ok(c) = CACHE.lock() { if now - c.0 < 10.0 { return c.1; } }
    let v = std::process::Command::new("loginctl").args(["show-session", "auto", "-p", "IdleHint", "-p", "IdleSinceHint"]).output().ok()
        .and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout).to_string();
            let idle = s.lines().any(|l| l.trim() == "IdleHint=yes");
            let since: f64 = s.lines().find_map(|l| l.strip_prefix("IdleSinceHint=")?.trim().parse().ok())?;
            Some(if idle && since > 0.0 { (now - since / 1e6).max(0.0) } else { 0.0 })
        }).unwrap_or(0.0);
    if let Ok(mut c) = CACHE.lock() { *c = (now, v); }
    v
}

/// Full-screen detection comes with the desktop support (X11, KDE, wlroots, GNOME): later step.
pub fn fullscreen_d3d() -> bool { false }
pub fn covers_monitor(_h: isize) -> bool { false }
pub fn minimize_quietly(_h: isize) -> bool { false }
/// Which windows are open and which is in front: later step (per desktop). Empty means
/// "unknown", and snifrig fix does not run its governor on Linux until this is in.
pub fn visible_windows() -> Vec<(isize, u32)> { Vec::new() }
pub fn window_pids() -> HashSet<u32> { HashSet::new() }
pub fn windows() -> (HashSet<u32>, HashSet<u32>) { (HashSet::new(), HashSet::new()) }
pub fn foreground_pid() -> Option<u32> { None }

/// Every thread of a process.
fn tasks(pid: u32) -> Vec<i32> {
    fs::read_dir(format!("/proc/{pid}/task")).map(|d| d.flatten().filter_map(|e| e.file_name().to_str()?.parse().ok()).collect()).unwrap_or_default()
}

fn errno() -> String { std::io::Error::last_os_error().to_string() }

/// Efficiency cores on a hybrid CPU (Intel 12th gen and later list them in sysfs). None when
/// every core is the same kind.
pub fn efficiency_cpu_sets() -> Option<&'static [u32]> {
    static SETS: std::sync::OnceLock<Option<Vec<u32>>> = std::sync::OnceLock::new();
    SETS.get_or_init(|| {
        let s = fs::read_to_string("/sys/devices/cpu_atom/cpus").ok()?;
        let v = parse_ranges(s.trim());
        (!v.is_empty()).then_some(v)
    }).as_deref()
}

fn parse_ranges(s: &str) -> Vec<u32> {
    let mut v = Vec::new();
    for part in s.split(',').filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => if let (Ok(a), Ok(b)) = (a.parse::<u32>(), b.parse::<u32>()) { v.extend(a..=b) },
            None => if let Ok(a) = part.parse() { v.push(a) },
        }
    }
    v
}

fn set_cpus(pid: u32, cpus: &[u32]) -> Result<(), String> {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        for &c in cpus { libc::CPU_SET(c as usize, &mut set); }
        let mut ok = false;
        for t in tasks(pid) { if libc::sched_setaffinity(t, std::mem::size_of::<libc::cpu_set_t>(), &set) == 0 { ok = true; } }
        if ok { Ok(()) } else { Err(format!("sched_setaffinity({pid}): {}", errno())) }
    }
}

fn online_cpus() -> Vec<u32> { parse_ranges(fs::read_to_string("/sys/devices/system/cpu/online").unwrap_or_default().trim()) }

/// Keep a process on these cores (empty = any core again).
pub fn set_cpu_sets(pid: u32, ids: &[u32]) -> Result<(), String> {
    if ids.is_empty() { set_cpus(pid, &online_cpus()) } else { set_cpus(pid, ids) }
}

fn nice_of(class: u32) -> i32 { match class { IDLE => 19, BELOW_NORMAL => 10, _ => 0 } }

pub fn get_priority(pid: u32) -> Option<u32> {
    unsafe { *libc::__errno_location() = 0; }
    let n = unsafe { libc::getpriority(libc::PRIO_PROCESS, pid) };
    if n == -1 && unsafe { *libc::__errno_location() } != 0 { return None; }
    Some(if n >= 15 { IDLE } else if n >= 5 { BELOW_NORMAL } else { NORMAL })
}

/// Lowering always works for your own processes. Raising back needs permission Linux gives
/// only to root (or a nice limit), so a failed restore says so; the cgroup step removes this.
pub fn set_priority(pid: u32, class: u32) -> Result<(), String> {
    if class != IDLE && class != BELOW_NORMAL && class != NORMAL { return Err(format!("refusing priority class {class:#x}")); }
    let n = nice_of(class);
    let mut ok = false;
    let mut last = String::new();
    for t in tasks(pid) {
        if unsafe { libc::setpriority(libc::PRIO_PROCESS, t as u32, n) } == 0 { ok = true; } else { last = errno(); }
    }
    if ok { Ok(()) } else { Err(format!("setpriority({pid}, {n}): {last}")) }
}

/// Linux has no per-process efficiency mode; on a hybrid CPU the efficiency cores do the same job.
pub fn set_efficiency(pid: u32, on: bool) -> Result<(), String> {
    match efficiency_cpu_sets() {
        Some(e) => if on { set_cpus(pid, e) } else { set_cpu_sets(pid, &[]) },
        None => Err("no efficiency cores on this CPU".into()),
    }
}

/// 0 very low (idle class), 1 low, 2 normal: Linux ioprio_set on every thread.
pub fn set_io_priority(pid: u32, prio: u32) -> Result<(), String> {
    const CLASS_BE: i32 = 2; const CLASS_IDLE: i32 = 3; const WHO_PROCESS: i32 = 1;
    let v = match prio { 0 => CLASS_IDLE << 13, 1 => (CLASS_BE << 13) | 7, _ => (CLASS_BE << 13) | 4 };
    let mut ok = false;
    for t in tasks(pid) { if unsafe { libc::syscall(libc::SYS_ioprio_set, WHO_PROCESS, t, v) } == 0 { ok = true; } }
    if ok { Ok(()) } else { Err(format!("ioprio_set({pid}): {}", errno())) }
}

/// Linux has no memory priority for a process; the kernel decides by recent use.
pub fn set_memory_priority(_pid: u32, _prio: u32) -> Result<(), String> { Err("memory priority does not exist on Linux".into()) }

/// (process mask, system mask), first 64 cores.
pub fn get_affinity(pid: u32) -> Option<(u64, u64)> {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(pid as i32, std::mem::size_of::<libc::cpu_set_t>(), &mut set) != 0 { return None; }
        let mut p = 0u64;
        for c in 0..64 { if libc::CPU_ISSET(c, &set) { p |= 1 << c; } }
        let s = online_cpus().iter().filter(|c| **c < 64).fold(0u64, |m, c| m | 1 << c);
        Some((p, s))
    }
}

pub fn set_affinity(pid: u32, mask: u64) -> Result<(), String> {
    let cpus: Vec<u32> = (0..64).filter(|c| mask & (1 << c) != 0).collect();
    set_cpus(pid, &cpus)
}

/// CPU caps use cgroups under your user session: later step.
pub fn cap_cpu(_pid: u32, _percent: u32) -> Result<(), String> { Err("CPU caps are not on Linux yet".into()) }
pub fn uncap(_pid: u32) -> Result<(), String> { Ok(()) }

/// pid still has that start time and program name.
pub fn image_matches(pid: u32, created: u64, name: &str) -> bool {
    match stat(pid) {
        Some((comm, _, start, _)) => created_of(start, boot_unix(), clk_tck()) == created && name_of(pid, &comm).eq_ignore_ascii_case(name),
        None => false,
    }
}

pub fn cmdline(pid: u32) -> String {
    fs::read(format!("/proc/{pid}/cmdline")).map(|b| {
        let s: Vec<String> = b.split(|c| *c == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned()).collect();
        s.join(" ")
    }).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sees_itself() {
        let me = std::process::id();
        let v = snapshot(&mut Vec::new());
        let p = v.iter().find(|p| p.pid == me).expect("own pid");
        assert!(p.created > 0 && !p.name.is_empty());
        assert!(image_matches(me, p.created, &p.name));
        assert!(!cmdline(me).is_empty());
        assert!(memory_status().map_or(false, |(u, a)| u > 0.0 && a > 0.0));
        assert!(boot_unix() > 1.0e9);
    }

    #[test]
    fn lowers_a_child() {
        let mut c = std::process::Command::new("sleep").arg("5").spawn().unwrap();
        let pid = c.id();
        assert!(set_priority(pid, BELOW_NORMAL).is_ok());
        assert_eq!(get_priority(pid), Some(BELOW_NORMAL));
        assert!(set_io_priority(pid, 1).is_ok());
        let _ = c.kill(); let _ = c.wait();
    }

    #[test]
    fn ranges() { assert_eq!(parse_ranges("0-2,5,7-8"), vec![0, 1, 2, 5, 7, 8]); }
}
