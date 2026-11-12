//! Fixed locations of the component. Everything here is writable only by SYSTEM / root; the
//! state directory is readable by everyone so the GUI can see an update in progress.

use std::path::{Path, PathBuf};

pub const SERVICE_NAME: &str = "clash_verge_updater";
pub const LAUNCHD_LABEL: &str = "io.github.clashverge.updater";
pub const SYSTEMD_UNIT: &str = "clash-verge-updater.service";

#[cfg(windows)]
pub const PIPE_NAME: &str = r"\\.\pipe\clash-verge-updater";

#[derive(Debug, Clone)]
pub struct Paths {
    root: PathBuf,
}

impl Paths {
    pub fn system() -> Self {
        Self { root: system_root() }
    }

    pub const fn at(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn bin_dir(&self) -> PathBuf {
        self.root.join("bin")
    }

    /// The resident program.
    pub fn program(&self) -> PathBuf {
        program_path(&self.root)
    }

    /// The previous resident program, kept to restore or recover from.
    pub fn previous_program(&self) -> PathBuf {
        let mut name = self.program().file_name().unwrap_or_default().to_os_string();
        name.push(".prev");
        self.program().with_file_name(name)
    }

    pub fn state_file(&self) -> PathBuf {
        self.root.join("state.json")
    }

    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.json")
    }

    pub fn lock_file(&self) -> PathBuf {
        self.root.join("update.lock")
    }

    /// Lines appended by installers that did not run through the component.
    pub fn external_installs(&self) -> PathBuf {
        self.root.join("external-installs")
    }

    pub fn txn_dir(&self, id: &str) -> PathBuf {
        self.root.join("txn").join(id)
    }

    pub fn payloads_dir(&self) -> PathBuf {
        self.root.join("payloads")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    #[cfg(unix)]
    pub fn socket(&self) -> PathBuf {
        socket_path()
    }
}

#[cfg(windows)]
fn system_root() -> PathBuf {
    let data = std::env::var_os("ProgramData").map_or_else(|| PathBuf::from(r"C:\ProgramData"), PathBuf::from);
    data.join("clash-verge-updater")
}

#[cfg(target_os = "linux")]
fn system_root() -> PathBuf {
    PathBuf::from("/var/lib/clash-verge-updater")
}

#[cfg(target_os = "macos")]
fn system_root() -> PathBuf {
    PathBuf::from("/Library/Application Support/Clash Verge Updater")
}

#[cfg(windows)]
fn program_path(root: &Path) -> PathBuf {
    root.join("bin").join("clash-verge-updater.exe")
}

#[cfg(target_os = "linux")]
fn program_path(root: &Path) -> PathBuf {
    root.join("bin").join("clash-verge-updater")
}

#[cfg(target_os = "macos")]
fn program_path(_root: &Path) -> PathBuf {
    PathBuf::from("/Library/PrivilegedHelperTools").join(LAUNCHD_LABEL)
}

#[cfg(target_os = "linux")]
pub fn socket_path() -> PathBuf {
    PathBuf::from("/run/clash-verge-updater/updater.sock")
}

#[cfg(target_os = "macos")]
pub fn socket_path() -> PathBuf {
    PathBuf::from("/var/run/clash-verge-updater/updater.sock")
}
