//! Resolve processes on the live system.
//! - target(pid) is None if the process is gone or cannot be opened for query.
//! - cmdline via NtQueryInformationProcess class 60; "" if unreadable.
//! - service: the one running Win32 service with this pid, else None.
//! - top_private(n): biggest private-memory processes, excluding this one and pids 0/4.
//! Every handle is closed before returning.

use crate::{now_unix, Target};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
use windows_sys::Win32::System::ProcessStatus::*;
use windows_sys::Win32::System::Services::*;
use windows_sys::Win32::System::Threading::*;

#[link(name = "ntdll", kind = "raw-dylib")]
extern "system" {
    fn NtQueryInformationProcess(h: HANDLE, class: u32, buf: *mut u8, len: u32, ret: *mut u32) -> i32;
}

struct H(HANDLE);
impl Drop for H {
    fn drop(&mut self) { unsafe { CloseHandle(self.0); } }
}

fn open(pid: u32) -> Option<H> {
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() { None } else { Some(H(h)) }
}

fn base(path: &[u16]) -> String {
    let s = String::from_utf16_lossy(path);
    s.rsplit(['\\', '/']).next().unwrap_or("").to_string()
}

fn image_of(h: &H) -> Option<String> {
    let mut buf = [0u16; 1024];
    let mut n = buf.len() as u32;
    if unsafe { QueryFullProcessImageNameW(h.0, 0, buf.as_mut_ptr(), &mut n) } == 0 { return None; }
    Some(base(&buf[..n as usize]))
}

/// Current image base name of pid (e.g. "python.exe"), for identity re-checks right before acting.
pub fn image_name(pid: u32) -> Option<String> { image_of(&open(pid)?) }

fn cmdline(h: &H) -> String {
    let mut buf = vec![0u64; 8192]; // 64 KB, 8-byte aligned
    let mut ret = 0u32;
    let st = unsafe { NtQueryInformationProcess(h.0, 60, buf.as_mut_ptr() as *mut u8, (buf.len() * 8) as u32, &mut ret) };
    if st < 0 { return String::new(); }
    // UNICODE_STRING { u16 Length; u16 MaximumLength; PWSTR Buffer } (Buffer points inside buf)
    let len = (buf[0] & 0xFFFF) as usize / 2;
    let p = buf[1] as *const u16;
    let (lo, hi) = (buf.as_ptr() as usize, buf.as_ptr() as usize + buf.len() * 8);
    if p.is_null() || (p as usize) < lo || (p as usize) + len * 2 > hi { return String::new(); }
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) })
}

/// (pid, name) for running Win32 services.
fn services() -> Vec<(u32, String)> {
    let mut out = Vec::new();
    unsafe {
        let scm = OpenSCManagerW(null(), null(), SC_MANAGER_ENUMERATE_SERVICE);
        if scm.is_null() { return out; }
        let (mut need, mut count, mut resume) = (0u32, 0u32, 0u32);
        EnumServicesStatusExW(scm, SC_ENUM_PROCESS_INFO, SERVICE_WIN32, SERVICE_ACTIVE, null_mut(), 0, &mut need, &mut count, &mut resume, null());
        let mut buf = vec![0u64; (need as usize + 65536) / 8 + 1];
        resume = 0;
        let ok = EnumServicesStatusExW(scm, SC_ENUM_PROCESS_INFO, SERVICE_WIN32, SERVICE_ACTIVE, buf.as_mut_ptr() as *mut u8, (buf.len() * 8) as u32, &mut need, &mut count, &mut resume, null());
        if ok != 0 {
            let items = std::slice::from_raw_parts(buf.as_ptr() as *const ENUM_SERVICE_STATUS_PROCESSW, count as usize);
            for it in items {
                let mut n = 0;
                while *it.lpServiceName.add(n) != 0 { n += 1; }
                let name = String::from_utf16_lossy(std::slice::from_raw_parts(it.lpServiceName, n));
                out.push((it.ServiceStatusProcess.dwProcessId, name));
            }
        }
        CloseServiceHandle(scm);
    }
    out
}

fn build(pid: u32, svcs: &[(u32, String)]) -> Option<Target> {
    let h = open(pid)?;
    let name = image_of(&h)?;
    let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut c, mut e, mut k, mut u) = (z, z, z, z);
    if unsafe { GetProcessTimes(h.0, &mut c, &mut e, &mut k, &mut u) } == 0 { return None; }
    let ft = (((c.dwHighDateTime as u64) << 32) | c.dwLowDateTime as u64) as f64 / 1e7 - 11_644_473_600.0;
    let mut m: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
    m.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    let private_mb = if unsafe { K32GetProcessMemoryInfo(h.0, &mut m as *mut _ as *mut PROCESS_MEMORY_COUNTERS, m.cb) } != 0 {
        m.PrivateUsage as f64 / 1048576.0
    } else { 0.0 };
    let mine: Vec<&(u32, String)> = svcs.iter().filter(|s| s.0 == pid).collect();
    Some(Target {
        pid,
        name,
        age_secs: (now_unix() - ft).max(0.0),
        cmdline: cmdline(&h),
        service: if mine.len() == 1 { Some(mine[0].1.clone()) } else { None },
        private_mb,
    })
}

pub fn target(pid: u32) -> Option<Target> { build(pid, &services()) }

pub fn top_private(n: usize) -> Vec<Target> {
    let me = std::process::id();
    let mut pids = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap.is_null() || snap as isize == -1 { return Vec::new(); }
        let mut pe: PROCESSENTRY32W = std::mem::zeroed();
        pe.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snap, &mut pe);
        while ok != 0 {
            let p = pe.th32ProcessID;
            if p != 0 && p != 4 && p != me { pids.push(p); }
            ok = Process32NextW(snap, &mut pe);
        }
        CloseHandle(snap);
    }
    let svcs = services();
    let mut v: Vec<Target> = pids.into_iter().filter_map(|p| build(p, &svcs)).collect();
    v.sort_by(|a, b| b.private_mb.partial_cmp(&a.private_mb).unwrap_or(std::cmp::Ordering::Equal));
    v.truncate(n);
    v
}

#[cfg(test)]
mod proc_tests {
    use super::*;

    #[test]
    fn proc_target_self() {
        let t = target(std::process::id()).expect("self");
        assert!(t.name.to_lowercase().ends_with(".exe"), "{}", t.name);
        assert!(t.age_secs >= 0.0 && t.age_secs < 3600.0, "{}", t.age_secs);
        assert!(t.private_mb > 0.5 && t.private_mb < 4096.0, "{}", t.private_mb);
        assert!(!t.cmdline.is_empty());
        assert_eq!(image_name(t.pid).unwrap(), t.name);
    }

    #[test]
    fn proc_top_private() {
        let v = top_private(3);
        assert_eq!(v.len(), 3);
        assert!(v[0].private_mb >= v[1].private_mb && v[1].private_mb >= v[2].private_mb);
        assert!(v.iter().all(|t| t.pid != std::process::id() && t.pid > 4));
    }

    #[test]
    fn proc_gone() {
        assert!(target(0xFFFF_FFF0).is_none());
    }
}
