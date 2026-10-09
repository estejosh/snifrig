//! Low-level Windows calls the governor needs.
//! Contract (every function closes its handles and never panics; failures return Err/None):
//! - snapshot(): every process via NtQuerySystemInformation(SystemProcessInformation=5) into a
//!   reusable buffer held by the caller: pid, ppid, image name, create time (FILETIME u64),
//!   total CPU time (kernel+user, 100 ns units). One syscall, no per-process handles.
//! - foreground_pid(): GetForegroundWindow + GetWindowThreadProcessId; None if no window.
//! - get_priority / set_priority: never set above NORMAL.
//! - set_efficiency: PROCESS_POWER_THROTTLING_EXECUTION_SPEED; off hands control back to Windows.
//! - set_io_priority: NtSetInformationProcess(ProcessIoPriority = 33), 0 very low, 1 low, 2 normal.
//! - set_memory_priority: SetProcessInformation(ProcessMemoryPriority), 1 very low .. 5 normal.
//! - set_affinity / get_affinity (process, system).
//! - cap_cpu / uncap: job object CPU hard cap, handles kept in a process-wide table by pid.
//! - image_matches(pid, created, name): pid still has that create time and image name.
//! - cmdline(pid): NtQueryInformationProcess class 60, "" if unreadable.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Mutex;

use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows_sys::Win32::System::JobObjects::*;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

#[derive(Clone, Debug, Default)]
pub struct Proc { pub pid: u32, pub ppid: u32, pub name: String, pub created: u64, pub cpu: u64 }

pub const IDLE: u32 = 0x40;
pub const BELOW_NORMAL: u32 = 0x4000;
pub const NORMAL: u32 = 0x20;

#[link(name = "ntdll", kind = "raw-dylib")]
extern "system" {
    fn NtQuerySystemInformation(class: u32, info: *mut c_void, len: u32, ret: *mut u32) -> i32;
    fn NtSetInformationProcess(h: HANDLE, class: u32, info: *const c_void, len: u32) -> i32;
    fn NtQueryInformationProcess(h: HANDLE, class: u32, info: *mut u8, len: u32, ret: *mut u32) -> i32;
}

const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004u32 as i32;
const STATUS_BUFFER_OVERFLOW: i32 = 0x8000_0005u32 as i32;
const STATUS_BUFFER_TOO_SMALL: i32 = 0xC000_0023u32 as i32;

struct H(HANDLE);
impl Drop for H { fn drop(&mut self) { unsafe { CloseHandle(self.0); } } }

fn open(pid: u32, access: u32) -> Result<H, String> {
    let h = unsafe { OpenProcess(access, 0, pid) };
    if h.is_null() { Err(format!("OpenProcess({pid}) failed: {}", unsafe { windows_sys::Win32::Foundation::GetLastError() })) } else { Ok(H(h)) }
}

fn rd<T: Copy>(b: &[u8], off: usize) -> T {
    assert!(off + std::mem::size_of::<T>() <= b.len());
    unsafe { std::ptr::read_unaligned(b.as_ptr().add(off) as *const T) }
}

pub fn snapshot(buf: &mut Vec<u8>) -> Vec<Proc> {
    if buf.len() < 1 << 20 { buf.resize(1 << 20, 0); }
    let mut got: u32 = 0;
    for _ in 0..8 {
        let st = unsafe { NtQuerySystemInformation(5, buf.as_mut_ptr() as *mut c_void, buf.len() as u32, &mut got) };
        if st == STATUS_INFO_LENGTH_MISMATCH {
            let want = (got as usize).max(buf.len()) + (256 << 10);
            buf.resize(want, 0);
            continue;
        }
        if st < 0 { return Vec::new(); }
        let mut out = Vec::with_capacity(512);
        let mut off = 0usize;
        loop {
            if off + 96 > buf.len() { break; }
            let b = &buf[..];
            let next: u32 = rd(b, off);
            let create: i64 = rd(b, off + 32);
            let user: i64 = rd(b, off + 40);
            let kern: i64 = rd(b, off + 48);
            let nlen: u16 = rd(b, off + 56);
            let nptr: usize = rd(b, off + 64);
            let pid: usize = rd(b, off + 80);
            let ppid: usize = rd(b, off + 88);
            let name = if nptr == 0 || nlen == 0 {
                if pid == 0 { "Idle".to_string() } else { String::new() }
            } else {
                let s = unsafe { std::slice::from_raw_parts(nptr as *const u16, (nlen / 2) as usize) };
                String::from_utf16_lossy(s)
            };
            out.push(Proc {
                pid: pid as u32, ppid: ppid as u32, name,
                created: create as u64, cpu: (user as u64).wrapping_add(kern as u64),
            });
            if next == 0 { break; }
            off += next as usize;
        }
        return out;
    }
    Vec::new()
}

/// (commit %, available physical MB) from GlobalMemoryStatusEx; None if the call fails.
pub fn memory_status() -> Option<(f64, f64)> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    unsafe {
        let mut m: MEMORYSTATUSEX = std::mem::zeroed();
        m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut m) == 0 || m.ullTotalPageFile == 0 { return None; }
        let used = m.ullTotalPageFile.saturating_sub(m.ullAvailPageFile) as f64;
        Some((used / m.ullTotalPageFile as f64 * 100.0, m.ullAvailPhys as f64 / 1048576.0))
    }
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

pub fn foreground_pid() -> Option<u32> {
    unsafe {
        let w = GetForegroundWindow();
        if w.is_null() { return None; }
        let mut pid = 0u32;
        GetWindowThreadProcessId(w, &mut pid);
        if pid == 0 { None } else { Some(pid) }
    }
}

pub fn get_priority(pid: u32) -> Option<u32> {
    let h = open(pid, PROCESS_QUERY_LIMITED_INFORMATION).ok()?;
    let c = unsafe { GetPriorityClass(h.0) };
    if c == 0 { None } else { Some(c) }
}

pub fn set_priority(pid: u32, class: u32) -> Result<(), String> {
    if class != IDLE && class != BELOW_NORMAL && class != NORMAL {
        return Err(format!("refusing priority class {class:#x}"));
    }
    let h = open(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SET_INFORMATION)?;
    if unsafe { SetPriorityClass(h.0, class) } == 0 { Err("SetPriorityClass failed".into()) } else { Ok(()) }
}

#[repr(C)]
struct PowerThrottling { version: u32, control: u32, state: u32 }

pub fn set_efficiency(pid: u32, on: bool) -> Result<(), String> {
    let h = open(pid, PROCESS_SET_INFORMATION)?;
    let bit = 1u32; // PROCESS_POWER_THROTTLING_EXECUTION_SPEED
    let s = PowerThrottling { version: 1, control: if on { bit } else { 0 }, state: if on { bit } else { 0 } };
    // ProcessPowerThrottling = 4
    let ok = unsafe { SetProcessInformation(h.0, 4, &s as *const _ as *const c_void, std::mem::size_of::<PowerThrottling>() as u32) };
    if ok == 0 { Err("SetProcessInformation(power throttling) failed".into()) } else { Ok(()) }
}

pub fn set_io_priority(pid: u32, prio: u32) -> Result<(), String> {
    let h = open(pid, PROCESS_SET_INFORMATION)?;
    let v = prio;
    let st = unsafe { NtSetInformationProcess(h.0, 33, &v as *const u32 as *const c_void, 4) };
    if st < 0 { Err(format!("NtSetInformationProcess(io) {st:#x}")) } else { Ok(()) }
}

pub fn set_memory_priority(pid: u32, prio: u32) -> Result<(), String> {
    let h = open(pid, PROCESS_SET_INFORMATION)?;
    let v = prio; // MEMORY_PRIORITY_INFORMATION { MemoryPriority: u32 }
    // ProcessMemoryPriority = 0
    let ok = unsafe { SetProcessInformation(h.0, 0, &v as *const u32 as *const c_void, 4) };
    if ok == 0 { Err("SetProcessInformation(memory priority) failed".into()) } else { Ok(()) }
}

pub fn get_affinity(pid: u32) -> Option<(u64, u64)> {
    let h = open(pid, PROCESS_QUERY_LIMITED_INFORMATION).ok()?;
    let (mut p, mut s) = (0usize, 0usize);
    if unsafe { GetProcessAffinityMask(h.0, &mut p, &mut s) } == 0 { None } else { Some((p as u64, s as u64)) }
}

pub fn set_affinity(pid: u32, mask: u64) -> Result<(), String> {
    let h = open(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SET_INFORMATION)?;
    if unsafe { SetProcessAffinityMask(h.0, mask as usize) } == 0 { Err("SetProcessAffinityMask failed".into()) } else { Ok(()) }
}

static JOBS: Mutex<Option<HashMap<u32, isize>>> = Mutex::new(None);

#[repr(C)]
struct CpuRate { flags: u32, rate: u32 }

fn set_rate(job: HANDLE, flags: u32, rate: u32) -> Result<(), String> {
    let c = CpuRate { flags, rate };
    // JobObjectCpuRateControlInformation = 15
    let ok = unsafe { SetInformationJobObject(job, 15, &c as *const _ as *const c_void, std::mem::size_of::<CpuRate>() as u32) };
    if ok == 0 { Err("SetInformationJobObject(cpu rate) failed".into()) } else { Ok(()) }
}

fn in_job(proc_h: HANDLE, job: HANDLE) -> bool {
    let mut r = 0;
    unsafe { IsProcessInJob(proc_h, job, &mut r) != 0 && r != 0 }
}

pub fn cap_cpu(pid: u32, percent: u32) -> Result<(), String> {
    let pct = percent.clamp(1, 100);
    let h = open(pid, PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION)?;
    let mut g = JOBS.lock().unwrap_or_else(|e| e.into_inner());
    let map = g.get_or_insert_with(HashMap::new);
    // prune jobs whose processes are all gone
    map.retain(|&k, j| {
        if k == pid { return true; }
        let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe { QueryInformationJobObject(*j as HANDLE, 1, &mut info as *mut _ as *mut c_void,
            std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32, std::ptr::null_mut()) };
        let alive = ok != 0 && info.ActiveProcesses > 0;
        if !alive { unsafe { CloseHandle(*j as HANDLE); } }
        alive
    });
    let flags = JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP;
    if let Some(&j) = map.get(&pid) {
        if in_job(h.0, j as HANDLE) {
            return set_rate(j as HANDLE, flags, pct * 100);
        }
        unsafe { CloseHandle(j as HANDLE); }
        map.remove(&pid);
    }
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() { return Err("CreateJobObjectW failed".into()); }
    let r = set_rate(job, flags, pct * 100).and_then(|_| {
        if unsafe { AssignProcessToJobObject(job, h.0) } == 0 { Err("AssignProcessToJobObject failed".to_string()) } else { Ok(()) }
    });
    match r {
        Ok(()) => { map.insert(pid, job as isize); Ok(()) }
        Err(e) => { unsafe { CloseHandle(job); } Err(e) }
    }
}

pub fn uncap(pid: u32) -> Result<(), String> {
    let g = JOBS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(map) = g.as_ref() else { return Ok(()) };
    let Some(&j) = map.get(&pid) else { return Ok(()) };
    // keep the handle so a later cap_cpu reuses the same job (a process cannot leave a job)
    set_rate(j as HANDLE, JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP, 10000)
}

fn ft(f: FILETIME) -> u64 { ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64 }

pub fn image_matches(pid: u32, created: u64, name: &str) -> bool {
    let Ok(h) = open(pid, PROCESS_QUERY_LIMITED_INFORMATION) else { return false };
    let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut c, mut e, mut k, mut u) = (z, z, z, z);
    if unsafe { GetProcessTimes(h.0, &mut c, &mut e, &mut k, &mut u) } == 0 { return false; }
    if ft(c) != created { return false; }
    let mut buf = [0u16; 1024];
    let mut n = buf.len() as u32;
    if unsafe { QueryFullProcessImageNameW(h.0, 0, buf.as_mut_ptr(), &mut n) } == 0 { return false; }
    let full = String::from_utf16_lossy(&buf[..n as usize]);
    let base = full.rsplit(['\\', '/']).next().unwrap_or(&full);
    base.eq_ignore_ascii_case(name)
}

pub fn cmdline(pid: u32) -> String {
    let Ok(h) = open(pid, PROCESS_QUERY_LIMITED_INFORMATION) else { return String::new() };
    let mut buf: Vec<u8> = vec![0; 4096];
    for _ in 0..4 {
        let mut ret = 0u32;
        let st = unsafe { NtQueryInformationProcess(h.0, 60, buf.as_mut_ptr(), buf.len() as u32, &mut ret) };
        if st == STATUS_INFO_LENGTH_MISMATCH || st == STATUS_BUFFER_OVERFLOW || st == STATUS_BUFFER_TOO_SMALL {
            let n = (ret as usize).max(buf.len() * 2);
            buf.resize(n, 0);
            continue;
        }
        if st < 0 { return String::new(); }
        let len: u16 = rd(&buf, 0);
        let ptr: usize = rd(&buf, 8);
        if ptr == 0 || len == 0 { return String::new(); }
        let s = unsafe { std::slice::from_raw_parts(ptr as *const u16, (len / 2) as usize) };
        return String::from_utf16_lossy(s);
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    fn spawn() -> std::process::Child {
        Command::new("ping").args(["-n", "30", "127.0.0.1"])
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
            .spawn().expect("spawn ping")
    }

    #[test]
    fn all_ops_on_child() {
        let mut child = spawn();
        let pid = child.id();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let mut buf = Vec::new();
        let snap = snapshot(&mut buf);
        let me = snap.iter().find(|p| p.pid == pid).cloned();
        let r = std::panic::catch_unwind(|| {
            let me = me.expect("child in snapshot");
            assert!(me.name.eq_ignore_ascii_case("ping.exe"), "name {}", me.name);
            assert!(image_matches(pid, me.created, "PING.EXE"));
            assert!(!image_matches(pid, me.created + 1, "ping.exe"));
            assert!(!image_matches(pid, me.created, "notping.exe"));
            // the child inherits our class; the test host may run below normal
            let p0 = get_priority(pid).unwrap();
            assert!(p0 == NORMAL || p0 == BELOW_NORMAL, "initial class {p0:#x}");
            for c in [BELOW_NORMAL, IDLE, NORMAL] {
                set_priority(pid, c).unwrap();
                assert_eq!(get_priority(pid), Some(c));
            }
            set_efficiency(pid, true).unwrap();
            set_efficiency(pid, false).unwrap();
            set_io_priority(pid, 1).unwrap();
            set_io_priority(pid, 2).unwrap();
            set_memory_priority(pid, 2).unwrap();
            set_memory_priority(pid, 5).unwrap();
            let (orig, sys) = get_affinity(pid).unwrap();
            let first = sys & sys.wrapping_neg();
            set_affinity(pid, first).unwrap();
            assert_eq!(get_affinity(pid).unwrap().0, first);
            set_affinity(pid, orig).unwrap();
            assert_eq!(get_affinity(pid).unwrap().0, orig);
            cap_cpu(pid, 20).unwrap();
            cap_cpu(pid, 30).unwrap();
            uncap(pid).unwrap();
            assert!(cmdline(pid).to_lowercase().contains("ping"));
            let _ = foreground_pid();
        });
        let _ = child.kill();
        let _ = child.wait();
        if let Err(e) = r { std::panic::resume_unwind(e); }
    }

    #[test]
    #[ignore]
    fn bench_snapshot() {
        let mut buf = Vec::new();
        let _ = snapshot(&mut buf);
        let t = std::time::Instant::now();
        let mut n = 0;
        for _ in 0..100 { n = snapshot(&mut buf).len(); }
        println!("snapshot avg {:?} ({} procs, buf {} KB)", t.elapsed() / 100, n, buf.len() / 1024);
    }
}
