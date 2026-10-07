//! Evidence that Windows itself (power plan, thermals, background work, drivers, disk, paging, GPU clocks)
//! is slowing the machine. One persistent PDH query, sampled once per monitor cycle. Nothing is guessed:
//! every finding records what was measured, the limit crossed, for how long, and the busiest processes.
use std::collections::HashMap;
use std::path::Path;
use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, GetDriveTypeW, GetLogicalDrives, OPEN_EXISTING};
use windows_sys::Win32::System::Ioctl::{DISK_PERFORMANCE, IOCTL_DISK_PERFORMANCE};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Performance::*;
use windows_sys::Win32::System::Power::{GetSystemPowerStatus, PowerGetActiveScheme, PowerReadFriendlyName, SYSTEM_POWER_STATUS};
use windows_sys::Win32::System::Threading::{
    GetProcessInformation, OpenProcess, ProcessPowerThrottling, PROCESS_POWER_THROTTLING_STATE, PROCESS_QUERY_LIMITED_INFORMATION,
};

const FMT: u32 = PDH_FMT_DOUBLE | 0x8000; // 0x8000 = PDH_FMT_NOCAP100 (missing from windows-sys)
const NEED: u32 = 3; // consecutive samples before a condition counts
const REPEAT: f64 = 3600.0; // at most one record per kind per hour
const PATHS: [&str; 7] = [
    "\\Processor Information(_Total)\\% Processor Performance",
    "\\Processor Information(_Total)\\% Processor Utility",
    "\\Thermal Zone Information(*)\\% Passive Limit",
    "\\Thermal Zone Information(*)\\Temperature (Kelvin)",
    "\\Processor(_Total)\\% DPC Time",
    "\\Processor(_Total)\\% Interrupt Time",
    "\\Memory\\Page Reads/sec",
];
pub const KINDS: [&str; 8] = ["cpu-throttle", "thermal", "os-maintenance", "efficiency-mode", "driver-latency", "disk-saturated", "paging", "gpu-throttle"];
const MAINT: [&str; 13] = [
    "msmpeng.exe", "mpdefendercoreservice.exe", "searchindexer.exe", "searchprotocolhost.exe", "searchfilterhost.exe", "tiworker.exe",
    "trustedinstaller.exe", "compattelrunner.exe", "mousocoreworker.exe", "usocoreworker.exe", "wuauclt.exe", "sppsvc.exe", "wmiprvse.exe",
];

pub struct Evidence { pub t: f64, pub kind: &'static str, pub value: f64, pub limit: f64, pub minutes: f64, pub detail: String, pub who: String }

impl Evidence {
    pub fn json(&self) -> String {
        format!("{{\"t\":\"{}\",\"unix\":{:.0},\"kind\":\"{}\",\"value\":{:.2},\"limit\":{:.2},\"minutes\":{:.1},\"who\":\"{}\",\"detail\":\"{}\"}}",
            crate::iso(self.t), self.t, self.kind, self.value, self.limit, self.minutes, crate::esc(&self.who), crate::esc(&self.detail))
    }
    pub fn msg(&self) -> String {
        format!("Windows slowdown [{}]: measured {:.1} against limit {:.1} for {:.1} min. {}", self.kind, self.value, self.limit, self.minutes, self.detail)
    }
}

pub struct Slow {
    q: isize,
    c: [isize; 7],
    dprev: HashMap<u8, (i64, i64, i64, i64, u32, u32)>,
    buf: Vec<u64>,
    streak: HashMap<&'static str, (u32, f64)>,
    last: HashMap<&'static str, f64>,
    now: [Option<f64>; 7],
    gpu_now: String,
    /// Data dir; set by the monitor. NVML costs ~25 MB, so it never loads in the resident
    /// monitor: a short-lived `snifrig --gpu-probe` child writes gpu.json every 5 min instead.
    pub dir: Option<std::path::PathBuf>,
    gpu_spawned: f64,
    gpu_seen: f64,
}

impl Slow {
    pub fn open() -> Slow {
        let mut s = Slow { q: 0, c: [0; 7], dprev: HashMap::new(), buf: Vec::new(), streak: HashMap::new(), last: HashMap::new(), now: [None; 7], gpu_now: String::new(), dir: None, gpu_spawned: -1e12, gpu_seen: 0.0 };
        unsafe {
            let mut q: isize = 0;
            if PdhOpenQueryW(std::ptr::null(), 0, &mut q) != 0 { return s; }
            s.q = q;
            for (i, p) in PATHS.iter().enumerate() {
                let w: Vec<u16> = p.encode_utf16().chain(Some(0)).collect();
                let mut h: isize = 0;
                if PdhAddEnglishCounterW(q, w.as_ptr(), 0, &mut h) == 0 { s.c[i] = h; }
            }
            PdhCollectQueryData(q);
        }
        s
    }

    fn get(&mut self, i: usize) -> Vec<(String, f64)> {
        let mut out = Vec::new();
        let c = self.c[i];
        if c == 0 { return out; }
        unsafe {
            let (mut sz, mut n) = (0u32, 0u32);
            let r = PdhGetFormattedCounterArrayW(c, FMT, &mut sz, &mut n, std::ptr::null_mut());
            if r != 0x8000_07D2 || sz == 0 { return out; }
            self.buf.resize(sz as usize / 8 + 8, 0);
            let mut sz2 = (self.buf.len() * 8) as u32;
            let p = self.buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
            if PdhGetFormattedCounterArrayW(c, FMT, &mut sz2, &mut n, p) != 0 { return out; }
            for k in 0..n as usize {
                let it = &*p.add(k);
                if it.FmtValue.CStatus > 1 { continue; }
                let mut l = 0;
                while *it.szName.add(l) != 0 { l += 1; }
                out.push((String::from_utf16_lossy(std::slice::from_raw_parts(it.szName, l)), it.FmtValue.Anonymous.doubleValue));
            }
        }
        out
    }

    /// Busiest fixed volume since the previous call: (idle %, seconds per transfer, drive letter).
    /// Read straight from the disk driver (IOCTL_DISK_PERFORMANCE); the PDH PhysicalDisk object makes PDH spawn extra pool threads.
    fn disk(&mut self) -> Option<(f64, f64, char)> {
        let mut best: Option<(f64, f64, char)> = None;
        unsafe {
            let mask = GetLogicalDrives();
            for d in 0..26u8 {
                if mask & (1 << d) == 0 { continue; }
                let l = (b'A' + d) as char;
                let root: Vec<u16> = format!("{}:\\", l).encode_utf16().chain(Some(0)).collect();
                if GetDriveTypeW(root.as_ptr()) != 3 { continue; }
                let path: Vec<u16> = format!("\\\\.\\{}:", l).encode_utf16().chain(Some(0)).collect();
                let h = CreateFileW(path.as_ptr(), 0, 3, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut());
                if h == INVALID_HANDLE_VALUE { continue; }
                let mut dp: DISK_PERFORMANCE = std::mem::zeroed();
                let mut ret = 0u32;
                let ok = DeviceIoControl(h, IOCTL_DISK_PERFORMANCE, std::ptr::null(), 0, &mut dp as *mut _ as *mut core::ffi::c_void,
                    std::mem::size_of::<DISK_PERFORMANCE>() as u32, &mut ret, std::ptr::null_mut()) != 0;
                CloseHandle(h);
                if !ok { continue; }
                let cur = (dp.IdleTime, dp.QueryTime, dp.ReadTime, dp.WriteTime, dp.ReadCount, dp.WriteCount);
                if let Some(p) = self.dprev.insert(d, cur) {
                    let dq = cur.1 - p.1;
                    if dq <= 0 { continue; }
                    let idle = ((cur.0 - p.0) as f64 / dq as f64 * 100.0).clamp(0.0, 100.0);
                    let n = cur.4.wrapping_sub(p.4) as f64 + cur.5.wrapping_sub(p.5) as f64;
                    let lat = if n > 0.0 { ((cur.2 - p.2) + (cur.3 - p.3)) as f64 / n / 1e7 } else { 0.0 };
                    if best.map_or(true, |b| idle < b.0) { best = Some((idle, lat, l)); }
                }
            }
        }
        best
    }

    fn one(&mut self, i: usize) -> Option<f64> { self.get(i).first().map(|x| x.1) }

    /// One-line view of the latest readings (used by the --once report).
    pub fn now_line(&self) -> String {
        let f = |o: Option<f64>, u: &str| o.map(|v| format!("{:.0}{}", v, u)).unwrap_or_else(|| "n/a".into());
        let n = &self.now;
        format!("CPU at {} of max speed ({} utility) | DPC+interrupt {} | disk idle {}, {} per transfer | page reads {}/s{}",
            f(n[0], "%"), f(n[1], "%"), f(n[2].zip(n[3]).map(|x| x.0 + x.1), "%"), f(n[4], "%"),
            n[5].map(|v| format!("{:.0} ms", v * 1000.0)).unwrap_or_else(|| "n/a".into()), f(n[6], ""), self.gpu_now)
    }

    /// Starts a probe child every 5 min (only if nvml.dll exists) and returns a reading only
    /// once per new gpu.json, so each probe counts as one sample. The probe itself requires
    /// throttling in all 3 of its readings, 2 s apart, before it reports reasons.
    fn gpu_probe(&mut self, t: f64) -> Option<crate::gpu_throttle::GpuThrottle> {
        let dir = self.dir.clone()?;
        let sys = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        if !std::path::Path::new(&sys).join("System32\\nvml.dll").exists() { return None; }
        if t - self.gpu_spawned >= 300.0 {
            self.gpu_spawned = t;
            if let Ok(exe) = std::env::current_exe() {
                use std::os::windows::process::CommandExt;
                use std::process::Stdio;
                let exe = exe.with_file_name("snifrig.exe");
                let _ = std::process::Command::new(exe).arg("--gpu-probe").arg("--dir").arg(&dir)
                    .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).creation_flags(0x0800_0000).spawn();
            }
        }
        let s = std::fs::read_to_string(dir.join("gpu.json")).ok()?;
        let u = crate::json_num(&s, "unix") as f64;
        if u <= self.gpu_seen || t - u > 360.0 { return None; }
        self.gpu_seen = u;
        let n = |k: &str| crate::json_num(&s, k) as f64;
        let reasons: Vec<&'static str> = crate::json_str(&s, "reasons").unwrap_or_default().split(',')
            .filter_map(|r| crate::gpu_throttle::LABELS.iter().find(|l| **l == r).copied()).collect();
        Some(crate::gpu_throttle::GpuThrottle { reasons, temp_c: n("temp_c") as u32, power_w: n("power_w"), limit_w: n("limit_w"), sm_mhz: n("sm_mhz") as u32, sm_max_mhz: n("sm_max_mhz") as u32 })
    }

    /// `procs` is (pid, image name, CPU % of the whole machine since the previous cycle).
    pub fn sample(&mut self, t: f64, procs: &[(u32, String, f64)]) -> Vec<Evidence> {
        if self.q != 0 { unsafe { PdhCollectQueryData(self.q); } }
        let dk = self.disk();
        let (perf, util, dpc, intr, idle, lat, pgr) =
            (self.one(0), self.one(1), self.one(4), self.one(5), dk.map(|x| x.0), dk.map(|x| x.1), self.one(6));
        self.now = [perf, util, dpc, intr, idle, lat, pgr];
        let pl = self.get(2);
        let tk = self.get(3);

        let mut by_name: HashMap<&str, f64> = HashMap::new();
        for (_, n, p) in procs { *by_name.entry(n.as_str()).or_insert(0.0) += p; }
        let mut top: Vec<(&str, f64)> = by_name.into_iter().collect();
        top.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let fmt_list = |v: &[(&str, f64)]| v.iter().map(|(n, p)| format!("{} {:.0}%", n, p)).collect::<Vec<_>>().join(", ");
        let top3: Vec<(&str, f64)> = top.iter().take(3).cloned().collect();

        let maint: Vec<(&str, f64)> = top.iter().filter(|(n, _)| MAINT.contains(&n.to_ascii_lowercase().as_str())).cloned().collect();
        let maint_sum: f64 = maint.iter().map(|x| x.1).sum();

        let mut eff: Vec<(&str, f64)> = Vec::new();
        let mut by_cpu: Vec<&(u32, String, f64)> = procs.iter().collect();
        by_cpu.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        for (pid, n, p) in by_cpu.into_iter().take(20) {
            if *p > 5.0 && eco(*pid) { eff.push((n.as_str(), *p)); }
        }
        let eff_sum: f64 = eff.iter().map(|x| x.1).sum();

        let gpu = self.gpu_probe(t);
        self.gpu_now = match &gpu {
            Some(g) if !g.reasons.is_empty() => format!(" | GPU limited by {}", g.reasons.join(", ")),
            Some(g) => format!(" | GPU clock {}/{} MHz, no throttle", g.sm_mhz, g.sm_max_mhz),
            None => String::new(),
        };

        let cap = pl.iter().filter(|x| x.1 < 100.0).min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        let dl = dpc.unwrap_or(0.0) + intr.unwrap_or(0.0);
        let mut hit: Vec<(&'static str, f64, f64)> = Vec::new();
        if let (Some(p), Some(u)) = (perf, util) { if p < 80.0 && u > 50.0 { hit.push(("cpu-throttle", p, 80.0)); } }
        if let Some(z) = cap { hit.push(("thermal", z.1, 100.0)); }
        if maint_sum > 15.0 { hit.push(("os-maintenance", maint_sum, 15.0)); }
        if !eff.is_empty() { hit.push(("efficiency-mode", eff_sum, 5.0)); }
        if dpc.is_some() && dl > 10.0 { hit.push(("driver-latency", dl, 10.0)); }
        if let (Some(i), Some(l)) = (idle, lat) { if i < 10.0 && l > 0.05 { hit.push(("disk-saturated", l * 1000.0, 50.0)); } }
        if let Some(r) = pgr { if r > 500.0 { hit.push(("paging", r, 500.0)); } }
        if let Some(g) = &gpu { if !g.reasons.is_empty() { hit.push(("gpu-throttle", g.sm_mhz as f64, g.sm_max_mhz as f64)); } }

        let mut out = Vec::new();
        for kind in KINDS {
            let h = match hit.iter().find(|x| x.0 == kind) {
                Some(h) => *h,
                None => { self.streak.remove(kind); continue; }
            };
            let e = self.streak.entry(kind).or_insert((0, t));
            e.0 += 1;
            let (n, start) = *e;
            if n < (if kind == "gpu-throttle" { 1 } else { NEED }) || t - self.last.get(kind).copied().unwrap_or(-1e12) < REPEAT { continue; }
            self.last.insert(kind, t);
            let busy = fmt_list(&top3);
            let (detail, who) = match kind {
                "cpu-throttle" => (format!("CPUs ran at {:.0}% of max speed while {:.0}% busy. Power plan \"{}\", {}.", h.1, util.unwrap_or(0.0), plan_name(), power_src()), busy_names(&top3)),
                "thermal" => (cap.map(|z| format!("thermal zone {} capped CPU to {}% of max at {:.0} C.", z.0, z.1 as i64,
                        tk.iter().find(|k| k.0 == z.0).map(|k| k.1 - 273.15).unwrap_or(0.0))).unwrap_or_default(), busy_names(&top3)),
                "os-maintenance" => (format!("Windows background work used {:.0}% of the machine: {}.", h.1, fmt_list(&maint)), busy_names(&maint)),
                "efficiency-mode" => (format!("in efficiency mode (EcoQoS), throttled by Windows or by the app itself: {}.", fmt_list(&eff)), busy_names(&eff)),
                "driver-latency" => (format!("DPC {:.1}% + interrupt {:.1}% of CPU time spent in drivers.", dpc.unwrap_or(0.0), intr.unwrap_or(0.0)), busy_names(&top3)),
                "disk-saturated" => (format!("busiest volume {}: idle {:.0}%, {:.0} ms per transfer.", dk.map(|x| x.2).unwrap_or('?'), idle.unwrap_or(0.0), h.1), busy_names(&top3)),
                "paging" => (format!("{:.0} hard page reads/s: memory is being read back from disk.", h.1), busy_names(&top3)),
                _ => gpu.as_ref().map(|g| (format!("GPU limited by {}: {} C, {:.0}/{:.0} W, SM clock {}/{} MHz.", g.reasons.join(", "), g.temp_c, g.power_w, g.limit_w, g.sm_mhz, g.sm_max_mhz), busy_names(&top3))).unwrap_or_default(),
            };
            let detail = if kind == "os-maintenance" || kind == "efficiency-mode" { detail } else { format!("{} Top CPU: {}.", detail, busy) };
            out.push(Evidence { t, kind, value: h.1, limit: h.2, minutes: (t - start) / 60.0, detail, who });
        }
        out
    }
}

fn busy_names(v: &[(&str, f64)]) -> String { v.iter().map(|x| x.0).collect::<Vec<_>>().join(",") }

fn eco(pid: u32) -> bool {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() { return false; }
        let mut s = PROCESS_POWER_THROTTLING_STATE { Version: 1, ControlMask: 0, StateMask: 0 };
        let ok = GetProcessInformation(h, ProcessPowerThrottling, &mut s as *mut _ as *mut core::ffi::c_void, std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32) != 0;
        CloseHandle(h);
        ok && s.ControlMask & 1 != 0 && s.StateMask & 1 != 0
    }
}

fn power_src() -> &'static str {
    unsafe {
        let mut s: SYSTEM_POWER_STATUS = std::mem::zeroed();
        if GetSystemPowerStatus(&mut s) == 0 { return "power source unknown"; }
        match s.ACLineStatus { 1 => "on AC power", 0 => "on battery", _ => "power source unknown" }
    }
}

fn plan_name() -> String {
    unsafe {
        let mut g: *mut windows_sys::core::GUID = std::ptr::null_mut();
        if PowerGetActiveScheme(std::ptr::null_mut(), &mut g) != 0 || g.is_null() { return "unknown".into(); }
        let mut sz = 0u32;
        PowerReadFriendlyName(std::ptr::null_mut(), g, std::ptr::null(), std::ptr::null(), std::ptr::null_mut(), &mut sz);
        let mut name = "unknown".to_string();
        if sz >= 2 {
            let mut b = vec![0u8; sz as usize];
            if PowerReadFriendlyName(std::ptr::null_mut(), g, std::ptr::null(), std::ptr::null(), b.as_mut_ptr(), &mut sz) == 0 {
                let w: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
                name = String::from_utf16_lossy(&w);
            }
        }
        LocalFree(g as *mut core::ffi::c_void);
        name
    }
}

// ---------- summary of the last 24 h ----------
fn num(s: &str, key: &str) -> f64 {
    let pat = format!("\"{}\":", key);
    s.find(&pat).map(|i| s[i + pat.len()..].chars().take_while(|c| c.is_ascii_digit() || matches!(c, '.' | '-')).collect::<String>().parse().unwrap_or(0.0)).unwrap_or(0.0)
}

pub fn summarize(dir: &Path) -> Vec<String> {
    let mut txt = std::fs::read_to_string(dir.join("slowdown.old")).unwrap_or_default();
    txt.push_str(&std::fs::read_to_string(dir.join("slowdown.jsonl")).unwrap_or_default());
    let cut = crate::now() - 86400.0;
    // kind -> (unix, value, limit, minutes, who)
    let mut rows: HashMap<String, Vec<(f64, f64, f64, f64, String)>> = HashMap::new();
    for l in txt.lines() {
        let (u, k) = (num(l, "unix"), crate::json_str(l, "kind").unwrap_or_default());
        if u >= cut && !k.is_empty() { rows.entry(k).or_default().push((u, num(l, "value"), num(l, "limit"), num(l, "minutes"), crate::json_str(l, "who").unwrap_or_default())); }
    }
    let mut out = Vec::new();
    for kind in KINDS {
        let Some(v) = rows.get_mut(kind) else { continue };
        v.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let (mut eps, mut mins, mut first, mut prev) = (0, 0.0, 0.0, 0.0);
        for (i, r) in v.iter().enumerate() {
            if i == 0 || r.0 - prev > REPEAT + 300.0 { if i > 0 { mins += (prev - first) / 60.0 + v[i - 1].3; } eps += 1; first = r.0; }
            prev = r.0;
        }
        mins += (prev - first) / 60.0 + v[v.len() - 1].3;
        let low = matches!(kind, "cpu-throttle" | "thermal" | "gpu-throttle");
        let w = v.iter().min_by(|a, b| { let o = a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal); if low { o } else { o.reverse() } }).unwrap();
        let mut who: HashMap<&str, u32> = HashMap::new();
        for r in v.iter() { for n in r.4.split(',').filter(|s| !s.is_empty()) { *who.entry(n).or_insert(0) += 1; } }
        let mut who: Vec<(&str, u32)> = who.into_iter().collect();
        who.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        let who = who.iter().take(3).map(|x| x.0).collect::<Vec<_>>().join(", ");
        out.push(format!("{}: {} episode{}, {:.0} min total, worst {:.1} (limit {:.0}){}", kind, eps, if eps == 1 { "" } else { "s" }, mins, w.1, w.2,
            if who.is_empty() { String::new() } else { format!(", mostly {}", who) }));
    }
    out
}
