//! snifrig snif, the free monitor. One program per platform behind the same entry points
//! (`run`, `default_dir`, `json_str`, `json_num`), so the binaries are the same everywhere.
//! Windows: monitor_windows.rs. Linux: monitor_linux.rs.

// The app, the MCP server and scripts run `snifrig ...` and read what it prints. If the caller
// closes the pipe first, std's println! panics, the panic message fails too, and the process
// aborts (0xc0000409). Printing must never crash snifrig: these write and ignore errors. Defined
// before the modules so every module below uses them.
macro_rules! println { ($($t:tt)*) => {{ use std::io::Write as _; let _ = writeln!(std::io::stdout(), $($t)*); }} }
macro_rules! eprintln { ($($t:tt)*) => {{ use std::io::Write as _; let _ = writeln!(std::io::stderr(), $($t)*); }} }
macro_rules! print { ($($t:tt)*) => {{ use std::io::Write as _; let _ = write!(std::io::stdout(), $($t)*); }} }

pub mod queue;

#[cfg(windows)]
#[path = "monitor_windows.rs"]
mod imp;
#[cfg(target_os = "linux")]
#[path = "monitor_linux.rs"]
mod imp;
pub use imp::*;
