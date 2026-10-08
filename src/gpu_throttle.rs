//! NVIDIA GPU throttle reading via NVML, loaded at runtime (no link-time dependency).
//! Machines without NVIDIA return None quietly.

use std::sync::OnceLock;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

pub struct GpuThrottle {
    pub reasons: Vec<&'static str>,
    pub temp_c: u32,
    pub power_w: f64,
    pub limit_w: f64,
    pub sm_mhz: u32,
    pub sm_max_mhz: u32,
}

type Dev = *mut core::ffi::c_void;
type FnDevU64 = unsafe extern "C" fn(Dev, *mut u64) -> i32;
type FnDevU32 = unsafe extern "C" fn(Dev, *mut u32) -> i32;
type FnTemp = unsafe extern "C" fn(Dev, u32, *mut u32) -> i32;
type FnClock = unsafe extern "C" fn(Dev, u32, *mut u32) -> i32;

struct Nvml {
    dev: usize,
    reasons: FnDevU64,
    temp: FnTemp,
    power: FnDevU32,
    limit: FnDevU32,
    clock: FnClock,
    max_clock: FnClock,
    meminfo: Option<unsafe extern "C" fn(Dev, *mut [u64; 3]) -> i32>,
}

const NVML_TEMPERATURE_GPU: u32 = 0;
const NVML_CLOCK_SM: u32 = 1;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

unsafe fn sym<T: Copy>(lib: windows_sys::Win32::Foundation::HMODULE, name: &[u8]) -> Option<T> {
    let p = GetProcAddress(lib, name.as_ptr())?;
    Some(core::mem::transmute_copy::<_, T>(&p))
}

fn init() -> Option<Nvml> {
    unsafe {
        let mut lib = LoadLibraryW(wide("nvml.dll").as_ptr());
        if lib.is_null() {
            lib = LoadLibraryW(wide(r"C:\Program Files\NVIDIA Corporation\NVSMI\nvml.dll").as_ptr());
        }
        if lib.is_null() {
            return None;
        }
        let init: unsafe extern "C" fn() -> i32 = sym(lib, b"nvmlInit_v2\0")?;
        let by_index: unsafe extern "C" fn(u32, *mut Dev) -> i32 =
            sym(lib, b"nvmlDeviceGetHandleByIndex_v2\0")?;
        let reasons: FnDevU64 = sym(lib, b"nvmlDeviceGetCurrentClocksEventReasons\0")
            .or_else(|| sym(lib, b"nvmlDeviceGetCurrentClocksThrottleReasons\0"))?;
        let temp: FnTemp = sym(lib, b"nvmlDeviceGetTemperature\0")?;
        let power: FnDevU32 = sym(lib, b"nvmlDeviceGetPowerUsage\0")?;
        let limit: FnDevU32 = sym(lib, b"nvmlDeviceGetEnforcedPowerLimit\0")?;
        let clock: FnClock = sym(lib, b"nvmlDeviceGetClockInfo\0")?;
        let max_clock: FnClock = sym(lib, b"nvmlDeviceGetMaxClockInfo\0")?;
        if init() != 0 {
            return None;
        }
        let mut d: Dev = core::ptr::null_mut();
        if by_index(0, &mut d) != 0 || d.is_null() {
            return None;
        }
        let meminfo = sym(lib, b"nvmlDeviceGetMemoryInfo\0");
        Some(Nvml { dev: d as usize, reasons, temp, power, limit, clock, max_clock, meminfo })
    }
}

const MAP_ALL: [(u64, &str); 7] = [
    (0x4, "power cap (software)"), (0x8, "hardware slowdown"), (0x10, "sync boost"),
    (0x20, "thermal (software)"), (0x40, "thermal (hardware)"), (0x80, "power brake (hardware)"),
    (0x100, "display clock setting"),
];
pub const LABELS: [&str; 7] = ["power cap (software)", "hardware slowdown", "sync boost", "thermal (software)", "thermal (hardware)", "power brake (hardware)", "display clock setting"];

/// Run by the short-lived `snifrig --gpu-probe` child. Takes 3 readings 2 s apart and reports a
/// reason only if it held in all 3, then writes gpu.json for the monitor and exits (NVML goes with it).
pub fn probe(dir: &std::path::Path) {
    let mut rs: Vec<GpuThrottle> = Vec::new();
    for i in 0..3 {
        if i > 0 { std::thread::sleep(std::time::Duration::from_secs(2)); }
        match read() { Some(g) => rs.push(g), None => return }
    }
    let last = rs.pop().unwrap();
    let held: Vec<&str> = last.reasons.iter().filter(|r| rs.iter().all(|g| g.reasons.contains(r))).copied().collect();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (used_mb, total_mb) = mem_mb().unwrap_or((0, 0));
    let line = format!("{{\"unix\":{},\"reasons\":\"{}\",\"temp_c\":{},\"power_w\":{:.0},\"limit_w\":{:.0},\"sm_mhz\":{},\"sm_max_mhz\":{},\"used_mb\":{},\"total_mb\":{}}}",
        now, held.join(","), last.temp_c, last.power_w, last.limit_w, last.sm_mhz, last.sm_max_mhz, used_mb, total_mb);
    let tmp = dir.join("gpu.json.tmp");
    if std::fs::write(&tmp, line).is_ok() { let _ = std::fs::rename(&tmp, dir.join("gpu.json")); }
}

fn nvml() -> Option<&'static Nvml> {
    static N: OnceLock<Option<Nvml>> = OnceLock::new();
    N.get_or_init(init).as_ref()
}

/// (used, total) graphics memory in MB of GPU 0, via NVML. Used by the probe only.
pub fn mem_mb() -> Option<(u64, u64)> {
    let n = nvml()?;
    let f = n.meminfo?;
    let mut m = [0u64; 3]; // total, free, used (bytes)
    if unsafe { f(n.dev as Dev, &mut m) } != 0 { return None; }
    Some((m[2] >> 20, m[0] >> 20))
}

pub fn read() -> Option<GpuThrottle> {
    let n = nvml()?;
    let d = n.dev as Dev;
    unsafe {
        let (mut r, mut t, mut p, mut l, mut c, mut m) = (0u64, 0u32, 0u32, 0u32, 0u32, 0u32);
        if (n.reasons)(d, &mut r) != 0 {
            return None;
        }
        // Non-fatal: leave 0 if a query is unsupported.
        let _ = (n.temp)(d, NVML_TEMPERATURE_GPU, &mut t);
        let _ = (n.power)(d, &mut p);
        let _ = (n.limit)(d, &mut l);
        let _ = (n.clock)(d, NVML_CLOCK_SM, &mut c);
        let _ = (n.max_clock)(d, NVML_CLOCK_SM, &mut m);
        const MAP: [(u64, &str); 7] = MAP_ALL;
        let reasons = MAP.iter().filter(|(b, _)| r & b != 0).map(|(_, s)| *s).collect();
        Some(GpuThrottle {
            reasons,
            temp_c: t,
            power_w: p as f64 / 1000.0,
            limit_w: l as f64 / 1000.0,
            sm_mhz: c,
            sm_max_mhz: m,
        })
    }
}

#[cfg(test)]
mod gt_tests {
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    fn ws() -> usize {
        unsafe {
            let mut pc: PROCESS_MEMORY_COUNTERS = core::mem::zeroed();
            pc.cb = core::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            GetProcessMemoryInfo(GetCurrentProcess(), &mut pc, pc.cb);
            pc.WorkingSetSize
        }
    }

    #[test]
    fn read_twice() {
        let before = ws();
        for _ in 0..2 {
            match super::read() {
                Some(g) => println!(
                    "GPU {:?} {}C {:.1}/{:.1}W {}/{}MHz",
                    g.reasons, g.temp_c, g.power_w, g.limit_w, g.sm_mhz, g.sm_max_mhz
                ),
                None => println!("GPU none"),
            }
        }
        println!("WS before {} KB after {} KB delta {} KB", before / 1024, ws() / 1024, (ws() as i64 - before as i64) / 1024);
    }
}
