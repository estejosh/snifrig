//! Carry out an action on a live process. Identity is re-checked first because pids get recycled.
//! Ok(text) says what happened; Err(text) says why not. Every handle is closed.

use crate::{proc, Action, Target};
use std::ptr::null;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ACCESS_DENIED, HANDLE};
use windows_sys::Win32::System::ProcessStatus::K32EmptyWorkingSet;
use windows_sys::Win32::System::Services::*;
use windows_sys::Win32::System::Threading::*;

struct H(HANDLE);
impl Drop for H {
    fn drop(&mut self) { unsafe { CloseHandle(self.0); } }
}

struct Sc(SC_HANDLE);
impl Drop for Sc {
    fn drop(&mut self) { unsafe { CloseServiceHandle(self.0); } }
}

fn open(access: u32, t: &Target) -> Result<H, String> {
    let h = unsafe { OpenProcess(access, 0, t.pid) };
    if h.is_null() {
        let e = unsafe { GetLastError() };
        return Err(if e == ERROR_ACCESS_DENIED { format!("access denied for {} ({}); may need administrator rights", t.name, t.pid) } else { format!("cannot open {} ({}): error {}", t.name, t.pid, e) });
    }
    let h = H(h);
    // Re-check identity while we hold the handle: an open handle pins the pid, so the
    // process cannot exit and have its pid reused between this check and the action.
    match proc::image_name(t.pid) {
        Some(n) if n.eq_ignore_ascii_case(&t.name) => Ok(h),
        _ => Err("pid reused".into()),
    }
}

pub fn run(action: &Action, t: &Target) -> Result<String, String> {
    match proc::image_name(t.pid) {
        Some(n) if n.eq_ignore_ascii_case(&t.name) => {}
        _ => return Err("pid reused".into()),
    }
    let who = format!("{} ({})", t.name, t.pid);
    match action {
        Action::Report => Ok("reported".into()),
        Action::Trim => {
            let h = open(PROCESS_SET_QUOTA | PROCESS_QUERY_LIMITED_INFORMATION, t)?;
            if unsafe { K32EmptyWorkingSet(h.0) } == 0 { return Err(format!("trim failed for {}: error {}", who, unsafe { GetLastError() })); }
            Ok(format!("trimmed working set of {}", who))
        }
        Action::LowerPriority => {
            let h = open(PROCESS_SET_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION, t)?;
            if unsafe { SetPriorityClass(h.0, BELOW_NORMAL_PRIORITY_CLASS) } == 0 { return Err(format!("priority change failed for {}: error {}", who, unsafe { GetLastError() })); }
            Ok(format!("lowered priority of {}", who))
        }
        Action::Terminate => {
            let h = open(PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION, t)?;
            if unsafe { TerminateProcess(h.0, 1) } == 0 { return Err(format!("terminate failed for {}: error {}", who, unsafe { GetLastError() })); }
            Ok(format!("terminated {}", who))
        }
        Action::RestartService(name) => restart(name),
    }
}

fn restart(name: &str) -> Result<String, String> {
    let denied = |e: u32| if e == ERROR_ACCESS_DENIED { format!("needs administrator rights to restart {}", name) } else { format!("restart {} failed: error {}", name, e) };
    let w: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let scm = OpenSCManagerW(null(), null(), SC_MANAGER_CONNECT);
        if scm.is_null() { return Err(denied(GetLastError())); }
        let _scm = Sc(scm);
        let s = OpenServiceW(scm, w.as_ptr(), SERVICE_STOP | SERVICE_START | SERVICE_QUERY_STATUS);
        if s.is_null() { return Err(denied(GetLastError())); }
        let _s = Sc(s);
        let mut st: SERVICE_STATUS = std::mem::zeroed();
        if ControlService(s, SERVICE_CONTROL_STOP, &mut st) == 0 {
            let e = GetLastError();
            if e != 1062 /* ERROR_SERVICE_NOT_ACTIVE */ { return Err(denied(e)); }
        }
        let mut stopped = false;
        for _ in 0..40 {
            if QueryServiceStatus(s, &mut st) != 0 && st.dwCurrentState == SERVICE_STOPPED { stopped = true; break; }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        if !stopped { return Err(format!("{} did not stop within 20 s", name)); }
        if StartServiceW(s, 0, null()) == 0 { return Err(denied(GetLastError())); }
    }
    Ok(format!("restarted service {}", name))
}

#[cfg(test)]
mod action_tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn actions_pid_reused() {
        let mut c = Command::new("ping").args(["-n", "60", "127.0.0.1"]).stdin(Stdio::null()).stdout(Stdio::null()).spawn().unwrap();
        let mut t = proc::target(c.id()).unwrap();
        t.name = "notcmd.exe".into();
        let r = run(&Action::Terminate, &t);
        let alive = c.try_wait().unwrap().is_none();
        let _ = c.kill();
        assert!(r.unwrap_err().contains("pid reused"));
        assert!(alive);
    }

    #[test]
    #[ignore]
    fn actions_live_child() {
        let mut c = Command::new("ping").args(["-n", "60", "127.0.0.1"]).stdin(Stdio::null()).stdout(Stdio::null()).spawn().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));
        let t = proc::target(c.id()).expect("child");
        println!("{:?}", run(&Action::Trim, &t));
        let a = run(&Action::Trim, &t);
        let b = run(&Action::LowerPriority, &t);
        let d = run(&Action::Terminate, &t);
        let _ = c.wait();
        assert!(a.is_ok() && b.is_ok() && d.is_ok(), "{:?} {:?} {:?}", a, b, d);
        assert!(proc::image_name(t.pid).is_none());
    }
}
