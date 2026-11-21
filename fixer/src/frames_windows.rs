//! snifrig-frames: measures a game's real frame times (opt-in, set up once with your OK).
//!
//! Windows only lets an administrator read the graphics "present" events, so this small helper
//! runs from a scheduled task with highest privileges. It does nothing until snifrig asks: the
//! file frames-want holds the game's process id. While it is there, the helper listens to the
//! DXGI (Direct3D 10/11/12) and Direct3D 9 present events of that one process and writes
//! frames-now.txt once a second: unix pid fps median_ms p99_ms stutters. Nothing else is read,
//! nothing leaves the PC. Vulkan and OpenGL games are not covered; snifrig then falls back to
//! its CPU-based stutter detection. frames-off makes it stop and exit.
//!
//! Network per program (same reason: only an administrator may read kernel network events):
//! while net-want holds a recent time (the fixer writes it when the network is busy), the helper
//! counts bytes sent and received per process id from the Kernel-Network provider and writes
//! net-now.txt every second: "unix secs" then "pid bytes_in bytes_out" for the busiest 15.
//! Only byte counts per process id; no addresses, ports or contents.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use windows_sys::core::GUID;
use windows_sys::Win32::System::Diagnostics::Etw::*;

const DXGI: GUID = GUID::from_u128(0xCA11C036_0102_4A2D_A6AD_F03CFED5D3C9);
const D3D9: GUID = GUID::from_u128(0x783ACA0A_790E_4D7F_8451_AA850511C6B9);
static WANT: AtomicU32 = AtomicU32::new(0);
const KNET: GUID = GUID::from_u128(0x7DD42A49_5329_4832_8DFD_43D979153A88);
static NET: Mutex<Option<std::collections::HashMap<u32, (u64, u64)>>> = Mutex::new(None);
static STAMPS: Mutex<Vec<i64>> = Mutex::new(Vec::new());

fn wide(s: &str) -> Vec<u16> { s.encode_utf16().chain(Some(0)).collect() }
fn same(a: &GUID, b: &GUID) -> bool { a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4 }

unsafe extern "system" fn on_event(rec: *mut EVENT_RECORD) {
    let h = &(*rec).EventHeader;
    if same(&h.ProviderId, &KNET) {
        // TCP/UDP send and receive, IPv4 and IPv6: payload starts with PID (u32), size (u32).
        let id = h.EventDescriptor.Id;
        let out = matches!(id, 10 | 26 | 42 | 58);
        if !(out || matches!(id, 11 | 27 | 43 | 59)) || (*rec).UserDataLength < 8 { return; }
        let d = (*rec).UserData as *const u8;
        let pid = u32::from_le_bytes([*d, *d.add(1), *d.add(2), *d.add(3)]);
        let size = u32::from_le_bytes([*d.add(4), *d.add(5), *d.add(6), *d.add(7)]) as u64;
        // Then daddr, saddr: traffic inside this PC (127.x.x.x, ::1) never touches the network.
        let v6 = matches!(id, 26 | 27 | 58 | 59);
        let len = (*rec).UserDataLength as usize;
        let lo = if v6 { len >= 24 && (0..15).all(|k| *d.add(8 + k) == 0) && *d.add(23) == 1 } else { len >= 12 && *d.add(8) == 127 };
        if lo { return; }
        if let Ok(mut g) = NET.lock() {
            let e = g.get_or_insert_with(Default::default).entry(pid).or_default();
            if out { e.1 += size; } else { e.0 += size; }
        }
        return;
    }
    let pid = WANT.load(Ordering::Relaxed);
    if pid == 0 || h.ProcessId != pid { return; }
    let id = h.EventDescriptor.Id;
    let present = (same(&h.ProviderId, &DXGI) && (id == 42 || id == 55)) || (same(&h.ProviderId, &D3D9) && id == 1);
    if present { if let Ok(mut v) = STAMPS.lock() { v.push(h.TimeStamp); } }
}

/// Pure: net-now.txt from one window of byte counts, busiest 15 first.
fn net_text(now: u64, secs: f64, counts: std::collections::HashMap<u32, (u64, u64)>) -> String {
    let mut v: Vec<(u32, (u64, u64))> = counts.into_iter().filter(|(p, _)| *p != 0).collect();
    v.sort_by(|a, b| (b.1 .0 + b.1 .1).cmp(&(a.1 .0 + a.1 .1)));
    let mut s = format!("{} {:.2}\n", now, secs);
    for (pid, (i, o)) in v.into_iter().take(15) { s.push_str(&format!("{} {} {}\n", pid, i, o)); }
    s
}

/// Pure: (fps, median ms, p99 ms, stutters) from present timestamps in QPC ticks over `secs`.
fn stats(stamps: &[i64], freq: f64, secs: f64) -> (f64, f64, f64, usize) {
    if stamps.len() < 2 || freq <= 0.0 { return (stamps.len() as f64 / secs.max(0.001), 0.0, 0.0, 0); }
    let mut d: Vec<f64> = stamps.windows(2).map(|w| (w[1] - w[0]) as f64 * 1000.0 / freq).filter(|x| *x > 0.0).collect();
    let fps = stamps.len() as f64 / secs.max(0.001);
    d.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let q = |p: f64| d[((d.len() - 1) as f64 * p).round() as usize];
    let (med, p99) = (q(0.5), q(0.99));
    let stutters = d.iter().filter(|x| **x > (med * 2.5).max(33.4)).count();
    (fps, med, p99, stutters)
}

pub fn main() {
    let dir = std::env::args().skip_while(|a| a != "--dir").nth(1).map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default()).join("snifrig"));
    // It runs with administrator rights, so it only reads your folder; what it writes goes next to
    // it, in C:\Program Files\snifrig\state, where only administrators can write.
    let out = std::env::current_exe().ok().and_then(|e| Some(e.parent()?.parent()?.to_path_buf()))
        .filter(|r| r.to_string_lossy().to_lowercase().contains("program files"))
        .map(|r| r.join("state")).unwrap_or_else(|| dir.clone());
    let _ = std::fs::create_dir_all(&out);
    let name = wide("snifrig-frames");
    // EVENT_TRACE_PROPERTIES followed by room for the session name.
    let size = std::mem::size_of::<EVENT_TRACE_PROPERTIES>() + 1024;
    let mut buf = vec![0u8; size];
    let props = buf.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;
    unsafe {
        (*props).Wnode.BufferSize = size as u32;
        (*props).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        (*props).Wnode.ClientContext = 1; // QPC timestamps
        (*props).LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
        (*props).LoggerNameOffset = std::mem::size_of::<EVENT_TRACE_PROPERTIES>() as u32;
        // A session left over from a crash would block ours.
        ControlTraceW(std::mem::zeroed(), name.as_ptr(), props, EVENT_TRACE_CONTROL_STOP);
        (*props).Wnode.BufferSize = size as u32;
        (*props).LoggerNameOffset = std::mem::size_of::<EVENT_TRACE_PROPERTIES>() as u32;
        let mut session: CONTROLTRACE_HANDLE = std::mem::zeroed();
        let r = StartTraceW(&mut session, name.as_ptr(), props);
        if r != 0 { let _ = std::fs::write(out.join("frames-error.txt"), format!("could not start the frame trace (error {}); it needs administrator rights\n", r)); return; }
        let mut log: EVENT_TRACE_LOGFILEW = std::mem::zeroed();
        log.LoggerName = name.as_ptr() as *mut u16;
        log.Anonymous1.ProcessTraceMode = PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD;
        log.Anonymous2.EventRecordCallback = Some(on_event);
        let trace = OpenTraceW(&mut log);
        std::thread::spawn(move || { let t = trace; ProcessTrace(&t, 1, std::ptr::null(), std::ptr::null()); });
        let mut freq = 0i64;
        #[link(name = "kernel32", kind = "raw-dylib")]
        extern "system" { fn QueryPerformanceFrequency(f: *mut i64) -> i32; }
        QueryPerformanceFrequency(&mut freq);
        let mut enabled = false;
        let mut net_on = false;
        let mut last = std::time::Instant::now();
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            if dir.join("frames-off").exists() { break; }
            let want: u32 = std::fs::read_to_string(dir.join("frames-want")).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
            if want != 0 && !enabled {
                for g in [&DXGI, &D3D9] { EnableTraceEx2(session, g, EVENT_CONTROL_CODE_ENABLE_PROVIDER as u32, TRACE_LEVEL_VERBOSE as u8, u64::MAX, 0, 0, std::ptr::null()); }
                enabled = true;
            } else if want == 0 && enabled {
                for g in [&DXGI, &D3D9] { EnableTraceEx2(session, g, EVENT_CONTROL_CODE_DISABLE_PROVIDER as u32, 0, 0, 0, 0, std::ptr::null()); }
                enabled = false;
                let _ = std::fs::remove_file(out.join("frames-now.txt"));
            }
            WANT.store(want, Ordering::Relaxed);
            let now_s = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            let net_want = std::fs::read_to_string(dir.join("net-want")).ok().and_then(|s| s.trim().parse::<u64>().ok()).map_or(false, |t| now_s.saturating_sub(t) < 120);
            if net_want != net_on {
                let code = if net_want { EVENT_CONTROL_CODE_ENABLE_PROVIDER } else { EVENT_CONTROL_CODE_DISABLE_PROVIDER };
                EnableTraceEx2(session, &KNET, code as u32, TRACE_LEVEL_INFORMATION as u8, u64::MAX, 0, 0, std::ptr::null());
                net_on = net_want;
                if !net_on { let _ = std::fs::remove_file(out.join("net-now.txt")); }
            }
            let counts = NET.lock().ok().and_then(|mut g| g.take()).unwrap_or_default();
            let secs = last.elapsed().as_secs_f64();
            last = std::time::Instant::now();
            let stamps: Vec<i64> = STAMPS.lock().map(|mut v| std::mem::take(&mut *v)).unwrap_or_default();
            if net_on { let _ = std::fs::write(out.join("net-now.txt"), net_text(now_s, secs, counts)); }
            if want != 0 {
                let (fps, med, p99, st) = stats(&stamps, freq as f64, secs);
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                let _ = std::fs::write(out.join("frames-now.txt"), format!("{} {} {:.1} {:.1} {:.1} {}\n", now, want, fps, med, p99, st));
            }
        }
        ControlTraceW(session, name.as_ptr(), props, EVENT_TRACE_CONTROL_STOP);
        CloseTrace(trace);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn net_text_lists_the_busiest() {
        let m: std::collections::HashMap<u32, (u64, u64)> = [(1u32, (10, 0)), (2, (500, 500)), (0, (9999, 9999))].into_iter().collect();
        assert_eq!(super::net_text(5, 1.0, m), "5 1.00\n2 500 500\n1 10 0\n");
    }

    #[test]
    fn frame_stats() {
        // 60 fps with one 100 ms hitch
        let f = 10_000_000.0;
        let mut t = 0i64;
        let mut s = vec![0i64];
        for i in 0..60 { t += if i == 30 { 1_000_000 } else { 166_667 }; s.push(t); }
        let (fps, med, p99, st) = super::stats(&s, f, 1.0);
        assert!((fps - 61.0).abs() < 0.1);
        assert!((med - 16.7).abs() < 0.1, "{}", med);
        assert!(p99 > 16.0 && st == 1, "{} {}", p99, st);
    }
}
