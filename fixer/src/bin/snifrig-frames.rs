//! snifrig-frames: measures a game's real frame times (opt-in, set up once with your OK).
//!
//! Windows only lets an administrator read the graphics "present" events, so this small helper
//! runs from a scheduled task with highest privileges. It does nothing until snifrig asks: the
//! file frames-want holds the game's process id. While it is there, the helper listens to the
//! DXGI (Direct3D 10/11/12) and Direct3D 9 present events of that one process and writes
//! frames-now.txt once a second: unix pid fps median_ms p99_ms stutters. Nothing else is read,
//! nothing leaves the PC. Vulkan and OpenGL games are not covered; snifrig then falls back to
//! its CPU-based stutter detection. frames-off makes it stop and exit.
#![windows_subsystem = "windows"]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use windows_sys::core::GUID;
use windows_sys::Win32::System::Diagnostics::Etw::*;

const DXGI: GUID = GUID::from_u128(0xCA11C036_0102_4A2D_A6AD_F03CFED5D3C9);
const D3D9: GUID = GUID::from_u128(0x783ACA0A_790E_4D7F_8451_AA850511C6B9);
static WANT: AtomicU32 = AtomicU32::new(0);
static STAMPS: Mutex<Vec<i64>> = Mutex::new(Vec::new());

fn wide(s: &str) -> Vec<u16> { s.encode_utf16().chain(Some(0)).collect() }
fn same(a: &GUID, b: &GUID) -> bool { a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4 }

unsafe extern "system" fn on_event(rec: *mut EVENT_RECORD) {
    let h = &(*rec).EventHeader;
    let pid = WANT.load(Ordering::Relaxed);
    if pid == 0 || h.ProcessId != pid { return; }
    let id = h.EventDescriptor.Id;
    let present = (same(&h.ProviderId, &DXGI) && (id == 42 || id == 55)) || (same(&h.ProviderId, &D3D9) && id == 1);
    if present { if let Ok(mut v) = STAMPS.lock() { v.push(h.TimeStamp); } }
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

fn main() {
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
            let secs = last.elapsed().as_secs_f64();
            last = std::time::Instant::now();
            let stamps: Vec<i64> = STAMPS.lock().map(|mut v| std::mem::take(&mut *v)).unwrap_or_default();
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
