//! Looks inside WSL when its VM (vmmemWSL) holds a lot of memory, to say *what* holds it:
//! Linux programs, Linux file cache, or memory held outside Linux (typically GPU memory a CUDA
//! program in WSL mapped from Windows). Runs at most every 10 minutes and only while the VM
//! is already running and big, so it never wakes WSL and costs one short `wsl.exe` call.
use std::path::Path;

pub const MIN_GB: f64 = 4.0;
const EVERY_SECS: f64 = 600.0;
const ALERT_EVERY: f64 = 3600.0;
const CAP: u64 = 256 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct Inside { pub total_gb: f64, pub free_gb: f64, pub cache_gb: f64, pub anon_gb: f64, pub top: Vec<(String, f64)> }

/// Parse the probe output: /proc/meminfo lines, a "---" line, then "rss_kb command" lines.
pub fn parse(out: &str) -> Option<Inside> {
    let (mem, ps) = out.split_once("---").unwrap_or((out, ""));
    let kb = |k: &str| mem.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<f64>().ok()).map(|v| v / 1048576.0);
    let total_gb = kb("MemTotal:")?;
    let mut top: Vec<(String, f64)> = Vec::new();
    for l in ps.lines() {
        let mut it = l.split_whitespace();
        let (Some(r), Some(name)) = (it.next(), it.next()) else { continue };
        let Ok(r) = r.parse::<f64>() else { continue };
        let gb = r / 1048576.0;
        if let Some(x) = top.iter_mut().find(|x| x.0 == name) { x.1 += gb; } else { top.push((name.to_string(), gb)); }
    }
    top.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    top.truncate(5);
    Some(Inside { total_gb, free_gb: kb("MemFree:").unwrap_or(0.0), cache_gb: kb("Cached:").unwrap_or(0.0), anon_gb: kb("AnonPages:").unwrap_or(0.0), top })
}

/// Pure: the plain-words finding, and whether it is worth an alert.
pub fn explain(vm_gb: f64, i: &Inside) -> (String, bool) {
    let in_linux = (i.total_gb - i.free_gb).max(0.0);
    let outside = (vm_gb - in_linux).max(0.0);
    let progs = i.top.iter().filter(|x| x.1 >= 0.1).map(|(n, g)| format!("{} {:.1} GB", n, g)).collect::<Vec<_>>().join(", ");
    let mut parts = vec![format!("WSL holds {:.1} GB of Windows memory. Inside, Linux uses {:.1} GB: programs {:.1} GB, file cache {:.1} GB", vm_gb, in_linux, i.anon_gb, i.cache_gb)];
    if !progs.is_empty() { parts.push(format!("biggest programs: {}", progs)); }
    if outside >= 3.0 {
        parts.push(format!("{:.1} GB is held outside Linux, which usually means a GPU program in WSL (CUDA, a model server) mapped memory from Windows; restarting that program or `wsl --shutdown` gives it back", outside));
    } else if i.cache_gb >= 3.0 {
        parts.push("most of it is file cache Linux keeps; it is returned when WSL goes idle, or at once with `wsl --shutdown`".into());
    }
    (format!("{}.", parts.join("; ")), outside >= 3.0 || vm_gb >= 12.0)
}

pub struct WslWatch { last: f64, last_alert: f64 }

impl WslWatch {
    pub fn new() -> Self { WslWatch { last: 0.0, last_alert: 0.0 } }

    /// Call every monitor cycle with the VM's private memory in GB (0 if no VM).
    /// Returns an alert (key, message) when one is due.
    pub fn tick(&mut self, t: f64, vm_gb: f64, dir: &Path, iso: &dyn Fn(f64) -> String) -> Option<(String, String)> {
        if vm_gb < MIN_GB || t - self.last < EVERY_SECS { return None; }
        self.last = t;
        let i = parse(&probe()?)?;
        let (msg, worth) = explain(vm_gb, &i);
        let top: Vec<String> = i.top.iter().map(|(n, g)| format!("[\"{}\",{:.2}]", n.replace('"', ""), g)).collect();
        let line = format!("{{\"t\":\"{}\",\"vm_gb\":{:.2},\"linux_total_gb\":{:.2},\"linux_free_gb\":{:.2},\"cache_gb\":{:.2},\"programs_gb\":{:.2},\"top\":[{}]}}",
            iso(t), vm_gb, i.total_gb, i.free_gb, i.cache_gb, i.anon_gb, top.join(","));
        crate::append_capped(&dir.join("wsl.jsonl"), &line, CAP);
        if worth && t - self.last_alert >= ALERT_EVERY { self.last_alert = t; return Some(("wsl:mem".into(), msg)); }
        None
    }
}

/// One short look inside the default WSL distro. None on any failure or after 10 s.
fn probe() -> Option<String> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    let mut c = Command::new("wsl.exe")
        .args(["-e", "sh", "-c", "grep -E '^(MemTotal|MemFree|Cached|AnonPages):' /proc/meminfo; echo ---; ps -eo rss=,comm= --sort=-rss | head -12"])
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).creation_flags(0x0800_0000).spawn().ok()?;
    for _ in 0..100 {
        if let Ok(Some(_)) = c.try_wait() { break; }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if c.try_wait().ok().flatten().is_none() { let _ = c.kill(); let _ = c.wait(); return None; }
    let mut s = String::new();
    std::io::Read::read_to_string(&mut c.stdout.take()?, &mut s).ok()?;
    Some(s)
}

#[cfg(test)]
mod wsl_tests {
    use super::*;
    const OUT: &str = "MemTotal:       10185320 kB\nMemFree:         1437892 kB\nCached:          3983116 kB\nAnonPages:       3705092 kB\n---\n1629144 mega-cmd-server\n1146916 python\n518884 omniroute (v16.\n216892 openclaw-gatewa\n100000 python\n";
    #[test]
    fn finds_memory_outside_linux() {
        let i = parse(OUT).unwrap();
        assert!((i.total_gb - 9.71).abs() < 0.01);
        assert_eq!(i.top[0].0, "mega-cmd-server");
        assert!((i.top[1].1 - 1.19).abs() < 0.01, "python processes are summed: {:?}", i.top);
        let (m, worth) = explain(22.8, &i);
        assert!(worth && m.contains("held outside Linux") && m.contains("14.5 GB"), "{}", m);
        let (m, worth) = explain(9.0, &i);
        assert!(!worth && m.contains("file cache"), "{}", m);
    }
}
