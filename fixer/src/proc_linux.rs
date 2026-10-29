//! Resolve processes on Linux, same contract as proc_windows.rs.
//! - private_mb: anonymous resident memory plus swapped out (what the program itself holds).
//! - service: the systemd unit the process runs in, when it is a service (`foo.service`).
use crate::{now_unix, sys, Target};
use std::fs;

/// Program name of pid (e.g. "python3"), for identity re-checks right before acting.
pub fn image_name(pid: u32) -> Option<String> {
    sys::snapshot_one(pid).map(|p| p.name)
}

fn status_kb(pid: u32, key: &str) -> f64 {
    fs::read_to_string(format!("/proc/{pid}/status")).ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix(key)?.trim().split_whitespace().next()?.parse::<f64>().ok()))
        .unwrap_or(0.0)
}

fn service(pid: u32) -> Option<String> {
    let c = fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let path = c.lines().find_map(|l| l.strip_prefix("0::"))?;
    let unit = path.rsplit('/').find(|p| !p.is_empty())?;
    unit.ends_with(".service").then(|| unit.to_string())
}

pub fn target(pid: u32) -> Option<Target> {
    let p = sys::snapshot_one(pid)?;
    let started = p.created as f64 / 1e7 - 11_644_473_600.0;
    Some(Target {
        pid,
        name: p.name,
        age_secs: (now_unix() - started).max(0.0),
        cmdline: sys::cmdline(pid),
        service: service(pid),
        private_mb: (status_kb(pid, "RssAnon:") + status_kb(pid, "VmSwap:")) / 1024.0,
    })
}

/// Biggest private-memory processes, excluding this one.
pub fn top_private(n: usize) -> Vec<Target> {
    let me = std::process::id();
    let mut v: Vec<Target> = sys::snapshot(&mut Vec::new()).into_iter().filter(|p| p.pid != me && p.pid > 1).filter_map(|p| target(p.pid)).collect();
    v.sort_by(|a, b| b.private_mb.partial_cmp(&a.private_mb).unwrap_or(std::cmp::Ordering::Equal));
    v.truncate(n);
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn target_self() {
        let t = target(std::process::id()).unwrap();
        assert!(t.private_mb > 0.1 && t.private_mb < 4096.0, "{}", t.private_mb);
        assert!(!t.cmdline.is_empty());
        assert!(target(u32::MAX - 1).is_none());
    }
}
