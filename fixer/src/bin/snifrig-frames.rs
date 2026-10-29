// Frame-time helper (Windows: reads Direct3D present events with administrator rights).
#![cfg_attr(windows, windows_subsystem = "windows")]
#[cfg(windows)]
#[path = "../frames_windows.rs"]
mod frames;

#[cfg(windows)]
fn main() { frames::main() }

#[cfg(not(windows))]
fn main() { eprintln!("snifrig-frames is for Windows."); }
