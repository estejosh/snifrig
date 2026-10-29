//! The operating-system layer: the same functions on every platform, so the governor, learning,
//! rules and policy never see Windows or Linux. Windows: sys_windows.rs. Linux: sys_linux.rs.
#[cfg(windows)]
#[path = "sys_windows.rs"]
mod imp;
#[cfg(target_os = "linux")]
#[path = "sys_linux.rs"]
mod imp;
pub use imp::*;
