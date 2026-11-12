//! Everything that differs per operating system. Each module offers the same functions.

// Every platform offers the same signatures, so some return values are trivial on one of them.
#[cfg(target_os = "linux")]
#[allow(clippy::unnecessary_wraps)]
mod linux;
#[cfg(target_os = "macos")]
#[allow(clippy::unnecessary_wraps)]
mod macos;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
mod windows_trust;

#[cfg(target_os = "linux")]
pub use linux::*;
#[cfg(target_os = "macos")]
pub use macos::*;
#[cfg(windows)]
pub use windows::*;

/// A real directory (no symlink) that root owns and nobody else can write to.
#[cfg(unix)]
pub fn owned_by_root(path: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir() && meta.uid() == 0 && meta.mode() & 0o022 == 0)
}

/// How undoing one journal step went.
pub enum Undo {
    /// The step's change was reverted.
    #[cfg_attr(not(windows), allow(dead_code))]
    Restored,
    /// The step had not changed anything, or nothing needs reverting.
    Clean,
    /// Something else changed the same thing since; it is left as it is.
    Superseded,
}

/// Quotes command-line arguments the way `CommandLineToArgvW` and POSIX shells both read
/// unquoted paths without spaces; ours never contain quotes.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn command_line(program: &std::path::Path, args: &[&std::ffi::OsStr]) -> std::ffi::OsString {
    let mut line = std::ffi::OsString::from("\"");
    line.push(program.as_os_str());
    line.push("\"");
    for arg in args {
        line.push(" ");
        if arg.to_string_lossy().contains(' ') {
            line.push("\"");
            line.push(arg);
            line.push("\"");
        } else {
            line.push(arg);
        }
    }
    line
}
