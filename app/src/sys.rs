//! Live numbers for the window: total CPU, RAM, and CPU per process, sampled once a second
//! on a background thread only while the window is open.
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::{Arc, Mutex};

#[link(name = "ntdll", kind = "raw-dylib")]
extern "system" { fn NtQuerySystemInformation(class: u32, info: *mut c_void, len: u32, ret: *mut u32) -> i32; }

#[derive(Clone, Debug)]
pub struct ProcNow { pub pid: u32, pub name: String, pub core_pct: f64, pub ws_mb: f64 }

#[derive(Clone, Default)]
pub struct Live {
    pub cpu_pct: f64,
    pub ram_used_mb: f64,
    pub ram_total_mb: f64,
    pub cores: usize,
    pub cpu_hist: VecDeque<f32>,
    pub ram_hist: VecDeque<f32>,
    pub procs: Vec<ProcNow>,
}

const HIST: usize = 120;

fn rd<T: Copy>(b: &[u8], off: usize) -> T { unsafe { std::ptr::read_unaligned(b.as_ptr().add(off) as *const T) } }

struct Raw { pid: u32, name: String, created: i64, cpu: u64, ws: u64 }

fn snapshot(buf: &mut Vec<u8>) -> Vec<Raw> {
    if buf.len() < 1 << 20 { buf.resize(1 << 20, 0); }
    let mut got = 0u32;
    for _ in 0..8 {
        let st = unsafe { NtQuerySystemInformation(5, buf.as_mut_ptr() as *mut c_void, buf.len() as u32, &mut got) };
        if st == 0xC000_0004u32 as i32 { let w = (got as usize).max(buf.len()) + (256 << 10); buf.resize(w, 0); continue; }
        if st < 0 { return Vec::new(); }
        let mut out = Vec::with_capacity(512);
        let mut off = 0usize;
        loop {
            if off + 152 > buf.len() { break; }
            let b = &buf[..];
            let next: u32 = rd(b, off);
            let created: i64 = rd(b, off + 32);
            let user: i64 = rd(b, off + 40);
            let kern: i64 = rd(b, off + 48);
            let nlen: u16 = rd(b, off + 56);
            let nptr: usize = rd(b, off + 64);
            let pid: usize = rd(b, off + 80);
            let ws: usize = rd(b, off + 144);
            let name = if nptr == 0 || nlen == 0 { if pid == 0 { "Idle".into() } else { "System".into() } } else {
                String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(nptr as *const u16, (nlen / 2) as usize) })
            };
            out.push(Raw { pid: pid as u32, name, created, cpu: (user as u64).wrapping_add(kern as u64), ws: ws as u64 });
            if next == 0 { break; }
            off += next as usize;
        }
        return out;
    }
    Vec::new()
}

fn ram() -> (f64, f64) {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    unsafe {
        let mut m: MEMORYSTATUSEX = std::mem::zeroed();
        m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut m) == 0 { return (0.0, 0.0); }
        let total = m.ullTotalPhys as f64 / 1048576.0;
        (total - m.ullAvailPhys as f64 / 1048576.0, total)
    }
}

/// Starts the sampler. It stops by itself when the window (the other Arc holder) is gone.
pub fn start() -> Arc<Mutex<Live>> {
    let live = Arc::new(Mutex::new(Live::default()));
    let l = live.clone();
    std::thread::spawn(move || {
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        let mut buf = Vec::new();
        let mut prev: HashMap<(u32, i64), u64> = HashMap::new();
        let mut t0 = std::time::Instant::now();
        loop {
            if Arc::strong_count(&l) < 2 { return; }
            let raw = snapshot(&mut buf);
            let dt = t0.elapsed().as_secs_f64().max(0.2);
            t0 = std::time::Instant::now();
            let mut next = HashMap::with_capacity(raw.len());
            let mut procs = Vec::with_capacity(raw.len());
            let mut busy = 0.0;
            for r in &raw {
                let k = (r.pid, r.created);
                let pct = prev.get(&k).map_or(0.0, |c0| r.cpu.saturating_sub(*c0) as f64 / 1e7 / dt * 100.0);
                next.insert(k, r.cpu);
                if r.pid == 0 { continue; }
                busy += pct;
                procs.push(ProcNow { pid: r.pid, name: r.name.clone(), core_pct: pct, ws_mb: r.ws as f64 / 1048576.0 });
            }
            let first = prev.is_empty();
            prev = next;
            procs.sort_by(|a, b| b.core_pct.partial_cmp(&a.core_pct).unwrap_or(std::cmp::Ordering::Equal));
            let (used, total) = ram();
            if let Ok(mut g) = l.lock() {
                if !first {
                    g.cpu_pct = (busy / cores as f64).min(100.0);
                    let c = g.cpu_pct as f32;
                    g.cpu_hist.push_back(c);
                    if g.cpu_hist.len() > HIST { g.cpu_hist.pop_front(); }
                    g.procs = procs;
                }
                g.cores = cores;
                g.ram_used_mb = used;
                g.ram_total_mb = total;
                let r = if total > 0.0 { (used / total * 100.0) as f32 } else { 0.0 };
                g.ram_hist.push_back(r);
                if g.ram_hist.len() > HIST { g.ram_hist.pop_front(); }
            }
            std::thread::sleep(std::time::Duration::from_millis(if first { 300 } else { 1000 }));
        }
    });
    live
}

/// Seconds to add to unix time to get local time (daylight saving included).
pub fn local_offset_secs() -> i64 {
    use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    unsafe {
        let mut z: TIME_ZONE_INFORMATION = std::mem::zeroed();
        let r = GetTimeZoneInformation(&mut z);
        if r == 0xFFFF_FFFF { return 0; }
        let bias = z.Bias + if r == 2 { z.DaylightBias } else { z.StandardBias };
        -(bias as i64) * 60
    }
}
