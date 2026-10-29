//! Live system sampling on Linux from /proc (no dependencies), same shapes as sys_windows.rs.
use std::fs;
use std::time::Duration;

pub struct Proc {
    pub pid: u64,
    pub ppid: u64,
    pub name: String,
    pub handles: u32,
    pub ws: u64,
    pub create: i64,
    pub cpu_time: u64, // user + system, 100ns units
}

pub struct Row {
    pub pid: u64,
    pub ppid: u64,
    pub name: String,
    pub handles: u32,
    pub ws_mb: f64,
    pub cpu: f64,
}

const TCK: u64 = 100; // USER_HZ is 100 on every Linux that snifrig runs on

pub fn procs() -> Vec<Proc> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir("/proc") else { return out };
    for e in rd.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u64>().ok()) else { continue };
        let Ok(s) = fs::read_to_string(format!("/proc/{pid}/stat")) else { continue };
        let (Some(a), Some(b)) = (s.find('('), s.rfind(')')) else { continue };
        let f: Vec<&str> = s[b + 2..].split_whitespace().collect();
        let n = |i: usize| f.get(i).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
        let handles = fs::read_dir(format!("/proc/{pid}/fd")).map(|d| d.count() as u32).unwrap_or(0);
        out.push(Proc { pid, ppid: n(1), name: s[a + 1..b].to_string(), handles, ws: n(21) * 4096, create: n(19) as i64, cpu_time: (n(11) + n(12)) * (10_000_000 / TCK) });
    }
    out
}

/// (idle, total) jiffies.
fn times() -> (u64, u64) {
    let s = fs::read_to_string("/proc/stat").unwrap_or_default();
    let v: Vec<u64> = s.lines().next().unwrap_or("").split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
    (v.get(3).copied().unwrap_or(0) + v.get(4).copied().unwrap_or(0), v.iter().take(8).sum())
}

pub fn cpu_now() -> f64 {
    let a = times();
    std::thread::sleep(Duration::from_millis(500));
    let b = times();
    let tot = b.1.saturating_sub(a.1) as f64;
    if tot <= 0.0 { return 0.0; }
    (100.0 * (1.0 - b.0.saturating_sub(a.0) as f64 / tot)).clamp(0.0, 100.0)
}

/// (used_mb, total_mb)
pub fn ram() -> (f64, f64) {
    let m = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let get = |k: &str| m.lines().find_map(|l| l.strip_prefix(k)?.trim().split_whitespace().next()?.parse::<f64>().ok()).unwrap_or(0.0) / 1024.0;
    let t = get("MemTotal:");
    (t - get("MemAvailable:"), t)
}

/// Returns (rows sorted, total machine CPU %). Row cpu is the share of the whole machine.
pub fn top(limit: usize, by_mem: bool) -> (Vec<Row>, f64) {
    let a = procs();
    let t1 = times();
    std::thread::sleep(Duration::from_millis(1000));
    let b = procs();
    let t2 = times();
    let dsys = t2.1.saturating_sub(t1.1) as f64;
    let busy = if dsys > 0.0 { 100.0 * (1.0 - t2.0.saturating_sub(t1.0) as f64 / dsys) } else { 0.0 };
    // All cores' time over the sample, in 100ns: the whole machine.
    let machine = dsys * (10_000_000 / TCK) as f64;
    let prev: std::collections::HashMap<(u64, i64), u64> = a.iter().map(|p| ((p.pid, p.create), p.cpu_time)).collect();
    let mut rows: Vec<Row> = b.into_iter().map(|p| {
        let d = prev.get(&(p.pid, p.create)).map(|o| p.cpu_time.saturating_sub(*o)).unwrap_or(0) as f64;
        Row { pid: p.pid, ppid: p.ppid, name: p.name, handles: p.handles, ws_mb: p.ws as f64 / 1048576.0, cpu: if machine > 0.0 { (100.0 * d / machine).min(100.0) } else { 0.0 } }
    }).collect();
    if by_mem { rows.sort_by(|x, y| y.ws_mb.total_cmp(&x.ws_mb)); } else { rows.sort_by(|x, y| y.cpu.total_cmp(&x.cpu).then(y.ws_mb.total_cmp(&x.ws_mb))); }
    rows.truncate(limit);
    (rows, busy.clamp(0.0, 100.0))
}
