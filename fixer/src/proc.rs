//! Resolve processes on the live system (see proc_windows.rs, proc_linux.rs).
#[cfg(windows)]
#[path = "proc_windows.rs"]
mod imp;
#[cfg(target_os = "linux")]
#[path = "proc_linux.rs"]
mod imp;
pub use imp::*;
