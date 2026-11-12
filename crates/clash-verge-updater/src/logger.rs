//! A plain diagnostic log beside the audit log; the audit log is the record of updates, this one is
//! for troubleshooting the component.

use clash_verge_updater::paths::Paths;
use std::io::Write as _;

const MAX_SIZE: u64 = 2 * 1024 * 1024;

pub fn log(message: &str) {
    let line = format!(
        "{} [{}] {message}\n",
        clash_verge_updater::audit::now(),
        std::process::id()
    );
    eprint!("{line}");
    // Before the directory is known to be the component's own, nothing is written there.
    let paths = Paths::system();
    if !crate::platform::layout_trusted(&paths) {
        return;
    }
    let dir = paths.logs_dir();
    let path = dir.join("updater.log");
    if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > MAX_SIZE) {
        let _ = std::fs::rename(&path, dir.join("updater.log.1"));
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = file.write_all(line.as_bytes());
    }
}
