//! Judgment before asking the user anything:
//! - say WHAT a process is ("ComfyUI image server", "Python script build.py"), not just python.exe,
//! - know which processes are servers other programs talk to (listening on a TCP port),
//! - remember a "no" (Dismiss) so the same question is not asked again for a week, and stop
//!   asking for good after two "no"s,
//! - never ask the same question twice in a day.
use std::collections::HashMap;
use std::ffi::c_void;
use std::path::Path;

pub const DECLINE_SECS: f64 = 7.0 * 86400.0;
pub const DECLINE_FOREVER: usize = 2;
pub const ASK_AGAIN_SECS: f64 = 86400.0;

/// A plain name for what the process is, from its image name and command line.
pub fn identity(name: &str, cmdline: &str) -> String {
    let n = name.to_lowercase();
    let c = cmdline.to_lowercase();
    let args: Vec<&str> = cmdline.split_whitespace().map(|a| a.trim_matches('"')).collect();
    let base = |p: &str| p.rsplit(['\\', '/']).next().unwrap_or(p).to_string();
    if n.starts_with("llama-server") { return "llama.cpp model server".into(); }
    if n.starts_with("ollama") { return "Ollama model server".into(); }
    if n.starts_with("python") || n.starts_with("pythonw") || n == "py.exe" {
        if c.contains("comfyui") || (args.iter().any(|a| a.eq_ignore_ascii_case("main.py")) && c.contains("8188")) { return "ComfyUI (image generation server)".into(); }
        if let Some(i) = args.iter().position(|a| *a == "-m") { if let Some(m) = args.get(i + 1) { return format!("Python module {}", m); } }
        if let Some(s) = args.iter().skip(1).find(|a| a.to_lowercase().ends_with(".py")) { return format!("Python script {}", base(s)); }
        return "Python".into();
    }
    if n == "node.exe" || n == "bun.exe" || n == "deno.exe" {
        if let Some(s) = args.iter().skip(1).find(|a| { let l = a.to_lowercase(); l.ends_with(".js") || l.ends_with(".mjs") || l.ends_with(".ts") || l.ends_with(".cjs") }) { return format!("Node script {}", base(s)); }
        if let Some(s) = args.get(1) { if !s.starts_with('-') { return format!("Node app {}", base(s)); } }
        return "Node.js".into();
    }
    if n == "powershell.exe" || n == "pwsh.exe" {
        if let Some(i) = args.iter().position(|a| a.eq_ignore_ascii_case("-file")) { if let Some(s) = args.get(i + 1) { return format!("PowerShell script {}", base(s)); } }
    }
    if n == "java.exe" || n == "javaw.exe" {
        if let Some(i) = args.iter().position(|a| *a == "-jar") { if let Some(s) = args.get(i + 1) { return format!("Java app {}", base(s)); } }
    }
    name.to_string()
}

#[cfg(windows)]
#[link(name = "iphlpapi", kind = "raw-dylib")]
extern "system" { fn GetExtendedTcpTable(t: *mut c_void, size: *mut u32, order: i32, af: u32, class: i32, reserved: u32) -> u32; }

/// Linux: listening sockets from /proc/net/tcp and tcp6 (state 0A), matched to the process that
/// holds each socket. Only your own processes can be matched without root, which is the point.
#[cfg(not(windows))]
fn table(af: u32, _row: usize, _pid_at: usize, _port_at: usize, out: &mut HashMap<u32, u16>) {
    let file = if af == 2 { "/proc/net/tcp" } else { "/proc/net/tcp6" };
    let mut by_inode: HashMap<u64, u16> = HashMap::new();
    for l in std::fs::read_to_string(file).unwrap_or_default().lines().skip(1) {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 10 || f[3] != "0A" { continue; }
        let port = f[1].rsplit(':').next().and_then(|p| u16::from_str_radix(p, 16).ok()).unwrap_or(0);
        if let Ok(ino) = f[9].parse::<u64>() { if ino != 0 { by_inode.insert(ino, port); } }
    }
    if by_inode.is_empty() { return; }
    let Ok(rd) = std::fs::read_dir("/proc") else { return };
    for e in rd.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else { continue };
        for fd in fds.flatten() {
            let Ok(t) = std::fs::read_link(fd.path()) else { continue };
            let t = t.to_string_lossy();
            if let Some(ino) = t.strip_prefix("socket:[").and_then(|s| s.strip_suffix(']')).and_then(|s| s.parse::<u64>().ok()) {
                if let Some(p) = by_inode.get(&ino) { out.entry(pid).or_insert(*p); }
            }
        }
    }
}

#[cfg(windows)]
fn table(af: u32, row: usize, pid_at: usize, port_at: usize, out: &mut HashMap<u32, u16>) {
    let mut size = 0u32;
    unsafe { GetExtendedTcpTable(std::ptr::null_mut(), &mut size, 0, af, 3, 0); }
    if size == 0 { return; }
    let mut buf = vec![0u8; size as usize + 4096];
    let mut size = buf.len() as u32;
    if unsafe { GetExtendedTcpTable(buf.as_mut_ptr() as *mut c_void, &mut size, 0, af, 3, 0) } != 0 { return; }
    let rd = |o: usize| -> u32 { if o + 4 <= buf.len() { u32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]) } else { 0 } };
    let n = rd(0) as usize;
    for i in 0..n.min(100_000) {
        let base = 4 + i * row;
        if base + row > buf.len() { break; }
        let pid = rd(base + pid_at);
        let p = rd(base + port_at);
        let port = (((p & 0xff) << 8) | ((p >> 8) & 0xff)) as u16;
        if pid > 4 { out.entry(pid).or_insert(port); }
    }
}

/// Pids listening on a TCP port (IPv4 and IPv6), with one of their ports.
pub fn listening_pids() -> HashMap<u32, u16> {
    let mut m = HashMap::new();
    table(2, 24, 20, 8, &mut m);   // MIB_TCPROW_OWNER_PID
    table(23, 56, 52, 20, &mut m); // MIB_TCP6ROW_OWNER_PID
    m
}

/// Ledger lines (fixes.jsonl and its rotated copy) as (unix, name, action, status).
fn history(dir: &Path) -> Vec<(f64, String, String, String)> {
    let mut text = std::fs::read_to_string(dir.join("fixes.jsonl.old")).unwrap_or_default();
    text.push_str(&std::fs::read_to_string(dir.join("fixes.jsonl")).unwrap_or_default());
    text.lines().filter_map(|l| {
        let u = crate::json::num_field(l, "unix")?;
        Some((u, crate::json::str_field(l, "name")?.to_lowercase(), crate::json::str_field(l, "action").unwrap_or_default(), crate::json::str_field(l, "status").unwrap_or_default()))
    }).collect()
}

pub struct Memory { h: Vec<(f64, String, String, String)> }

impl Memory {
    pub fn load(dir: &Path) -> Memory { Memory { h: history(dir) } }

    /// The user said no to this (name, action): Some(when) if that still holds.
    pub fn declined(&self, name: &str, action: &str, now: f64) -> Option<f64> {
        let n = name.to_lowercase();
        let no: Vec<f64> = self.h.iter().filter(|x| x.1 == n && x.2 == action && x.3 == "dismissed").map(|x| x.0).collect();
        let last = no.iter().cloned().fold(None, |a: Option<f64>, b| Some(a.map_or(b, |a| a.max(b))))?;
        if no.len() >= DECLINE_FOREVER || now - last < DECLINE_SECS { Some(last) } else { None }
    }

    /// Same question already asked (queued) within a day.
    pub fn asked_recently(&self, name: &str, action: &str, now: f64) -> bool {
        let n = name.to_lowercase();
        self.h.iter().any(|x| x.1 == n && x.2 == action && x.3 == "queued" && now - x.0 < ASK_AGAIN_SECS)
    }

    /// The same action was done to this name within `secs`.
    pub fn done_within(&self, name: &str, action: &str, now: f64, secs: f64) -> bool {
        let n = name.to_lowercase();
        self.h.iter().any(|x| x.1 == n && x.2 == action && x.3 == "done" && now - x.0 < secs)
    }

    #[cfg(test)]
    pub fn from(h: Vec<(f64, &str, &str, &str)>) -> Memory { Memory { h: h.into_iter().map(|(u, n, a, s)| (u, n.to_string(), a.to_string(), s.to_string())).collect() } }
}

#[cfg(test)]
mod judge_tests {
    use super::*;
    #[test]
    fn names_what_it_is() {
        assert_eq!(identity("python.exe", r#""X:\Python\python.exe" main.py --listen 127.0.0.1 --port 8188 --fp16-text-enc"#), "ComfyUI (image generation server)");
        assert_eq!(identity("python.exe", r#"python3 C:\NATV-ops\producer.py --x"#), "Python script producer.py");
        assert_eq!(identity("python.exe", "python -m jev.server --port 8791"), "Python module jev.server");
        assert_eq!(identity("node.exe", r#"node C:\a\b\server.mjs"#), "Node script server.mjs");
        assert_eq!(identity("powershell.exe", r#"powershell -NoProfile -File C:\x\run-continuous.ps1"#), "PowerShell script run-continuous.ps1");
        assert_eq!(identity("comet.exe", "comet.exe --type=renderer"), "comet.exe");
    }
    #[test]
    fn remembers_no() {
        let now = 1_000_000.0;
        let m = Memory::from(vec![(now - 3600.0, "legendsofidleon.exe", "trim", "dismissed")]);
        assert!(m.declined("LegendsOfIdleon.exe", "trim", now).is_some());
        assert!(m.declined("LegendsOfIdleon.exe", "terminate", now).is_none());
        let old = Memory::from(vec![(now - 8.0 * 86400.0, "x.exe", "trim", "dismissed")]);
        assert!(old.declined("x.exe", "trim", now).is_none(), "one old no expires after a week");
        let twice = Memory::from(vec![(now - 30.0 * 86400.0, "x.exe", "trim", "dismissed"), (now - 20.0 * 86400.0, "x.exe", "trim", "dismissed")]);
        assert!(twice.declined("x.exe", "trim", now).is_some(), "two noes stick");
        let asked = Memory::from(vec![(now - 600.0, "python.exe", "terminate", "queued")]);
        assert!(asked.asked_recently("python.exe", "terminate", now) && !asked.asked_recently("python.exe", "terminate", now + 86400.0));
    }
}
