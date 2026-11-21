//! The rest of the PC, so nothing can take it over unseen: the network link, the graphics card's
//! scheduler, thread leaks, what starts at login, and the page file's size.
//!
//! - Network: throughput of each physical adapter against its link speed and against the
//!   fastest this PC has ever been seen to move (that is the internet line's real limit), plus
//!   TCP connections per program (no admin): a program opening thousands of connections, or the
//!   PC running out of ports (about 16,000 per address), breaks networking for every app.
//! - GPU fairness: when the app in front is using the graphics card and a background program
//!   holds most of it, that program's GPU scheduling priority is lowered (like CPU priority, it
//!   still runs, just after the app in front), and restored when the app in front changes.
//! - Thread leaks: a program past 3,000 threads and still adding them.
//! - Startup: what Windows starts at login (Run keys, Startup folders) and what it cost in the
//!   first minutes after this boot.
//! - Page file: a fixed page file smaller than RAM, or none, caps how much memory programs may
//!   commit; the PC then hits "out of memory" with RAM to spare.
//!
//! Judging is pure and tested; probes are Windows-only.

use std::collections::HashMap;

// ---------- network ----------

/// One adapter sample: name, bytes in, bytes out, link speed in and out (bits/s).
#[derive(Clone, Debug, PartialEq)]
pub struct If { pub name: String, pub rx: u64, pub tx: u64, pub rx_bps: u64, pub tx_bps: u64 }

#[derive(Default)]
pub struct Net {
    last: HashMap<String, (f64, u64, u64)>,
    /// Consecutive full samples per (adapter, direction).
    streak: HashMap<(String, bool), u32>,
    /// Fastest rate seen per (adapter, direction), bits/s: the real line limit, learned.
    pub peak: HashMap<(String, bool), f64>,
    /// At the last tick some adapter was at half its limit or more (worth asking who is using it).
    pub busy: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NetFull { pub name: String, pub download: bool, pub mbps: f64, pub limit_mbps: f64 }

impl Net {
    /// Call every 10 s. Full = 85% of link speed, or 90% of the fastest ever seen (at least
    /// 10 Mbit/s), for 2 minutes in a row.
    pub fn tick(&mut self, now: f64, ifs: &[If]) -> Vec<NetFull> {
        let mut out = Vec::new();
        self.busy = false;
        for i in ifs {
            let prev = self.last.insert(i.name.clone(), (now, i.rx, i.tx));
            let Some((t0, rx0, tx0)) = prev else { continue };
            let dt = now - t0;
            if dt < 1.0 || i.rx < rx0 || i.tx < tx0 { continue; }
            for (down, bytes, link) in [(true, i.rx - rx0, i.rx_bps), (false, i.tx - tx0, i.tx_bps)] {
                let bps = bytes as f64 * 8.0 / dt;
                let key = (i.name.clone(), down);
                let peak = self.peak.entry(key.clone()).or_insert(0.0);
                let full = (link > 0 && bps >= link as f64 * 0.85) || (*peak >= 10e6 && bps >= *peak * 0.9);
                if (link > 0 && bps >= link as f64 * 0.5) || (*peak >= 10e6 && bps >= *peak * 0.6) { self.busy = true; }
                if bps > *peak { *peak = bps; }
                let s = self.streak.entry(key).or_insert(0);
                *s = if full { *s + 1 } else { 0 };
                if *s == 12 {
                    let limit = if link > 0 { (link as f64).min(self.peak[&(i.name.clone(), down)].max(bps)) } else { bps };
                    out.push(NetFull { name: i.name.clone(), download: down, mbps: bps / 1e6, limit_mbps: limit / 1e6 });
                }
            }
        }
        out
    }
}

/// (pid, state, remote is loopback) per TCP connection; state 11 = TIME_WAIT, 5 = ESTABLISHED.
pub type Conn = (u32, u32, bool);

#[derive(Clone, Debug, PartialEq)]
pub enum ConnStep {
    /// One program holds this many connections (not counting loopback).
    Flood { pid: u32, count: usize },
    /// The PC is close to running out of ports: total connections, TIME_WAIT among them, top owners.
    Ports { total: usize, time_wait: usize, top: Vec<(u32, usize)> },
}

pub fn judge_conns(conns: &[Conn]) -> Vec<ConnStep> {
    let mut per: HashMap<u32, usize> = HashMap::new();
    let mut tw = 0;
    for &(pid, state, lo) in conns {
        if state == 11 { tw += 1; }
        if pid != 0 && !lo { *per.entry(pid).or_default() += 1; }
    }
    let mut top: Vec<(u32, usize)> = per.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1));
    let mut out: Vec<ConnStep> = top.iter().filter(|t| t.1 >= 3000).map(|&(pid, count)| ConnStep::Flood { pid, count }).collect();
    if conns.len() >= 12000 { out.push(ConnStep::Ports { total: conns.len(), time_wait: tw, top: top.into_iter().take(3).collect() }); }
    out
}

// ---------- GPU fairness ----------

/// GPU use per pid (percent of the busiest engine type, 3D or compute), from one sample.
pub type GpuUse = HashMap<u32, f64>;

/// Sum of "pid_N_..._engtype_3D" / "engtype_Compute*" instance values per pid.
pub fn gpu_by_pid(items: &[(String, f64)]) -> GpuUse {
    let mut m: GpuUse = HashMap::new();
    for (name, v) in items {
        let n = name.to_lowercase();
        if !(n.contains("engtype_3d") || n.contains("engtype_compute") || n.contains("engtype_cuda")) { continue; }
        let Some(pid) = n.strip_prefix("pid_").and_then(|r| r.split('_').next()).and_then(|p| p.parse::<u32>().ok()) else { continue };
        let e = m.entry(pid).or_default();
        *e = (*e + v).min(100.0);
    }
    m
}

#[derive(Default)]
pub struct GpuFair { streak: HashMap<u32, u32>, pub lowered: Vec<(u32, String)>, fg_idle: u32 }

#[derive(Clone, Debug, PartialEq)]
pub enum GpuStep { Lower { pid: u32, pct: f64 }, Restore { pids: Vec<u32> } }

impl GpuFair {
    /// Call every 5 s. `names`: pid -> image name (to skip protected and system programs).
    /// Lower: the app in front uses the GPU (10%+), the card is busy (85%+ total), and a background
    /// program has held 40%+ for 15 s. Restore: the app in front stopped using the GPU for a minute,
    /// or you switched to another app that does not use it.
    pub fn tick(&mut self, use_: &GpuUse, fg: Option<u32>, skip: &dyn Fn(u32) -> bool) -> Vec<GpuStep> {
        let mut out = Vec::new();
        let total: f64 = use_.values().sum::<f64>().min(100.0);
        let fg_use = fg.and_then(|p| use_.get(&p)).copied().unwrap_or(0.0);
        if fg_use < 5.0 { self.fg_idle += 1; } else { self.fg_idle = 0; }
        if !self.lowered.is_empty() && self.fg_idle >= 12 {
            out.push(GpuStep::Restore { pids: self.lowered.iter().map(|l| l.0).collect() });
            self.lowered.clear();
        }
        let busy = fg_use >= 10.0 && total >= 85.0;
        let keys: Vec<u32> = self.streak.keys().copied().collect();
        for p in keys { if !busy || use_.get(&p).copied().unwrap_or(0.0) < 40.0 { self.streak.remove(&p); } }
        if !busy { return out; }
        for (&pid, &u) in use_ {
            if Some(pid) == fg || u < 40.0 || skip(pid) || self.lowered.iter().any(|l| l.0 == pid) { continue; }
            let s = self.streak.entry(pid).or_insert(0);
            *s += 1;
            if *s >= 3 && self.lowered.len() < 3 { out.push(GpuStep::Lower { pid, pct: u }); }
        }
        out
    }
}

// ---------- thread leaks ----------

#[derive(Default)]
pub struct Threads { hist: HashMap<(u32, u64), Vec<(f64, u32)>>, said: std::collections::HashSet<(u32, u64)> }

impl Threads {
    /// Call every minute with (pid, created, threads). Returns (pid, created, threads now, added
    /// in the last 30 min) for programs past 3,000 threads that added 1,000+ in 30 minutes. Once per process.
    pub fn tick(&mut self, now: f64, procs: &[(u32, u64, u32)]) -> Vec<(u32, u64, u32, u32)> {
        let alive: std::collections::HashSet<(u32, u64)> = procs.iter().map(|p| (p.0, p.1)).collect();
        self.hist.retain(|k, _| alive.contains(k));
        let mut out = Vec::new();
        for &(pid, created, n) in procs {
            if n < 500 { continue; }
            let h = self.hist.entry((pid, created)).or_default();
            h.push((now, n));
            h.retain(|&(t, _)| now - t <= 1800.0);
            let first = h[0].1;
            if n >= 3000 && n >= first + 1000 && self.said.insert((pid, created)) { out.push((pid, created, n, n - first)); }
        }
        out
    }
}

// ---------- startup ----------

#[derive(Clone, Debug, PartialEq)]
pub struct StartItem { pub name: String, pub command: String, pub place: String, pub enabled: bool }

/// The program file a startup command runs ("C:\x\app.exe" -> "app.exe").
pub fn exe_of(cmd: &str) -> String {
    let c = cmd.trim();
    let path = if let Some(r) = c.strip_prefix('"') { r.split('"').next().unwrap_or("") } else {
        let l = c.to_lowercase();
        match l.find(".exe") { Some(i) => &c[..i + 4], None => c.split_whitespace().next().unwrap_or("") }
    };
    path.rsplit(['\\', '/']).next().unwrap_or("").to_lowercase()
}

/// Heavy startup items: CPU seconds and disk MB used in the first minutes after boot by the
/// processes each item started (matched by program file). Over 20 CPU-seconds or 300 MB.
pub fn heavy_startup(items: &[StartItem], cost: &HashMap<String, (f64, f64)>) -> Vec<(String, f64, f64)> {
    let mut out: Vec<(String, f64, f64)> = items.iter().filter(|i| i.enabled).filter_map(|i| {
        let (cpu, mb) = *cost.get(&exe_of(&i.command))?;
        (cpu >= 20.0 || mb >= 300.0).then(|| (i.name.clone(), cpu, mb))
    }).collect();
    out.sort_by(|a, b| (b.1 + b.2 / 30.0).partial_cmp(&(a.1 + a.2 / 30.0)).unwrap_or(std::cmp::Ordering::Equal));
    out.dedup_by(|a, b| a.0 == b.0);
    out
}

// ---------- field report ----------

/// Windows builds snifrig's checks are already tested on (major.minor.build).
pub const TESTED_BUILDS: [&str; 1] = ["10.0.26200"];

/// Why this PC's field report would teach us something; empty = it would not, so the honest
/// advice is not to send it. `drives`: (on USB, how it was read, health read). `checks`: (name, works).
pub fn report_why(os: &str, drives: &[(bool, String, bool)], checks: &[(&str, bool)]) -> Vec<String> {
    let mut why = Vec::new();
    let build = os.split("Version ").nth(1).map(|v| v.trim_end_matches(']').split('.').take(3).collect::<Vec<_>>().join(".")).unwrap_or_default();
    if !build.is_empty() && !TESTED_BUILDS.contains(&build.as_str()) { why.push(format!("Windows build {} is not tested yet", build)); }
    for (usb, probe, ok) in drives {
        if probe.starts_with("ata") && *ok { why.push("it has a SATA drive snifrig read; real numbers check that reading (tested only on made-up data so far)".into()); }
        else if !ok && !usb { why.push(format!("a drive snifrig could not read ({})", probe)); }
        else if *usb { why.push("a USB drive: tells us which enclosures hide drive health".into()); }
    }
    for (name, works) in checks { if !works { why.push(format!("the {} check does not work here", name)); } }
    why.sort();
    why.dedup();
    why
}

// ---------- page file ----------

/// Judge the PagingFiles registry entries against RAM. "?:\pagefile.sys" is fully automatic;
/// "C:\pagefile.sys 0 0" is system-managed on C:; "C:\pagefile.sys 4096 8192" is fixed (MB).
pub fn pagefile_problem(entries: &[String], ram_mb: f64) -> Option<String> {
    let real: Vec<&String> = entries.iter().filter(|e| !e.trim().is_empty()).collect();
    if real.is_empty() {
        return Some(format!("This PC has no page file. With {:.0} GB of RAM, programs can only commit that much memory in all; past it they crash with \"out of memory\" even when the PC is not busy. Turn on \"Automatically manage paging file size\" (System > About > Advanced system settings > Performance > Advanced > Virtual memory).", ram_mb / 1024.0));
    }
    let mut fixed_max = 0.0;
    for e in &real {
        let f: Vec<&str> = e.split_whitespace().collect();
        if f.first().map_or(false, |p| p.starts_with("?:")) { return None; }
        match (f.get(1).and_then(|x| x.parse::<f64>().ok()), f.get(2).and_then(|x| x.parse::<f64>().ok())) {
            (Some(a), Some(b)) if a == 0.0 && b == 0.0 => return None,
            (_, Some(b)) => fixed_max += b,
            _ => return None,
        }
    }
    (fixed_max < ram_mb).then(|| format!("The page file is set to a fixed size of at most {:.0} GB, less than this PC's {:.0} GB of RAM. That caps how much memory programs may commit (RAM plus page file), so big jobs fail or the PC freezes before RAM is used up. Let Windows manage it (System > About > Advanced system settings > Performance > Advanced > Virtual memory > Automatically manage).", fixed_max / 1024.0, ram_mb / 1024.0))
}

#[cfg(windows)]
pub use imp::*;

#[cfg(windows)]
mod imp {
    use super::*;
    use std::ffi::c_void;

    #[link(name = "iphlpapi", kind = "raw-dylib")]
    extern "system" {
        fn GetExtendedTcpTable(table: *mut c_void, size: *mut u32, order: i32, af: u32, class: i32, reserved: u32) -> u32;
    }
    #[link(name = "gdi32", kind = "raw-dylib")]
    extern "system" {
        fn D3DKMTSetProcessSchedulingPriorityClass(h: windows_sys::Win32::Foundation::HANDLE, class: i32) -> i32;
    }

    fn rd32(b: &[u8], o: usize) -> u32 { u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) }

    /// Every TCP connection (IPv4 and IPv6) with its owner pid; no admin needed.
    pub fn conns() -> Vec<Conn> {
        let mut out = Vec::new();
        for (af, row, state_at, pid_at, remote_lo) in [(2u32, 24usize, 0usize, 20usize, 0u8), (23u32, 56usize, 48usize, 52usize, 1u8)] {
            let mut size = 0u32;
            unsafe { GetExtendedTcpTable(std::ptr::null_mut(), &mut size, 0, af, 5, 0); }
            if size == 0 { continue; }
            let mut buf = vec![0u8; size as usize + 4096];
            let mut size = buf.len() as u32;
            if unsafe { GetExtendedTcpTable(buf.as_mut_ptr() as *mut c_void, &mut size, 0, af, 5, 0) } != 0 { continue; }
            let n = rd32(&buf, 0) as usize;
            for k in 0..n {
                let o = 4 + k * row;
                if o + row > buf.len() { break; }
                let lo = if remote_lo == 0 { buf[o + 12] == 127 } else { buf[o + 24..o + 39].iter().all(|&x| x == 0) && buf[o + 39] == 1 };
                out.push((rd32(&buf, o + pid_at), rd32(&buf, o + state_at), lo));
            }
        }
        out
    }

    /// Physical adapters that are up: Ethernet (6) and Wi-Fi (71).
    pub fn adapters() -> Vec<If> {
        use windows_sys::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_TABLE2};
        let mut t: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        if unsafe { GetIfTable2(&mut t) } != 0 || t.is_null() { return Vec::new(); }
        let mut out = Vec::new();
        unsafe {
            let n = (*t).NumEntries as usize;
            let rows = std::slice::from_raw_parts((*t).Table.as_ptr(), n);
            for r in rows {
                let hw = r.InterfaceAndOperStatusFlags._bitfield & 1 == 1;
                if !hw || r.OperStatus != 1 || !(r.Type == 6 || r.Type == 71) { continue; }
                let l = r.Alias.iter().position(|&c| c == 0).unwrap_or(r.Alias.len());
                out.push(If { name: String::from_utf16_lossy(&r.Alias[..l]), rx: r.InOctets, tx: r.OutOctets, rx_bps: r.ReceiveLinkSpeed, tx_bps: r.TransmitLinkSpeed });
            }
            FreeMibTable(t as *const c_void);
        }
        out
    }

    /// Set a process's GPU scheduling priority class (0 idle, 1 below normal, 2 normal).
    pub fn set_gpu_priority(pid: u32, class: i32) -> Result<(), String> {
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION};
        let h = unsafe { OpenProcess(PROCESS_SET_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if h.is_null() { return Err(format!("cannot open pid {}", pid)); }
        let st = unsafe { D3DKMTSetProcessSchedulingPriorityClass(h, class) };
        unsafe { windows_sys::Win32::Foundation::CloseHandle(h); }
        if st < 0 { Err(format!("D3DKMTSetProcessSchedulingPriorityClass {:#x}", st)) } else { Ok(()) }
    }

    /// Per-process GPU engine use via PDH ("GPU Engine(*)\Utilization Percentage", no admin).
    pub struct GpuPdh { q: isize, c: isize, buf: Vec<u64> }

    impl GpuPdh {
        pub fn open() -> Option<GpuPdh> {
            use windows_sys::Win32::System::Performance::*;
            unsafe {
                let mut q: isize = 0;
                if PdhOpenQueryW(std::ptr::null(), 0, &mut q) != 0 { return None; }
                let w: Vec<u16> = "\\GPU Engine(*)\\Utilization Percentage".encode_utf16().chain(Some(0)).collect();
                let mut c: isize = 0;
                if PdhAddEnglishCounterW(q, w.as_ptr(), 0, &mut c) != 0 { PdhCloseQuery(q); return None; }
                PdhCollectQueryData(q);
                Some(GpuPdh { q, c, buf: Vec::new() })
            }
        }
        pub fn sample(&mut self) -> GpuUse {
            use windows_sys::Win32::System::Performance::*;
            let mut items = Vec::new();
            unsafe {
                if PdhCollectQueryData(self.q) != 0 { return GpuUse::new(); }
                let (mut sz, mut n) = (0u32, 0u32);
                let fmt = PDH_FMT_DOUBLE | 0x8000;
                let r = PdhGetFormattedCounterArrayW(self.c, fmt, &mut sz, &mut n, std::ptr::null_mut());
                if r != 0x8000_07D2 || sz == 0 { return GpuUse::new(); }
                self.buf.resize(sz as usize / 8 + 8, 0);
                let mut sz2 = (self.buf.len() * 8) as u32;
                let p = self.buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
                if PdhGetFormattedCounterArrayW(self.c, fmt, &mut sz2, &mut n, p) != 0 { return GpuUse::new(); }
                for k in 0..n as usize {
                    let it = &*p.add(k);
                    if it.FmtValue.CStatus > 1 { continue; }
                    let mut l = 0;
                    while *it.szName.add(l) != 0 { l += 1; }
                    items.push((String::from_utf16_lossy(std::slice::from_raw_parts(it.szName, l)), it.FmtValue.Anonymous.doubleValue));
                }
            }
            gpu_by_pid(&items)
        }
    }
    impl Drop for GpuPdh { fn drop(&mut self) { unsafe { windows_sys::Win32::System::Performance::PdhCloseQuery(self.q); } } }

    fn reg_values(key: &str) -> Vec<(String, String)> {
        use std::os::windows::process::CommandExt;
        let o = std::process::Command::new("reg").args(["query", key]).stdin(std::process::Stdio::null()).creation_flags(0x0800_0000).output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
        o.lines().filter_map(|l| {
            let l = l.trim_start();
            let (name, rest) = l.split_once("    ")?;
            let rest = rest.trim_start();
            let (ty, val) = rest.split_once("    ")?;
            ty.starts_with("REG_").then(|| (name.trim().to_string(), val.trim().to_string()))
        }).collect()
    }

    /// What starts at login: Run keys (user and machine) and the Startup folders, with the
    /// on/off state Task Manager keeps in StartupApproved.
    pub fn startup_items() -> Vec<StartItem> {
        let mut out = Vec::new();
        let approved = |key: &str| -> HashMap<String, bool> {
            reg_values(key).into_iter().map(|(n, v)| (n.to_lowercase(), !v.starts_with("03"))).collect()
        };
        let runs = [
            (r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run", "your Run key"),
            (r"HKLM\Software\Microsoft\Windows\CurrentVersion\Run", r"HKLM\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run", "the machine Run key"),
            (r"HKLM\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Run", r"HKLM\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run32", "the machine Run key (32-bit)"),
        ];
        for (k, ak, place) in runs {
            let ok = approved(ak);
            for (n, v) in reg_values(k) {
                let enabled = ok.get(&n.to_lowercase()).copied().unwrap_or(true);
                out.push(StartItem { name: n, command: v, place: place.into(), enabled });
            }
        }
        let folders = [
            (std::env::var("APPDATA").unwrap_or_default() + r"\Microsoft\Windows\Start Menu\Programs\Startup", "your Startup folder"),
            (std::env::var("ProgramData").unwrap_or_default() + r"\Microsoft\Windows\Start Menu\Programs\StartUp", "the shared Startup folder"),
        ];
        let ok = approved(r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\StartupFolder");
        for (f, place) in folders {
            for e in std::fs::read_dir(&f).into_iter().flatten().flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.eq_ignore_ascii_case("desktop.ini") { continue; }
                let stem = name.rsplit_once('.').map(|x| x.0.to_string()).unwrap_or(name.clone());
                let enabled = ok.get(&name.to_lowercase()).copied().unwrap_or(true);
                // A shortcut: the program it points to is stored in it as plain text.
                let command = if name.to_lowercase().ends_with(".lnk") { lnk_target(&e.path()).unwrap_or(name.clone()) } else { name.clone() };
                out.push(StartItem { name: stem, command, place: place.into(), enabled });
            }
        }
        out
    }

    /// The .exe a shortcut (.lnk) points to, read from the local path it stores in plain text.
    fn lnk_target(p: &std::path::Path) -> Option<String> {
        let b = std::fs::read(p).ok()?;
        let i = b.windows(4).position(|w| w.eq_ignore_ascii_case(b".exe"))?;
        let mut s = i;
        while s > 0 && (b[s - 1].is_ascii_graphic() || b[s - 1] == b' ') && !matches!(b[s - 1], b'\\' | b'/' | b':') { s -= 1; }
        (s < i).then(|| String::from_utf8_lossy(&b[s..i + 4]).to_string())
    }

    /// Total physical memory in MB.
    pub fn ram_mb() -> f64 {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if unsafe { GlobalMemoryStatusEx(&mut m) } == 0 { return 0.0; }
        m.ullTotalPhys as f64 / 1048576.0
    }

    /// Seconds since boot.
    pub fn uptime() -> f64 { unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() as f64 / 1000.0 } }

    #[cfg(test)]
    mod tests {
        #[test]
        fn reads_this_pcs_network_gpu_and_startup() {
            let c = super::conns();
            assert!(!c.is_empty(), "no TCP connections at all?");
            println!("{} tcp connections, adapters {:?}", c.len(), super::adapters());
            if let Some(mut g) = super::GpuPdh::open() { std::thread::sleep(std::time::Duration::from_millis(500)); println!("gpu {:?}", g.sample()); }
            let s = super::startup_items();
            println!("startup: {:?}", s.iter().map(|i| format!("{} [{}] {}", i.name, super::super::exe_of(&i.command), i.enabled)).collect::<Vec<_>>());
            assert!(super::ram_mb() > 1000.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_full_for_two_minutes() {
        let mut n = Net::default();
        let gb = 125_000_000u64; // 1 Gbit/s in bytes per second
        let mut hit = Vec::new();
        for k in 0..=13u64 { hit = n.tick(k as f64 * 10.0, &[If { name: "Ethernet".into(), rx: k * gb * 10 * 9 / 10, tx: 0, rx_bps: 1_000_000_000, tx_bps: 1_000_000_000 }]); if !hit.is_empty() { break; } }
        assert_eq!(hit.len(), 1);
        assert!(hit[0].download && (hit[0].mbps - 900.0).abs() < 1.0);
    }

    #[test]
    fn learned_peak_catches_a_full_internet_line_on_a_fast_link() {
        // 1 Gbit link, 100 Mbit internet: link-speed rule never fires, the learned peak does.
        let mut n = Net::default();
        let mut rx = 0u64;
        let mut hits = 0;
        for k in 0..40u64 {
            rx += 12_500_000 * 10; // 100 Mbit/s
            hits += n.tick(k as f64 * 10.0, &[If { name: "Ethernet".into(), rx, tx: 0, rx_bps: 1_000_000_000, tx_bps: 1_000_000_000 }]).len();
        }
        assert_eq!(hits, 1, "once per streak");
    }

    #[test]
    fn connection_floods_and_port_exhaustion() {
        let mut c: Vec<Conn> = (0..3500).map(|_| (42, 5, false)).collect();
        c.extend((0..9000).map(|_| (0, 11, false)));
        c.extend((0..100).map(|_| (7, 5, true)));
        let s = judge_conns(&c);
        assert!(s.contains(&ConnStep::Flood { pid: 42, count: 3500 }));
        assert!(s.iter().any(|x| matches!(x, ConnStep::Ports { time_wait: 9000, .. })));
        assert!(judge_conns(&c[..200]).is_empty());
    }

    #[test]
    fn gpu_parse_and_fairness() {
        let items = vec![
            ("pid_100_luid_0x0_0x1_phys_0_eng_0_engtype_3D".to_string(), 30.0),
            ("pid_200_luid_0x0_0x1_phys_0_eng_1_engtype_Compute_0".to_string(), 60.0),
            ("pid_200_luid_0x0_0x1_phys_0_eng_2_engtype_Copy".to_string(), 90.0),
        ];
        let u = gpu_by_pid(&items);
        assert_eq!(u[&100], 30.0);
        assert_eq!(u[&200], 60.0, "copy engine is not counted");
        let mut f = GpuFair::default();
        let no = |_p: u32| false;
        assert!(f.tick(&u, Some(100), &no).is_empty());
        assert!(f.tick(&u, Some(100), &no).is_empty());
        assert_eq!(f.tick(&u, Some(100), &no), vec![GpuStep::Lower { pid: 200, pct: 60.0 }]);
        f.lowered.push((200, "trainer.exe".into()));
        // The app in front stops using the GPU for a minute: restored.
        let idle: GpuUse = [(200u32, 90.0)].into_iter().collect();
        let mut r = Vec::new();
        for _ in 0..12 { r = f.tick(&idle, Some(100), &no); if !r.is_empty() { break; } }
        assert_eq!(r, vec![GpuStep::Restore { pids: vec![200] }]);
        // Never the app in front, never a skipped one.
        let mut f = GpuFair::default();
        let skip = |p: u32| p == 200;
        for _ in 0..5 { assert!(f.tick(&u, Some(100), &skip).is_empty()); }
    }

    #[test]
    fn thread_leak_once() {
        let mut t = Threads::default();
        assert!(t.tick(0.0, &[(5, 1, 2500)]).is_empty());
        assert!(t.tick(600.0, &[(5, 1, 3000)]).is_empty(), "500 more is not a leak yet");
        let r = t.tick(1200.0, &[(5, 1, 3600)]);
        assert_eq!(r, vec![(5, 1, 3600, 1100)]);
        assert!(t.tick(1260.0, &[(5, 1, 3700)]).is_empty(), "said once");
    }

    #[test]
    fn startup_exe_and_heavy() {
        assert_eq!(exe_of(r#""C:\Program Files\Razer\RazerCortex.exe" --silent"#), "razercortex.exe");
        assert_eq!(exe_of(r"C:\Tools\sync.exe -min"), "sync.exe");
        let items = vec![
            StartItem { name: "Cortex".into(), command: r#""C:\R\RazerCortex.exe""#.into(), place: "x".into(), enabled: true },
            StartItem { name: "Off".into(), command: "off.exe".into(), place: "x".into(), enabled: false },
            StartItem { name: "Light".into(), command: "light.exe".into(), place: "x".into(), enabled: true },
        ];
        let cost: HashMap<String, (f64, f64)> = [("razercortex.exe".to_string(), (45.0, 120.0)), ("off.exe".to_string(), (99.0, 999.0)), ("light.exe".to_string(), (2.0, 10.0))].into_iter().collect();
        assert_eq!(heavy_startup(&items, &cost), vec![("Cortex".to_string(), 45.0, 120.0)]);
    }

    #[test]
    fn field_report_is_only_worth_sending_when_it_teaches_something() {
        let ok = [("GPU", true), ("network", true)];
        let nvme = vec![(false, "nvme".to_string(), true)];
        // beastly: tested build, NVMe only, everything works -> nothing to learn.
        assert!(report_why("Microsoft Windows [Version 10.0.26200.9818]", &nvme, &ok).is_empty());
        let w = report_why("Microsoft Windows [Version 10.0.19045.5131]", &[(false, "ata (bus 11)".into(), true), (false, "ata (bus 11): no answer without admin".into(), false), (true, "usb: x".into(), false)], &[("GPU", false)]);
        assert_eq!(w.len(), 5, "{:?}", w);
        assert!(w.iter().any(|x| x.contains("19045")) && w.iter().any(|x| x.contains("GPU check")));
    }

    #[test]
    fn page_file_rules() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(pagefile_problem(&s(&["?:\\pagefile.sys"]), 65536.0).is_none());
        assert!(pagefile_problem(&s(&["C:\\pagefile.sys 0 0"]), 65536.0).is_none());
        assert!(pagefile_problem(&s(&["C:\\pagefile.sys 4096 8192"]), 65536.0).unwrap().contains("fixed size of at most 8 GB"));
        assert!(pagefile_problem(&s(&["C:\\pagefile.sys 65536 98304"]), 65536.0).is_none());
        assert!(pagefile_problem(&[], 65536.0).unwrap().contains("no page file"));
    }
}
