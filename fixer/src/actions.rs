//! Carry out an action on a live process (see actions_windows.rs, actions_linux.rs).
#[cfg(windows)]
#[path = "actions_windows.rs"]
mod imp;
#[cfg(target_os = "linux")]
#[path = "actions_linux.rs"]
mod imp;
pub use imp::*;
