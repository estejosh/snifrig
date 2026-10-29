//! Live system sampling, one file per platform with the same functions.
#[cfg(windows)]
#[path = "sys_windows.rs"]
mod imp;
#[cfg(target_os = "linux")]
#[path = "sys_linux.rs"]
mod imp;
pub use imp::*;
