//! Carry out an action on a live Linux process. Identity is re-checked first because pids get
//! recycled; a pidfd pins the process between the check and the action.
use crate::{proc, sys, Action, Target};

/// A pidfd for pid, then the identity check: the fd keeps referring to this process even if it
/// exits and the pid is reused, so a signal sent through it cannot hit a different program.
fn pin(t: &Target) -> Result<i32, String> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, t.pid as i32, 0) } as i32;
    if fd < 0 { return Err(format!("cannot open {} ({}): {}", t.name, t.pid, std::io::Error::last_os_error())); }
    match proc::image_name(t.pid) {
        Some(n) if n.eq_ignore_ascii_case(&t.name) => Ok(fd),
        _ => { unsafe { libc::close(fd); } Err("pid reused".into()) }
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
        // Linux has no "empty the working set"; it pages out by recent use on its own.
        Action::Trim => Err(format!("not on Linux: the kernel frees {}'s unused memory by itself", who)),
        Action::LowerPriority => sys::set_priority(t.pid, sys::BELOW_NORMAL).map(|_| format!("lowered priority of {}", who)),
        Action::Terminate => {
            let fd = pin(t)?;
            let r = unsafe { libc::syscall(libc::SYS_pidfd_send_signal, fd, libc::SIGTERM, std::ptr::null::<libc::siginfo_t>(), 0) };
            unsafe { libc::close(fd); }
            if r == 0 { Ok(format!("asked {} to end (SIGTERM)", who)) } else { Err(format!("terminate failed for {}: {}", who, std::io::Error::last_os_error())) }
        }
        Action::RestartService(name) => restart(name),
    }
}

/// Your own user services restart without root; system services need it, and snifrig says so.
fn restart(name: &str) -> Result<String, String> {
    let user = std::process::Command::new("systemctl").args(["--user", "restart", name]).output().map_err(|e| e.to_string())?;
    if user.status.success() { return Ok(format!("restarted {}", name)); }
    Err(format!("restarting {} needs administrator rights on Linux (it is a system service)", name))
}
