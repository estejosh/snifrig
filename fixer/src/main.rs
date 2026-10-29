//! snifrig fix. Windows: main_windows.rs. Linux: the governor needs to know which app you are
//! using before it may slow anything down, and that comes with the desktop step, so on Linux
//! this program only says so for now. Everything it decides with (governor, learning, rules,
//! policy, the scheduler) is in the library and builds and tests on Linux already.
#[cfg(windows)]
#[path = "main_windows.rs"]
mod app;

#[cfg(windows)]
fn main() { app::main() }

#[cfg(not(windows))]
fn main() {
    eprintln!("snifrig fix for Linux is coming: the free monitor (snifrig) runs on Linux now.");
    std::process::exit(2);
}
