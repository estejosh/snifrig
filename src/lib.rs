//! snifrig snif, the free monitor. One program per platform behind the same entry points
//! (`run`, `default_dir`, `json_str`, `json_num`), so the binaries are the same everywhere.
//! Windows: monitor_windows.rs. Linux: monitor_linux.rs.
#[cfg(windows)]
#[path = "monitor_windows.rs"]
mod imp;
#[cfg(target_os = "linux")]
#[path = "monitor_linux.rs"]
mod imp;
pub use imp::*;
