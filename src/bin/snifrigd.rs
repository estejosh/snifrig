// Windowless build of the monitor, started at login. No console window.
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    snifrig::run();
}
