//! snifrig - low-footprint Windows leak monitor (library: monitor loop, report, license steps, install).
//! One process, no child processes, no GPU use, idle priority, hard self-budgets.
//! Reads kernel tables directly (NtQuerySystemInformation) instead of spawning tools.
use std::collections::{HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives};
use windows_sys::Win32::System::ProcessStatus::{GetPerformanceInfo, PERFORMANCE_INFORMATION};
use windows_sys::Win32::System::Threading::{CreateMutexW, GetCurrentProcess, SetPriorityClass, IDLE_PRIORITY_CLASS};

#[link(name = "ntdll", kind = "raw-dylib")]
extern "system" {
    fn NtQuerySystemInformation(class: u32, buf: *mut u8, len: u32, ret: *mut u32) -> i32;
}

const CAP: usize = 1440; // samples kept per series (24h at 60s)
const MAX_BUF: usize = 64 << 20;
const JSONL_CAP: u64 = 1 << 20;
const ALERT_CAP: u64 = 256 << 10;
const REALERT_SECS: f64 = 2.0 * 3600.0;
const MB: f64 = 1048576.0;

// ---------- raw readers ----------
fn u16_at(b: &[u8], o: usize) -> u16 { u16::from_le_bytes([b[o], b[o + 1]]) }
fn u32_at(b: &[u8], o: usize) -> u32 { u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) }
fn u64_at(b: &[u8], o: usize) -> u64 { u64::from_le_bytes(b[o..o + 8].try_into().unwrap()) }

fn query(class: u32, buf: &mut Vec<u8>) -> bool {
    loop {
        let mut ret: u32 = 0;
        let st = unsafe { NtQuerySystemInformation(class, buf.as_mut_ptr(), buf.len() as u32, &mut ret) };
        if st == 0 { return true; }
        let u = st as u32;
        if u == 0xC000_0004 || u == 0xC000_0023 {
            let need = (ret as usize + 65536).max(buf.len() * 2);
            if need > MAX_BUF { return false; }
            buf.resize(need, 0);
            continue;
        }
        return false;
    }
}

#[derive(Clone)]
struct Proc { pid: u32, ppid: u32, name: String, handles: u32, ws: u64, private: u64, created: u64, threads: u32, cpu: u64 }

fn read_procs(buf: &mut Vec<u8>) -> Option<Vec<Proc>> {
    if !query(5, buf) { return None; }
    let base = buf.as_ptr() as u64;
    let mut out = Vec::with_capacity(700);
    let mut off = 0usize;
    loop {
        if off + 208 > buf.len() { break; }
        let next = u32_at(buf, off) as usize;
        let nlen = u16_at(buf, off + 56) as usize;
        let nptr = u64_at(buf, off + 64);
        let pid = u64_at(buf, off + 80) as u32;
        let name = if nlen > 0 && nptr >= base && ((nptr - base) as usize) + nlen <= buf.len() {
            let no = (nptr - base) as usize;
            let w: Vec<u16> = (0..nlen / 2).map(|i| u16::from_le_bytes([buf[no + 2 * i], buf[no + 2 * i + 1]])).collect();
            String::from_utf16_lossy(&w)
        } else if pid == 0 { "Idle".into() } else { String::new() };
        out.push(Proc {
            pid, ppid: u64_at(buf, off + 88) as u32, name,
            handles: u32_at(buf, off + 96), ws: u64_at(buf, off + 144), private: u64_at(buf, off + 200),
            created: u64_at(buf, off + 32), threads: u32_at(buf, off + 4),
            cpu: u64_at(buf, off + 40).wrapping_add(u64_at(buf, off + 48)),
        });
        if next == 0 { break; }
        off += next;
    }
    Some(out)
}

struct Tag { name: String, bytes: u64, live: i64, allocs: u64 }

fn read_tags(buf: &mut Vec<u8>) -> Option<Vec<Tag>> {
    if !query(22, buf) { return None; }
    let n = u32_at(buf, 0) as usize;
    let mut out = Vec::with_capacity(n.min(8192));
    for i in 0..n {
        let e = 8 + i * 40;
        if e + 40 > buf.len() { break; }
        let name: String = buf[e..e + 4].iter().map(|&c| if (32..127).contains(&c) { c as char } else { '?' }).collect();
        let pu = u64_at(buf, e + 16);
        let nu = u64_at(buf, e + 32);
        let live = (u32_at(buf, e + 4) as i64 - u32_at(buf, e + 8) as i64) + (u32_at(buf, e + 24) as i64 - u32_at(buf, e + 28) as i64);
        let bytes = pu + nu;
        let allocs = u32_at(buf, e + 4) as u64 + u32_at(buf, e + 24) as u64;
        if bytes > 0 { out.push(Tag { name, bytes, live, allocs }); }
    }
    Some(out)
}

struct Glob { commit_mb: f64, limit_mb: f64, avail_mb: f64, total_mb: f64, paged_mb: f64, nonpaged_mb: f64, handles: f64, procs: f64, threads: f64 }

fn read_glob() -> Glob {
    let mut pi: PERFORMANCE_INFORMATION = unsafe { std::mem::zeroed() };
    pi.cb = std::mem::size_of::<PERFORMANCE_INFORMATION>() as u32;
    unsafe { GetPerformanceInfo(&mut pi, pi.cb) };
    let ps = pi.PageSize as f64;
    Glob {
        commit_mb: pi.CommitTotal as f64 * ps / MB, limit_mb: pi.CommitLimit as f64 * ps / MB,
        avail_mb: pi.PhysicalAvailable as f64 * ps / MB, total_mb: pi.PhysicalTotal as f64 * ps / MB,
        paged_mb: pi.KernelPaged as f64 * ps / MB, nonpaged_mb: pi.KernelNonpaged as f64 * ps / MB,
        handles: pi.HandleCount as f64, procs: pi.ProcessCount as f64, threads: pi.ThreadCount as f64,
    }
}

fn disks() -> Vec<(String, f64, f64)> {
    let mut v = Vec::new();
    let mask = unsafe { GetLogicalDrives() };
    for i in 0..26u32 {
        if mask >> i & 1 == 0 { continue; }
        let root: Vec<u16> = format!("{}:\\", (b'A' + i as u8) as char).encode_utf16().chain(Some(0)).collect();
        if unsafe { GetDriveTypeW(root.as_ptr()) } != 3 { continue; }
        let (mut free, mut tot, mut tf) = (0u64, 0u64, 0u64);
        if unsafe { GetDiskFreeSpaceExW(root.as_ptr(), &mut free, &mut tot, &mut tf) } != 0 {
            v.push((format!("{}:", (b'A' + i as u8) as char), free as f64 / MB / 1024.0, tot as f64 / MB / 1024.0));
        }
    }
    v
}

// ---------- time series ----------
struct Series { v: VecDeque<(f64, f64)>, last_seen: u64, last_alert: f64 }
impl Series {
    fn new() -> Self { Series { v: VecDeque::new(), last_seen: 0, last_alert: 0.0 } }
    fn push(&mut self, t: f64, x: f64, cycle: u64) {
        if self.v.len() >= CAP { self.v.pop_front(); }
        self.v.push_back((t, x));
        self.last_seen = cycle;
    }
    /// (slope per hour, share of non-decreasing steps, points used, current value) over the last n points
    fn trend(&self, n: usize) -> Option<(f64, f64, usize, f64)> {
        let len = self.v.len();
        let m = len.min(n);
        if m < 10 { return None; }
        let pts: Vec<(f64, f64)> = self.v.iter().skip(len - m).cloned().collect();
        let t0 = pts[0].0;
        if pts[m - 1].0 - t0 < 300.0 { return None; } // need at least 5 minutes of history, or short bursts look like leaks
        let (mut sx, mut sy, mut sxx, mut sxy) = (0.0, 0.0, 0.0, 0.0);
        for &(t, y) in &pts { let x = (t - t0) / 3600.0; sx += x; sy += y; sxx += x * x; sxy += x * y; }
        let k = m as f64;
        let den = k * sxx - sx * sx;
        if den.abs() < 1e-12 { return None; }
        let slope = (k * sxy - sx * sy) / den;
        let up = pts.windows(2).filter(|w| w[1].1 >= w[0].1).count() as f64 / (m - 1) as f64;
        Some((slope, up, m, pts[m - 1].1))
    }
}

fn tag_hint(tag: &str) -> &'static str {
    match tag {
        "Toke" | "SeAt" | "SeTd" | "SeTl" => "token objects: security tokens are being kept alive. Usually process-spawn churn (short-lived child processes, esp. Git-bash/msys scripts in loops) or a service/driver holding token references. Cut spawn rate, find the top spawner below, then restart the offender; a reboot clears what is already leaked.",
        "FMfn" | "FMfc" | "FMsl" | "FMfl" => "Filter Manager name/context cache: a file-system minifilter (antivirus, backup, sync, container) is holding per-file contexts. Check `fltmc filters` (admin) and update/exclude paths in the AV or sync tool.",
        "WCsc" | "WCri" | "WCsn" => "wcifs.sys (Windows Container Isolation): stream contexts not freed. Seen after a burst of file access through container/MSIX/bind layers; clears on reboot. Check Windows updates for wcifs fixes.",
        "File" | "Filo" => "file objects held open at kernel level (often a driver or the System process). Compare System-process handle count; if it grows, a driver is leaking handles - update/remove recently added file-system drivers.",
        "NtfF" | "NtfC" | "NtfR" | "NtFs" => "NTFS metadata caches: often benign and reclaimable after large directory scans. Only act if commit is also climbing.",
        "Proc" | "Thre" | "Job " => "process/thread/job objects retained after exit: some process is holding handles to dead processes. Look at processes with very high handle counts and restart them.",
        "MmSt" | "Mm  " | "MmCa" => "memory-manager structures (mapped sections): large mapped-file use or many open section views. Check for apps mapping big files.",
        _ => "unknown tag: look it up in pooltag.txt (Windows Kits\\10\\Debuggers\\x64\\triage\\) or search driver files for the 4-char tag; correlate growth with spawn rate / a specific app.",
    }
}

// ---------- helpers ----------
fn now() -> f64 { SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0) }
fn esc(s: &str) -> String { s.replace('\\', "\\\\").replace('"', "\\\"") }

fn iso(t: f64) -> String {
    let s = t as i64;
    let (days, rem) = (s.div_euclid(86400), s.rem_euclid(86400));
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Append a line; when the file passes `cap` bytes it is rotated to `.old` (so disk use is bounded at 2x cap).
fn append_capped(path: &PathBuf, line: &str, cap: u64) {
    if let Ok(m) = fs::metadata(path) {
        if m.len() > cap {
            let old = path.with_extension("old");
            let _ = fs::remove_file(&old);
            let _ = fs::rename(path, &old);
        }
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{}", line);
    }
}

fn mb(x: u64) -> f64 { x as f64 / MB }

fn top_n<T, F: Fn(&T) -> f64>(v: &mut Vec<T>, n: usize, key: F) {
    v.sort_by(|a, b| key(b).partial_cmp(&key(a)).unwrap_or(std::cmp::Ordering::Equal));
    v.truncate(n);
}

// ---------- monitor ----------
struct Mon {
    dir: PathBuf,
    interval: f64,
    buf: Vec<u8>,
    cycle: u64,
    series: HashMap<String, Series>,
    cool: HashMap<String, f64>,
    prev_alloc: HashMap<String, u64>,
    tag_live: HashMap<String, i64>,
    young: Vec<(String, u32)>,
    top_tags: Vec<(String, f64)>,
    own_cpu: VecDeque<f64>,
    last_own: Option<(f64, u64)>,
    base: Option<(u64, u32)>,
    base_threads: u32,
    quiet: bool,
    last_alert_t: f64,
    last_alert_msg: String,
    alert_times: VecDeque<f64>,
}

impl Mon {
    fn new(dir: PathBuf, interval: f64, quiet: bool) -> Self {
        Mon {
            dir, interval, buf: vec![0u8; 1 << 20], cycle: 0, series: HashMap::new(), cool: HashMap::new(),
            prev_alloc: HashMap::new(), tag_live: HashMap::new(), young: Vec::new(), top_tags: Vec::new(),
            own_cpu: VecDeque::new(), last_own: None, base: None, base_threads: 0, quiet,
            last_alert_t: 0.0, last_alert_msg: String::new(), alert_times: VecDeque::new(),
        }
    }

    fn put(&mut self, key: String, t: f64, x: f64) {
        let c = self.cycle;
        self.series.entry(key).or_insert_with(Series::new).push(t, x, c);
    }

    fn alert(&mut self, t: f64, key: &str, msg: &str) {
        let line = format!("{{\"t\":\"{}\",\"key\":\"{}\",\"msg\":\"{}\"}}", iso(t), esc(key), esc(msg));
        ALERTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.last_alert_t = t;
        self.last_alert_msg = msg.to_string();
        self.alert_times.push_back(t);
        append_capped(&self.dir.join("alerts.jsonl"), &line, ALERT_CAP);
        if !self.quiet { println!("ALERT {} {}", iso(t), msg); }
    }
}

impl Mon {
    /// One sampling cycle. Returns false if the self-budget guard tripped (caller must exit).
    fn step(&mut self) -> bool {
        self.cycle += 1;
        let t = now();
        let g = read_glob();
        let procs = read_procs(&mut self.buf).unwrap_or_default();
        let mut tags = read_tags(&mut self.buf).unwrap_or_default();

        for (k, v) in [("commit", g.commit_mb), ("avail", g.avail_mb), ("paged", g.paged_mb), ("nonpaged", g.nonpaged_mb),
                       ("handles", g.handles), ("procs", g.procs), ("threads", g.threads)] {
            self.put(format!("g:{}", k), t, v);
        }
        // allocation counters on Proc/Toke tags are cumulative: their delta is the true spawn rate,
        // including processes too short-lived for any snapshot to see.
        for want in ["Proc", "Toke"] {
            if let Some(tg) = tags.iter().find(|x| x.name == want) {
                if let Some(&p) = self.prev_alloc.get(want) {
                    let rate = tg.allocs.saturating_sub(p) as f64 * 60.0 / self.interval;
                    self.put(format!("g:{}_per_min", want), t, rate);
                }
                self.prev_alloc.insert(want.to_string(), tg.allocs);
            }
        }
        self.tag_live.clear();
        top_n(&mut tags, 50, |x| x.bytes as f64);
        self.top_tags = tags.iter().take(5).map(|x| (x.name.clone(), mb(x.bytes))).collect();
        for tg in &tags {
            self.tag_live.insert(tg.name.clone(), tg.live);
            self.put(format!("tag:{}", tg.name), t, mb(tg.bytes));
        }

        let own = procs.iter().find(|p| p.pid == std::process::id()).cloned();
        let ft_now = (t * 1e7) as u64 + 116_444_736_000_000_000;
        let horizon = (self.interval * 2.0 * 1e7) as u64;
        let names: HashMap<u32, &str> = procs.iter().map(|p| (p.pid, p.name.as_str())).collect();
        let mut kids: HashMap<String, u32> = HashMap::new();
        for p in &procs {
            if p.pid > 4 && ft_now.saturating_sub(p.created) < horizon {
                *kids.entry(names.get(&p.ppid).map(|s| s.to_string()).unwrap_or_else(|| format!("pid{}", p.ppid))).or_insert(0) += 1;
            }
        }
        self.young = kids.into_iter().collect();
        top_n(&mut self.young, 5, |x| x.1 as f64);

        let mut by_h: Vec<&Proc> = procs.iter().collect();
        top_n(&mut by_h, 25, |p| p.handles as f64);
        let hs: Vec<(String, f64)> = by_h.iter().map(|p| (format!("ph:{}#{}", p.name, p.pid), p.handles as f64)).collect();
        let mut by_m: Vec<&Proc> = procs.iter().collect();
        top_n(&mut by_m, 25, |p| p.private as f64);
        let ms: Vec<(String, f64)> = by_m.iter().map(|p| (format!("pm:{}#{}", p.name, p.pid), mb(p.private))).collect();
        for (k, v) in hs.into_iter().chain(ms) { self.put(k, t, v); }

        let alerts = self.evaluate(&g, t);
        for (k, m) in alerts { self.alert(t, &k, &m); }
        self.log_cycle(t, &g);
        self.write_status(t, &g);
        if self.cycle % 30 == 0 {
            let c = self.cycle;
            self.series.retain(|_, s| c - s.last_seen < 120);
        }
        self.self_guard(t, own)
    }
}

impl Mon {
    fn last(&self, k: &str) -> f64 { self.series.get(k).and_then(|s| s.v.back().map(|x| x.1)).unwrap_or(0.0) }

    fn evaluate(&mut self, g: &Glob, t: f64) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let spawn = self.last("g:Proc_per_min");
        let young = self.young.iter().map(|(n, c)| format!("{}x{}", n, c)).collect::<Vec<_>>().join(", ");
        for (k, s) in self.series.iter_mut() {
            let (thr, unit, what) = if k.starts_with("tag:") { (25.0, "MB/h", "kernel pool tag") }
                else if k.starts_with("ph:") { (1500.0, "handles/h", "process handles") }
                else if k.starts_with("pm:") { (200.0, "MB/h", "process private memory") }
                else if k == "g:paged" || k == "g:nonpaged" { (100.0, "MB/h", "kernel pool") }
                else if k == "g:commit" { (1024.0, "MB/h", "committed memory") }
                else { continue };
            if t - s.last_alert < REALERT_SECS { continue; }
            if let Some((slope, up, n, cur)) = s.trend(60) {
                if slope >= thr && up >= 0.7 {
                    s.last_alert = t;
                    let mut m = format!("{} {} growing {:.0} {} (now {:.0}, {} samples, {:.0}% steps up).", what, k, slope, unit, cur, n, up * 100.0);
                    if let Some(tg) = k.strip_prefix("tag:") {
                        m.push(' '); m.push_str(tag_hint(tg));
                        if let Some(l) = self.tag_live.get(tg) { m.push_str(&format!(" Live allocations: {}.", l)); }
                    }
                    m.push_str(&format!(" Spawn rate {:.0} procs/min. Parents of newest processes: {}.", spawn, young));
                    out.push((k.clone(), m));
                }
            }
        }
        let due = |c: &HashMap<String, f64>, k: &str| t - c.get(k).copied().unwrap_or(0.0) >= REALERT_SECS;
        if g.avail_mb < 1024.0 && due(&self.cool, "avail") {
            self.cool.insert("avail".into(), t);
            out.push(("x:avail".into(), format!("only {:.0} MB of {:.0} MB RAM available. Restart the largest growing process listed in the log.", g.avail_mb, g.total_mb)));
        }
        let pct = if g.limit_mb > 0.0 { g.commit_mb / g.limit_mb * 100.0 } else { 0.0 };
        if pct >= 90.0 && due(&self.cool, "commit") {
            self.cool.insert("commit".into(), t);
            out.push(("x:commit".into(), format!("commit charge {:.0}% of limit ({:.0}/{:.0} MB). Apps will start failing allocations; grow the page file or restart the biggest process.", pct, g.commit_mb, g.limit_mb)));
        }
        let big: Vec<(String, f64)> = self.top_tags.iter().filter(|x| x.1 >= 1024.0).cloned().collect();
        for (n, v) in big {
            let key = format!("big:{}", n);
            if due(&self.cool, &key) {
                self.cool.insert(key.clone(), t);
                out.push((key, format!("kernel pool tag {} holds {:.0} MB. {}", n, v, tag_hint(&n))));
            }
        }
        out
    }

    fn log_cycle(&self, t: f64, g: &Glob) {
        let tags = self.top_tags.iter().map(|(n, v)| format!("[\"{}\",{:.1}]", esc(n), v)).collect::<Vec<_>>().join(",");
        let line = format!(
            "{{\"t\":\"{}\",\"commit_mb\":{:.0},\"avail_mb\":{:.0},\"paged_mb\":{:.0},\"nonpaged_mb\":{:.0},\"handles\":{:.0},\"procs\":{:.0},\"proc_per_min\":{:.1},\"toke_per_min\":{:.1},\"top_tags\":[{}]}}",
            iso(t), g.commit_mb, g.avail_mb, g.paged_mb, g.nonpaged_mb, g.handles, g.procs,
            self.last("g:Proc_per_min"), self.last("g:Toke_per_min"), tags);
        append_capped(&self.dir.join("snifrig.jsonl"), &line, JSONL_CAP);
    }

    /// Small status file the tray reads: level (ok/alert), free RAM, alerts in the last 2 h, newest alert.
    fn write_status(&mut self, t: f64, g: &Glob) {
        while self.alert_times.front().map_or(false, |&x| t - x > REALERT_SECS) { self.alert_times.pop_front(); }
        let level = if self.alert_times.is_empty() { "ok" } else { "alert" };
        let msg: String = self.last_alert_msg.chars().take(180).collect();
        let line = format!("{{\"unix\":{:.0},\"level\":\"{}\",\"avail_mb\":{:.0},\"commit_mb\":{:.0},\"alerts_2h\":{},\"last_alert_unix\":{:.0},\"last_alert\":\"{}\"}}",
            t, level, g.avail_mb, g.commit_mb, self.alert_times.len(), self.last_alert_t, esc(&msg));
        let _ = fs::write(self.dir.join("status.json"), line);
    }

    fn self_guard(&mut self, t: f64, own: Option<Proc>) -> bool {
        let o = match own { Some(o) => o, None => return true };
        if let Some((lt, lc)) = self.last_own {
            if t > lt {
                self.own_cpu.push_back(o.cpu.saturating_sub(lc) as f64 / 1e7 / (t - lt) * 100.0);
                if self.own_cpu.len() > 5 { self.own_cpu.pop_front(); }
            }
        }
        self.last_own = Some((t, o.cpu));
        if self.cycle == 3 { self.base = Some((o.private, o.handles)); self.base_threads = o.threads; }
        let mut why = Vec::new();
        if mb(o.ws) > 48.0 { why.push(format!("working set {:.1} MB over 48", mb(o.ws))); }
        if o.threads > 8 { why.push(format!("{} threads over 8", o.threads)); }
        if let Some((bp, bh)) = self.base {
            if o.threads > self.base_threads + 1 { why.push(format!("threads grew {} -> {}", self.base_threads, o.threads)); }
            if mb(o.private.saturating_sub(bp)) > 8.0 { why.push(format!("private bytes grew {:.1} MB over 8", mb(o.private.saturating_sub(bp)))); }
            if o.handles > bh + 8 { why.push(format!("handles grew {} over 8", o.handles - bh)); }
        }
        if self.interval >= 10.0 && self.own_cpu.len() >= 5 {
            let avg = self.own_cpu.iter().sum::<f64>() / self.own_cpu.len() as f64;
            if avg > 1.0 { why.push(format!("cpu {:.2}% over 1", avg)); }
        }
        if why.is_empty() { return true; }
        let m = format!("self-guard tripped, exiting: {}", why.join("; "));
        self.alert(t, "self", &m);
        false
    }
}

// ---------- license (UFL 3.4 Noncommercial: sections 9 and 10; local records only, no network) ----------
const LIC_VER: &str = "UFL-3.4";
static ALERTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn json_str(s: &str, key: &str) -> Option<String> {
    let pat = format!("\"{}\":\"", key);
    let i = s.find(&pat)? + pat.len();
    Some(s[i..].split('"').next()?.to_string())
}
pub fn json_num(s: &str, key: &str) -> u64 {
    let pat = format!("\"{}\":", key);
    s.find(&pat).map(|i| s[i + pat.len()..].chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0)).unwrap_or(0)
}

fn who() -> String {
    format!("{}@{}", std::env::var("USERNAME").unwrap_or_else(|_| "unknown".into()), std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".into()))
}

fn accepted(dir: &PathBuf) -> bool {
    fs::read_to_string(dir.join("license-accepted.json")).ok().and_then(|s| json_str(&s, "license")).as_deref() == Some(LIC_VER)
}

fn record_acceptance(dir: &PathBuf) {
    let line = format!("{{\"license\":\"{}\",\"scope\":\"Noncommercial\",\"accepted_at\":\"{}\",\"accepted_by\":\"{}\"}}", LIC_VER, iso(now()), esc(&who()));
    let _ = fs::write(dir.join("license-accepted.json"), line);
}

/// Section 9 step: an interactive prompt that names the version, or an explicit flag/env naming it.
fn ensure_accepted(dir: &PathBuf, given: Option<String>) -> bool {
    if accepted(dir) { return true; }
    let given = given.or_else(|| std::env::var("SNIFRIG_ACCEPT_LICENSE").ok());
    if given.as_deref() == Some(LIC_VER) { record_acceptance(dir); return true; }
    eprintln!("Snifrig is licensed under the Usufruct License {} (Operational Scope: Noncommercial).", LIC_VER);
    eprintln!("Free for home and other non-commercial use. Commercial use is Paid Use: see COMMERCIAL.md.");
    eprintln!("Type exactly `I accept {}` to continue, or run with --accept-license {}:", LIC_VER, LIC_VER);
    let mut line = String::new();
    let ok = std::io::stdin().read_line(&mut line).is_ok() && line.trim() == format!("I accept {}", LIC_VER);
    if ok { record_acceptance(dir); } else { eprintln!("Not accepted. Exiting."); }
    ok
}

fn bump_stats(dir: &PathBuf, cycles: u64) {
    let p = dir.join("stats.json");
    let old = fs::read_to_string(&p).unwrap_or_default();
    let first = json_str(&old, "first_run").unwrap_or_else(|| iso(now()));
    let line = format!("{{\"first_run\":\"{}\",\"last_run\":\"{}\",\"cycles\":{},\"alerts\":{}}}",
        first, iso(now()), json_num(&old, "cycles") + cycles, json_num(&old, "alerts") + ALERTS.swap(0, std::sync::atomic::Ordering::Relaxed));
    let _ = fs::write(&p, line);
}

/// Section 10: a statement built from local records. The signer fills in the use declaration.
fn statement(dir: &PathBuf) {
    let acc = fs::read_to_string(dir.join("license-accepted.json")).unwrap_or_default();
    let st = fs::read_to_string(dir.join("stats.json")).unwrap_or_default();
    println!("SNIFRIG USAGE STATEMENT (UFL 3.4 section 10)");
    println!("Software: Snifrig   License: {}   Operational Scope: Noncommercial", LIC_VER);
    println!("Period covered: {} to {}", json_str(&acc, "accepted_at").or_else(|| json_str(&st, "first_run")).unwrap_or_else(|| "(no local record)".into()), iso(now()));
    println!("Accepted by (local record): {}", json_str(&acc, "accepted_by").unwrap_or_else(|| "(none)".into()));
    println!("Computers measured here: 1 ({})", std::env::var("COMPUTERNAME").unwrap_or_else(|_| "this computer".into()));
    println!("Work processed (kept locally): {} sampling cycles, {} alerts", json_num(&st, "cycles"), json_num(&st, "alerts"));
    println!();
    println!("Made any use the Noncommercial scope withholds (commercial use) in this period?  [ ] No   [ ] Yes");
    println!("If yes, extent measured as the Published Price measures it (Computers): ______");
    println!();
    println!("Signed: ____________________   Name/authority: ____________________   Date: ____________");
    println!("This statement contains no content of processed data and no identity of any client or customer.");
    println!("Snifrig never sends this anywhere. Send it to the Licensor only on written request, at most once in 12 months.");
}

// ---------- one-shot report ----------
fn rate_h(s: &Series) -> f64 {
    match (s.v.front(), s.v.back()) {
        (Some(a), Some(b)) if b.0 > a.0 => (b.1 - a.1) / (b.0 - a.0) * 3600.0,
        _ => 0.0,
    }
}

fn report(m: &Mon) {
    let l = |k: &str| m.last(k);
    println!("== snifrig report {} ==", iso(now()));
    println!("RAM available {:.0} MB | commit {:.0} MB | paged pool {:.0} MB | nonpaged pool {:.0} MB", l("g:avail"), l("g:commit"), l("g:paged"), l("g:nonpaged"));
    println!("handles {:.0} | processes {:.0} | threads {:.0}", l("g:handles"), l("g:procs"), l("g:threads"));
    println!("spawn rate: {:.0} processes/min, {:.0} tokens/min (kernel allocation counters)", l("g:Proc_per_min"), l("g:Toke_per_min"));
    let mut rows: Vec<(&str, f64, f64)> = m.series.iter().filter(|(k, _)| k.starts_with("tag:")).map(|(k, s)| (&k[4..], s.v.back().map(|x| x.1).unwrap_or(0.0), rate_h(s))).collect();
    top_n(&mut rows, 10, |x| x.1);
    println!("\nTop kernel pool tags (MB now, MB/h over sample window):");
    for (n, v, r) in &rows { println!("  {:<5} {:>9.1} {:>+10.1}", n, v, r); }
    for (pre, title, unit) in [("ph:", "Top handle holders", "handles"), ("pm:", "Top private memory", "MB")] {
        let mut p: Vec<(&str, f64)> = m.series.iter().filter(|(k, _)| k.starts_with(pre)).map(|(k, s)| (&k[3..], s.v.back().map(|x| x.1).unwrap_or(0.0))).collect();
        top_n(&mut p, 10, |x| x.1);
        println!("\n{} ({}):", title, unit);
        for (n, v) in &p { println!("  {:<32} {:>10.0}", n, v); }
    }
    println!("\nFindings:");
    let mut found = 0;
    if l("g:avail") < 1024.0 { found += 1; println!("  - low RAM: {:.0} MB available. Restart the largest process above.", l("g:avail")); }
    for (n, v, r) in &rows {
        if *v >= 1024.0 { found += 1; println!("  - tag {} holds {:.0} MB. {}", n, v, tag_hint(n)); }
        else if *r >= 25.0 { found += 1; println!("  - tag {} growing about {:.0} MB/h during the sample (short window, confirm with a longer run). {}", n, r, tag_hint(n)); }
    }
    if l("g:Proc_per_min") > 120.0 { found += 1; println!("  - {:.0} processes/min is a heavy spawn rate; each spawn costs kernel token and process objects. Newest-process parents: {}", l("g:Proc_per_min"), m.young.iter().map(|(n, c)| format!("{}x{}", n, c)).collect::<Vec<_>>().join(", ")); }
    for (d, free, tot) in disks() { if tot > 0.0 && free / tot < 0.10 { found += 1; println!("  - disk {} has {:.0} of {:.0} GB free", d, free, tot); } }
    if found == 0 { println!("  none"); }
}

fn usage() {
    println!("snifrig - low-footprint Windows leak monitor (UFL 3.4 Noncommercial)\n");
    println!("  snifrig --once [--sample SECS]     print a report now (2 samples, default 10 s apart)");
    println!("  snifrig [--interval SECS]          watch loop (default 60), alerts to alerts.jsonl");
    println!("  snifrig install                    start at login (hidden monitor + tray icon), start now");
    println!("  snifrig uninstall                  remove login startup and stop the monitor");
    println!("  snifrig license accept             record acceptance of {}", LIC_VER);
    println!("  snifrig license statement          print a usage statement from local records");
    println!("options: --dir PATH  --cycles N  --accept-license {}  (or env SNIFRIG_ACCEPT_LICENSE)", LIC_VER);
}

pub fn default_dir() -> PathBuf {
    std::env::var("LOCALAPPDATA").map(|p| PathBuf::from(p).join("snifrig")).unwrap_or_else(|_| PathBuf::from("data"))
}

pub fn run() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let flag = |n: &str| a.iter().position(|x| x == n).and_then(|i| a.get(i + 1)).cloned();
    let has = |n: &str| a.iter().any(|x| x == n);
    if has("--help") || has("-h") || a.first().map(|s| s == "help").unwrap_or(false) { usage(); return; }
    let dir = flag("--dir").map(PathBuf::from).unwrap_or_else(default_dir);
    let _ = fs::create_dir_all(&dir);
    let given = flag("--accept-license");
    if a.first().map(|s| s.as_str()) == Some("license") {
        match a.get(1).map(|s| s.as_str()) {
            Some("accept") => { if ensure_accepted(&dir, given) { println!("Recorded acceptance of {}.", LIC_VER); } else { std::process::exit(1); } }
            Some("statement") => statement(&dir),
            _ => usage(),
        }
        return;
    }
    match a.first().map(|s| s.as_str()) {
        Some("install") => { install(&dir, given); return; }
        Some("uninstall") => { uninstall(&dir); return; }
        _ => {}
    }
    if !ensure_accepted(&dir, given) { std::process::exit(1); }

    unsafe {
        SetPriorityClass(GetCurrentProcess(), IDLE_PRIORITY_CLASS);
        SetPriorityClass(GetCurrentProcess(), 0x0010_0000); // PROCESS_MODE_BACKGROUND_BEGIN: lowers IO and memory priority too
    }
    if has("--once") {
        let secs: f64 = flag("--sample").and_then(|s| s.parse().ok()).unwrap_or(10.0f64).max(1.0);
        let mut m = Mon::new(dir.clone(), secs, true);
        m.step();
        std::thread::sleep(Duration::from_secs_f64(secs));
        m.step();
        report(&m);
        bump_stats(&dir, 2);
        return;
    }

    let name: Vec<u16> = "Global\\snifrig-monitor".encode_utf16().chain(Some(0)).collect();
    let _mutex = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    if unsafe { GetLastError() } == 183 { eprintln!("snifrig is already running."); std::process::exit(1); }

    let _ = fs::remove_file(dir.join("stop")); // a stale stop request must not kill a fresh start
    let interval: f64 = flag("--interval").and_then(|s| s.parse().ok()).unwrap_or(60.0f64).max(1.0);
    let max_cycles: u64 = flag("--cycles").and_then(|s| s.parse().ok()).unwrap_or(u64::MAX);
    let mut m = Mon::new(dir.clone(), interval, false);
    let start = Instant::now();
    let mut k: u64 = 0;
    let mut ok = true;
    while k < max_cycles {
        ok = m.step();
        k += 1;
        if k % 30 == 0 { bump_stats(&dir, 30); }
        if !ok { break; }
        let next = start + Duration::from_secs_f64(interval * k as f64);
        let stop = dir.join("stop");
        while Instant::now() < next && !stop.exists() {
            std::thread::sleep(Duration::from_millis(500).min(next.saturating_duration_since(Instant::now())));
        }
        if stop.exists() { let _ = fs::remove_file(&stop); break; }
    }
    bump_stats(&dir, k % 30);
    if !ok { std::process::exit(2); }
}

// ---------- install / uninstall (Windows: per-user login autostart, no admin, no service) ----------
use std::os::windows::process::CommandExt;
use windows_sys::Win32::System::Registry::{RegDeleteKeyValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ};

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
fn wide(s: &str) -> Vec<u16> { s.encode_utf16().chain(Some(0)).collect() }

fn set_run(name: &str, cmd: &str) -> bool {
    let (k, n, v) = (wide(RUN_KEY), wide(name), wide(cmd));
    unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr(), REG_SZ, v.as_ptr() as *const _, (v.len() * 2) as u32) == 0 }
}

fn del_run(name: &str) {
    let (k, n) = (wide(RUN_KEY), wide(name));
    unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr()) };
}

/// Start a program with no console window, detached from this one (DETACHED_PROCESS | CREATE_NO_WINDOW).
fn spawn_detached(exe: &PathBuf, dir: &PathBuf) {
    use std::process::Stdio;
    // null stdio so the child never holds the caller's output pipe open
    let _ = std::process::Command::new(exe).arg("--dir").arg(dir).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .creation_flags(0x0000_0008 | 0x0800_0000).spawn();
}

fn monitor_running(dir: &PathBuf) -> bool {
    let s = fs::read_to_string(dir.join("status.json")).unwrap_or_default();
    let u = json_num(&s, "unix") as f64;
    u > 0.0 && now() - u < 180.0
}

fn install(dir: &PathBuf, given: Option<String>) {
    if !ensure_accepted(dir, given) { std::process::exit(1); }
    let here = match std::env::current_exe() { Ok(p) => p, Err(_) => { eprintln!("cannot find own location"); std::process::exit(1) } };
    let sd = here.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let bin = dir.join("bin");
    let _ = fs::create_dir_all(&bin);
    if monitor_running(dir) {
        let _ = fs::write(dir.join("stop"), "1"); // ask the running monitor to exit
        for _ in 0..40 { if !dir.join("stop").exists() { break; } std::thread::sleep(Duration::from_millis(500)); }
        let _ = fs::remove_file(dir.join("stop"));
    }
    let _ = fs::write(dir.join("stop-tray"), "1"); // and the tray, so its exe can be replaced
    std::thread::sleep(Duration::from_secs(4));
    let _ = fs::remove_file(dir.join("stop-tray"));
    let mut failed = false;
    for n in ["snifrig.exe", "snifrigd.exe", "snifrig-tray.exe"] {
        let (from, to) = (sd.join(n), bin.join(n));
        if from == to { continue; }
        if let Err(e) = fs::copy(&from, &to) { eprintln!("could not copy {}: {}", n, e); failed = true; }
    }
    if failed { eprintln!("Install stopped. Build all three programs first (cargo build --release) and run install from that folder."); std::process::exit(1); }
    let q = |n: &str| format!("\"{}\" --dir \"{}\"", bin.join(n).display(), dir.display());
    if !(set_run("Snifrig", &q("snifrigd.exe")) && set_run("SnifrigTray", &q("snifrig-tray.exe"))) {
        eprintln!("Could not write the login startup entries."); std::process::exit(1);
    }
    spawn_detached(&bin.join("snifrigd.exe"), dir);
    spawn_detached(&bin.join("snifrig-tray.exe"), dir);
    println!("Installed. Snifrig now starts at login (hidden monitor + tray icon) and is running.");
    println!("Programs: {}", bin.display());
    println!("Data and alerts: {}", dir.display());
    println!("Remove with: snifrig uninstall");
}

fn uninstall(dir: &PathBuf) {
    del_run("Snifrig");
    del_run("SnifrigTray");
    let _ = fs::write(dir.join("stop"), "1");
    let _ = fs::write(dir.join("stop-tray"), "1");
    println!("Login startup removed. The monitor and tray exit within a minute.");
    println!("Your data stays in {} (delete that folder to remove it).", dir.display());
}
