// Snifrig tray icon (Windows). On Linux the monitor runs as a systemd user service and the
// snifrig window shows its state; a tray icon there comes with the desktop step.
#![cfg_attr(windows, windows_subsystem = "windows")]
#[cfg(windows)]
#[path = "../tray_windows.rs"]
mod tray;

#[cfg(windows)]
fn main() { tray::main() }

#[cfg(not(windows))]
fn main() {
    eprintln!("snifrig-tray is for Windows. On Linux, open the snifrig window instead.");
}
