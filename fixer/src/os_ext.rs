//! Small things std gives only on Windows, with the same names everywhere so shared code reads
//! the same. `creation_flags` (no window, priority) means nothing on Linux: processes there have
//! no windows of their own, so it is a no-op. `raw_arg` passes the text as one argument.
#[cfg(windows)]
pub use std::os::windows::process::CommandExt;

#[cfg(not(windows))]
pub trait CommandExt {
    fn creation_flags(&mut self, flags: u32) -> &mut Self;
    fn raw_arg<S: AsRef<std::ffi::OsStr>>(&mut self, arg: S) -> &mut Self;
}

#[cfg(not(windows))]
impl CommandExt for std::process::Command {
    fn creation_flags(&mut self, _flags: u32) -> &mut Self { self }
    fn raw_arg<S: AsRef<std::ffi::OsStr>>(&mut self, arg: S) -> &mut Self { self.arg(arg) }
}
