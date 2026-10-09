//! Maps one monitor alert to one intent. Pure: no system calls, so it is fully testable.
//!
//! Alert keys written by the monitor (src/lib.rs in the root crate):
//!   pm:<name>#<pid>  process private memory growing
//!   ph:<name>#<pid>  process handle count growing
//!   tag:<TAG>        kernel pool tag growing (no owning process)
//!   g:paged / g:nonpaged / g:commit   system-wide growth
//!   x:avail          available RAM under 1 GB
//!   x:commit         commit charge over 90% of the limit
//!   cpu:<name>#<pid> process (or name group) using a lot of CPU for 10+ minutes
//!   dup:<name>#<pid> idle twin of an identical running process
//!   x:vram           graphics memory 95% full
//!   big:<TAG>        a pool tag over 1 GB
//!   self             the monitor tripped its own budget guard
//! Any message may carry "Spawn burst watch: <name>#<pid> ..." naming a process that
//! is spawning children in a loop (the token-leak pattern snifrig was built to find).

use crate::{Action, Alert, Intent, Subject};

/// Windows shell hosts that the system relaunches by itself, so ending a stuck one is safe.
const AUTO_RESTART: [&str; 4] = ["startmenuexperiencehost.exe", "searchhost.exe", "shellexperiencehost.exe", "textinputhost.exe"];

/// Splits "python.exe#1234" into ("python.exe", 1234).
pub fn split_name_pid(s: &str) -> Option<(String, u32)> {
    let (n, p) = s.rsplit_once('#')?;
    let pid: u32 = p.trim().parse().ok()?;
    if n.is_empty() || pid == 0 { return None; }
    Some((n.to_string(), pid))
}

/// The top spawner named by the monitor's burst watch, if the message has one.
pub fn spawner(msg: &str) -> Option<(String, u32)> {
    let rest = &msg[msg.find("Spawn burst watch: ")? + "Spawn burst watch: ".len()..];
    let tok = rest.split(|c: char| c == ' ' || c == ',' || c == ';').next()?;
    let tok = tok.trim_end_matches('.');
    split_name_pid(tok)
}

pub fn plan(a: &Alert) -> Intent {
    let mk = |subject, action, why: &str| Intent { alert: a.clone(), subject, action, why: why.to_string() };
    let k = a.key.as_str();

    if let Some(rest) = k.strip_prefix("pm:") {
        if let Some((name, pid)) = split_name_pid(rest) {
            return mk(Subject::Pid { name, pid }, Action::Terminate,
                "its private memory keeps growing; restarting it gives the memory back");
        }
    }
    if let Some(rest) = k.strip_prefix("cpu:") {
        if let Some((name, pid)) = split_name_pid(rest) {
            if AUTO_RESTART.contains(&name.to_ascii_lowercase().as_str()) {
                return mk(Subject::Pid { name, pid }, Action::Terminate,
                    "it is stuck using a full CPU core; Windows restarts it automatically");
            }
            return mk(Subject::Pid { name, pid }, Action::LowerPriority,
                "it is using a lot of CPU; lowering its priority keeps the PC responsive without closing it");
        }
    }
    if let Some(rest) = k.strip_prefix("dup:") {
        if let Some((name, pid)) = split_name_pid(rest) {
            return mk(Subject::Pid { name, pid }, Action::Terminate,
                "it is an idle duplicate of another running copy");
        }
    }
    if k == "x:vram" {
        return mk(Subject::None, Action::Report, "graphics memory is full; closing a graphics app is a human decision");
    }
    if let Some(rest) = k.strip_prefix("ph:") {
        if let Some((name, pid)) = split_name_pid(rest) {
            return mk(Subject::Pid { name, pid }, Action::Terminate,
                "its handle count keeps growing; restarting it releases the handles");
        }
    }
    // A spawn loop behind any alert is the most actionable cause, so it wins over the rest.
    if let Some((name, pid)) = spawner(&a.msg) {
        return mk(Subject::Pid { name, pid }, Action::Terminate,
            "it is starting processes in a loop, which leaks kernel tokens and pool memory");
    }
    if k == "x:avail" {
        return mk(Subject::TopPrivate(1), Action::Trim,
            "RAM is nearly full; trimming the largest process frees physical memory without closing it");
    }
    mk(Subject::None, Action::Report, "no single process to act on; logged for review")
}
