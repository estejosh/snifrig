//! snifrig snif for Linux: what is slowing this PC down, in plain words, at nearly no cost.
//! First version: CPU, memory and disk pressure from the kernel's pressure-stall counters
//! (/proc/pressure, kernel 4.20 and later; run-queue and I/O wait otherwise), the busiest programs,
//! and status.json in the same shape as on Windows, so the snifrig window and the AI tools
//! (snifrig-mcp) work unchanged. Leak tracking, the junk and model scans and the service setup
//! come in the following steps.
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

#[path = "../fixer/src/os_ext.rs"]
#[allow(dead_code)]
pub mod os_ext;
#[path = "../fixer/src/junk.rs"]
#[allow(dead_code)]
pub mod junk;
#[path = "../fixer/src/disk.rs"]
#[allow(dead_code)]
pub mod disk;

const LIC_VER: &str = "UFL-3.7";

pub fn json_str(s: &str, key: &str) -> Option<String> {
    let pat = format!("\"{}\":\"", key);
    let i = s.find(&pat)? + pat.len();
    Some(s[i..].split('"').next()?.to_string())
}
pub fn json_num(s: &str, key: &str) -> u64 {
    let pat = format!("\"{}\":", key);
    s.find(&pat).map(|i| s[i + pat.len()..].chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0)).unwrap_or(0)
}

fn now() -> f64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0) }
fn esc(s: &str) -> String { s.replace('\\', "\\\\").replace('"', "\\\"") }

/// $XDG_STATE_HOME/snifrig, else ~/.local/state/snifrig (per user, like %LOCALAPPDATA% on Windows).
pub fn default_dir() -> PathBuf {
    if let Some(s) = std::env::var_os("XDG_STATE_HOME").filter(|s| !s.is_empty()) { return PathBuf::from(s).join("snifrig"); }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state/snifrig")).unwrap_or_else(|| PathBuf::from("data"))
}

// ---------- license acceptance (UFL 3.7 section 9; a local record only) ----------
fn accepted(dir: &PathBuf) -> bool {
    fs::read_to_string(dir.join("license-accepted.json")).ok().and_then(|s| json_str(&s, "license")).as_deref() == Some(LIC_VER)
}
fn ensure_accepted(dir: &PathBuf, given: Option<String>) -> bool {
    if accepted(dir) { return true; }
    let given = given.or_else(|| std::env::var("SNIFRIG_ACCEPT_LICENSE").ok());
    let ok = if given.as_deref() == Some(LIC_VER) { true } else {
        eprintln!("Snifrig is licensed under the Usufruct License (UFL) 3.7, Operational Scope: Unconditional. The monitor is free for everyone.");
        eprintln!("Full license: https://github.com/estejosh/snifrig/blob/main/LICENSE");
        eprintln!("Type exactly `I accept {}` to continue, or run with --accept-license {}:", LIC_VER, LIC_VER);
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).is_ok() && line.trim() == format!("I accept {}", LIC_VER)
    };
    if ok { let _ = fs::write(dir.join("license-accepted.json"), format!("{{\"license\":\"{}\",\"scope\":\"Unconditional\",\"notice\":true,\"unix\":{:.0}}}", LIC_VER, now())); }
    else { eprintln!("Not accepted. Exiting."); }
    ok
}

// ---------- readers ----------
/// (busy, total) jiffies of all CPUs.
fn cpu_times() -> Option<(u64, u64)> {
    let s = fs::read_to_string("/proc/stat").ok()?;
    let v: Vec<u64> = s.lines().next()?.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
    let total: u64 = v.iter().take(8).sum();
    let idle = v.get(3).copied().unwrap_or(0) + v.get(4).copied().unwrap_or(0);
    Some((total.saturating_sub(idle), total))
}

/// "some avg10" of /proc/pressure/<what>: % of the last 10 s some task waited. None if the kernel
/// has pressure-stall information turned off.
pub fn pressure(what: &str) -> Option<f64> {
    let s = fs::read_to_string(format!("/proc/pressure/{what}")).ok()?;
    s.lines().find(|l| l.starts_with("some"))?.split_whitespace().find_map(|f| f.strip_prefix("avg10=")?.parse().ok())
}

fn meminfo_mb(key: &str, m: &str) -> f64 {
    m.lines().find_map(|l| l.strip_prefix(key)?.trim().split_whitespace().next()?.parse::<f64>().ok()).unwrap_or(0.0) / 1024.0
}

#[derive(Clone)]
struct P { name: String, ticks: u64, rss_mb: f64 }

fn procs() -> HashMap<u32, P> {
    let mut out = HashMap::new();
    let Ok(rd) = fs::read_dir("/proc") else { return out };
    for e in rd.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        let Ok(s) = fs::read_to_string(format!("/proc/{pid}/stat")) else { continue };
        let (Some(a), Some(b)) = (s.find('('), s.rfind(')')) else { continue };
        let f: Vec<&str> = s[b + 2..].split_whitespace().collect();
        let ticks = f.get(11).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0) + f.get(12).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
        let rss_pages: f64 = f.get(21).and_then(|x| x.parse().ok()).unwrap_or(0.0);
        out.insert(pid, P { name: s[a + 1..b].to_string(), ticks, rss_mb: rss_pages * 4096.0 / 1048576.0 });
    }
    out
}

/// One look at the machine: the headline and causes, in the same words style as Windows.
struct Look { cpu_pct: f64, avail_mb: f64, used_mb: f64, psi: (Option<f64>, Option<f64>, Option<f64>), top_cpu: Vec<(String, f64)>, top_mem: Vec<(String, f64)> }

fn verdict(l: &Look) -> (u64, String, Vec<String>) {
    let (cpu, mem, io) = l.psi;
    let mut causes = Vec::new();
    let busiest = l.top_cpu.first().map(|(n, p)| format!("{} ({:.0}% of a core)", n, p)).unwrap_or_default();
    let biggest = l.top_mem.first().map(|(n, m)| format!("{} ({:.1} GB)", n, m / 1024.0)).unwrap_or_default();
    let mut sev = 0;
    if let Some(c) = cpu { if c >= 20.0 { sev = sev.max(if c >= 50.0 { 2 } else { 1 }); causes.push(format!("Programs waited for the CPU {:.0}% of the last 10 seconds. Busiest: {}.", c, busiest)); } }
    if let Some(m) = mem { if m >= 5.0 { sev = sev.max(if m >= 20.0 { 2 } else { 1 }); causes.push(format!("Programs waited for memory {:.0}% of the last 10 seconds ({:.0} MB free). Biggest: {}.", m, l.avail_mb, biggest)); } }
    if let Some(i) = io { if i >= 20.0 { sev = sev.max(if i >= 50.0 { 2 } else { 1 }); causes.push(format!("Programs waited for the disk {:.0}% of the last 10 seconds.", i)); } }
    if cpu.is_none() && l.cpu_pct >= 90.0 { sev = sev.max(1); causes.push(format!("The CPU is {:.0}% busy. Busiest: {}.", l.cpu_pct, busiest)); }
    let headline = match sev { 0 => "Running smoothly.".to_string(), 1 => "A little slow right now.".to_string(), _ => "Slow right now.".to_string() };
    (sev, headline, causes)
}

fn write_status(dir: &PathBuf, t: f64, l: &Look) {
    let (sev, hl, causes) = verdict(l);
    let cs = causes.iter().map(|c| format!("\"{}\"", esc(c))).collect::<Vec<_>>().join(",");
    let top = l.top_cpu.iter().take(5).map(|(n, p)| format!("{{\"name\":\"{}\",\"cpu_pct\":{:.1}}}", esc(n), p)).collect::<Vec<_>>().join(",");
    let psi = |v: Option<f64>| v.map(|x| format!("{:.1}", x)).unwrap_or_else(|| "null".into());
    let line = format!("{{\"unix\":{:.0},\"level\":\"{}\",\"avail_mb\":{:.0},\"commit_mb\":{:.0},\"alerts_2h\":0,\"last_alert_unix\":0,\"last_alert\":\"\",\"paused_until\":0,\"os\":\"linux\",\"cpu_pct\":{:.1},\"pressure\":{{\"cpu\":{},\"memory\":{},\"io\":{}}},\"top\":[{}],\"severity\":{},\"headline\":\"{}\",\"causes\":[{}]}}",
        t, if sev >= 2 { "alert" } else { "ok" }, l.avail_mb, l.used_mb, l.cpu_pct, psi(l.psi.0), psi(l.psi.1), psi(l.psi.2), top, sev, esc(&hl), cs);
    let tmp = dir.join("status.json.tmp");
    if fs::write(&tmp, line).is_ok() { let _ = fs::rename(tmp, dir.join("status.json")); }
}

fn look(prev: &mut Option<(f64, (u64, u64), HashMap<u32, P>)>) -> Option<Look> {
    let t = now();
    let ct = cpu_times()?;
    let ps = procs();
    let m = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let (total, avail) = (meminfo_mb("MemTotal:", &m), meminfo_mb("MemAvailable:", &m));
    let swap_used = meminfo_mb("SwapTotal:", &m) - meminfo_mb("SwapFree:", &m);
    let out = prev.as_ref().map(|(t0, c0, p0)| {
        let dt = (t - t0).max(0.5);
        let cpu_pct = if ct.1 > c0.1 { (ct.0 - c0.0) as f64 / (ct.1 - c0.1) as f64 * 100.0 } else { 0.0 };
        let tck = 100.0;
        let mut by: HashMap<String, f64> = HashMap::new();
        let mut mem: HashMap<String, f64> = HashMap::new();
        for (pid, p) in &ps {
            if let Some(q) = p0.get(pid) { if q.name == p.name { *by.entry(p.name.clone()).or_default() += p.ticks.saturating_sub(q.ticks) as f64 / tck / dt * 100.0; } }
            *mem.entry(p.name.clone()).or_default() += p.rss_mb;
        }
        let mut top_cpu: Vec<(String, f64)> = by.into_iter().filter(|x| x.1 >= 1.0).collect();
        top_cpu.sort_by(|a, b| b.1.total_cmp(&a.1));
        let mut top_mem: Vec<(String, f64)> = mem.into_iter().collect();
        top_mem.sort_by(|a, b| b.1.total_cmp(&a.1));
        top_cpu.truncate(10); top_mem.truncate(10);
        Look { cpu_pct, avail_mb: avail, used_mb: total - avail + swap_used, psi: (pressure("cpu"), pressure("memory"), pressure("io")), top_cpu, top_mem }
    });
    *prev = Some((t, ct, ps));
    out
}

fn usage() {
    println!("snifrig {} for Linux: what is slowing this PC down.\n", env!("CARGO_PKG_VERSION"));
    println!("  snifrig            run the monitor (writes status.json every 5 s)");
    println!("  snifrig why        why is it slow right now?");
    println!("  snifrig junk       what takes space for nothing (free scan)");
    println!("  --dir PATH         data folder (default {})", default_dir().display());
}

pub fn run() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let flag = |n: &str| a.iter().position(|x| x == n).and_then(|i| a.get(i + 1)).cloned();
    let has = |n: &str| a.iter().any(|x| x == n);
    if has("--help") || has("-h") || a.first().map_or(false, |s| s == "help") { usage(); return; }
    if has("--version") { println!("snifrig {}", env!("CARGO_PKG_VERSION")); return; }
    let dir = flag("--dir").map(PathBuf::from).unwrap_or_else(default_dir);
    let _ = fs::create_dir_all(&dir);
    if !ensure_accepted(&dir, flag("--accept-license")) { std::process::exit(1); }
    unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 19); }
    match a.first().map(|s| s.as_str()) {
        Some("why") => {
            let mut prev = None;
            let _ = look(&mut prev);
            std::thread::sleep(Duration::from_secs(2));
            if let Some(l) = look(&mut prev) {
                let (_, hl, causes) = verdict(&l);
                println!("{}", hl);
                for c in causes { println!("- {}", c); }
                if l.psi.0.is_none() { println!("(This kernel has pressure-stall information off, so snifrig judges by CPU use alone. Add psi=1 to the kernel command line for a better reading.)"); }
            }
            return;
        }
        Some("junk") => {
            let total = junk::scan(&dir);
            println!("Found {} you could free. Details: {}", junk::human(total), dir.join(junk::REPORT).display());
            return;
        }
        _ => {}
    }
    let mut prev = None;
    loop {
        if let Some(l) = look(&mut prev) { write_status(&dir, now(), &l); }
        std::thread::sleep(Duration::from_secs(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn looks_and_judges() {
        let mut prev = None;
        assert!(look(&mut prev).is_none());
        std::thread::sleep(Duration::from_millis(600));
        let l = look(&mut prev).expect("second look");
        assert!(l.avail_mb > 0.0 && l.cpu_pct >= 0.0 && l.cpu_pct <= 100.0);
        let calm = Look { cpu_pct: 5.0, avail_mb: 8000.0, used_mb: 4000.0, psi: (Some(1.0), Some(0.0), Some(0.0)), top_cpu: vec![], top_mem: vec![] };
        assert_eq!(verdict(&calm).0, 0);
        let busy = Look { psi: (Some(60.0), Some(0.0), Some(0.0)), top_cpu: vec![("cc1plus".into(), 380.0)], ..calm };
        let (s, _, c) = verdict(&busy);
        assert_eq!(s, 2);
        assert!(c[0].contains("cc1plus"));
    }
}
