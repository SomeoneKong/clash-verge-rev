//! `manifest.json` of an update payload. The whole payload is signed, so everything the component
//! decides from (version, platform, kind, file list and hashes) comes from signed content.

use anyhow::{Context as _, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const FORMAT: u32 = 1;
pub const MANIFEST_NAME: &str = "manifest.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Platform {
    #[serde(rename = "windows-x86_64")]
    WindowsX86_64,
    #[serde(rename = "linux-x86_64")]
    LinuxX86_64,
    #[serde(rename = "linux-aarch64")]
    LinuxAarch64,
    #[serde(rename = "darwin-aarch64")]
    DarwinAarch64,
}

impl Platform {
    /// The platform this program was built for, if payloads exist for it.
    pub const fn current() -> Option<Self> {
        if cfg!(all(windows, target_arch = "x86_64")) {
            Some(Self::WindowsX86_64)
        } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            Some(Self::LinuxX86_64)
        } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
            Some(Self::LinuxAarch64)
        } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            Some(Self::DarwinAarch64)
        } else {
            None
        }
    }

    pub const fn key(self) -> &'static str {
        match self {
            Self::WindowsX86_64 => "windows-x86_64",
            Self::LinuxX86_64 => "linux-x86_64",
            Self::LinuxAarch64 => "linux-aarch64",
            Self::DarwinAarch64 => "darwin-aarch64",
        }
    }

    const fn kinds(self) -> &'static [Kind] {
        match self {
            Self::WindowsX86_64 => &[Kind::Patch, Kind::Full],
            Self::LinuxX86_64 | Self::LinuxAarch64 => &[Kind::Deb],
            Self::DarwinAarch64 => &[Kind::Bundle],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Replaces the app and core files in place (Windows, same install fingerprint).
    Patch,
    /// Runs the NSIS installer silently (Windows, install fingerprint changed).
    Full,
    /// Installs the Debian package.
    Deb,
    /// Replaces the macOS app bundle (by the GUI) and publishes its privileged parts.
    Bundle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Gui,
    Core,
    /// The service binary, placed beside the service tool that installs it.
    Service,
    /// `clash-verge-service-install`, run from the protected staging directory.
    ServiceTool,
    Updater,
    Installer,
    Package,
    Bundle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub sha256: String,
    pub role: Role,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreEntry {
    pub name: String,
    pub version: String,
    pub sha256: String,
}

/// The bundle's own Tauri signature, which the GUI's updater checks again when it installs it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleMeta {
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    pub version: String,
    pub platform: Platform,
    pub kind: Kind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    pub files: Vec<FileEntry>,
    #[serde(default)]
    pub cores: Vec<CoreEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updater: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<BundleMeta>,
    pub commit: String,
    pub built_at: String,
}

const CORE_NAMES: [&str; 2] = ["verge-mihomo", "verge-mihomo-alpha"];

impl Manifest {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.format == FORMAT, "unknown payload format {}", self.format);
        crate::version::parse(&self.version)?;
        ensure!(
            self.platform.kinds().contains(&self.kind),
            "a {:?} payload does not apply to {}",
            self.kind,
            self.platform.key()
        );
        let mut names = BTreeSet::new();
        for file in &self.files {
            ensure!(is_plain_name(&file.path), "unsafe payload path {:?}", file.path);
            ensure!(is_sha256(&file.sha256), "bad digest for {}", file.path);
            ensure!(names.insert(file.path.as_str()), "{} is listed twice", file.path);
        }
        self.validate_layout()?;
        // An installer or package carries its cores inside; a patch or bundle carries them as files.
        let carries_cores = matches!(self.kind, Kind::Patch | Kind::Bundle);
        for core in &self.cores {
            ensure!(is_sha256(&core.sha256), "bad digest for core {}", core.name);
            if carries_cores {
                let file = self
                    .file(&self.core_file_name(&core.name))
                    .with_context(|| format!("core {} has no file", core.name))?;
                ensure!(
                    file.sha256 == core.sha256,
                    "core {} digest differs from its file",
                    core.name
                );
            }
        }
        for digest in [&self.base_fingerprint, &self.fingerprint, &self.service, &self.updater]
            .into_iter()
            .flatten()
        {
            ensure!(is_sha256(digest), "bad digest {digest:?}");
        }
        Ok(())
    }

    /// Each kind carries exactly these files; anything else in a payload is refused.
    fn validate_layout(&self) -> Result<()> {
        let exe = if self.platform == Platform::WindowsX86_64 {
            ".exe"
        } else {
            ""
        };
        let named = |name: &str, role| (format!("{name}{exe}"), role);
        let mut expected: Vec<(String, Role)> = match self.kind {
            Kind::Patch => vec![
                named("clash-verge", Role::Gui),
                named("verge-mihomo", Role::Core),
                named("verge-mihomo-alpha", Role::Core),
                named("clash-verge-service", Role::Service),
                named("clash-verge-service-install", Role::ServiceTool),
                named("clash-verge-updater", Role::Updater),
            ],
            Kind::Bundle => vec![
                named("verge-mihomo", Role::Core),
                named("verge-mihomo-alpha", Role::Core),
                named("clash-verge-service", Role::Service),
                named("clash-verge-service-install", Role::ServiceTool),
                named("clash-verge-updater", Role::Updater),
            ],
            Kind::Full | Kind::Deb => Vec::new(),
        };
        let single = match self.kind {
            Kind::Full => Some((Role::Installer, "-setup.exe")),
            Kind::Deb => Some((Role::Package, ".deb")),
            Kind::Bundle => Some((Role::Bundle, ".app.tar.gz")),
            Kind::Patch => None,
        };
        if let Some((role, suffix)) = single {
            let file = self
                .files
                .iter()
                .find(|file| file.role == role)
                .with_context(|| format!("{:?} payload has no {role:?} file", self.kind))?;
            ensure!(file.path.ends_with(suffix), "{} is not a {suffix} file", file.path);
            expected.push((file.path.clone(), role));
        }
        let mut actual: Vec<(String, Role)> = self.files.iter().map(|f| (f.path.clone(), f.role)).collect();
        expected.sort_by(|a, b| a.0.cmp(&b.0));
        actual.sort_by(|a, b| a.0.cmp(&b.0));
        if expected != actual {
            bail!("{:?} payload files do not match its kind", self.kind);
        }
        match self.kind {
            Kind::Patch => ensure!(
                self.base_fingerprint.is_some() && self.fingerprint == self.base_fingerprint,
                "a patch keeps the install fingerprint"
            ),
            Kind::Full => ensure!(self.fingerprint.is_some(), "a full payload names its fingerprint"),
            Kind::Bundle => ensure!(self.bundle.is_some(), "a bundle payload carries its update metadata"),
            Kind::Deb => {}
        }
        ensure!(
            self.cores.iter().all(|core| CORE_NAMES.contains(&core.name.as_str())),
            "unknown core name"
        );
        Ok(())
    }

    pub fn file(&self, path: &str) -> Option<&FileEntry> {
        self.files.iter().find(|file| file.path == path)
    }

    pub fn files_with(&self, role: Role) -> impl Iterator<Item = &FileEntry> {
        self.files.iter().filter(move |file| file.role == role)
    }

    pub fn core_file_name(&self, core: &str) -> String {
        if self.platform == Platform::WindowsX86_64 {
            format!("{core}.exe")
        } else {
            core.to_owned()
        }
    }
}

fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name != MANIFEST_NAME
        && !name.contains(['/', '\\', ':', '\0'])
        && !name.starts_with('.')
}

pub fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const D: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    fn entry(path: &str, role: Role) -> FileEntry {
        FileEntry {
            path: path.into(),
            sha256: D.into(),
            role,
        }
    }

    fn patch() -> Manifest {
        Manifest {
            format: FORMAT,
            version: "2.5.10-custom.6".into(),
            platform: Platform::WindowsX86_64,
            kind: Kind::Patch,
            base_fingerprint: Some(D.into()),
            fingerprint: Some(D.into()),
            files: vec![
                entry("clash-verge.exe", Role::Gui),
                entry("verge-mihomo.exe", Role::Core),
                entry("verge-mihomo-alpha.exe", Role::Core),
                entry("clash-verge-service.exe", Role::Service),
                entry("clash-verge-service-install.exe", Role::ServiceTool),
                entry("clash-verge-updater.exe", Role::Updater),
            ],
            cores: vec![CoreEntry {
                name: "verge-mihomo".into(),
                version: "v1.19.34-custom.2".into(),
                sha256: D.into(),
            }],
            service: None,
            updater: None,
            bundle: None,
            commit: "c".into(),
            built_at: "t".into(),
        }
    }

    #[test]
    fn accepts_a_patch() {
        patch().validate().unwrap();
    }

    #[test]
    fn refuses_extra_or_unsafe_files() {
        let mut extra = patch();
        extra.files.push(entry("evil.dll", Role::Gui));
        assert!(extra.validate().is_err());

        let mut traversal = patch();
        traversal.files[0].path = "..\\clash-verge.exe".into();
        assert!(traversal.validate().is_err());

        let mut wrong_kind = patch();
        wrong_kind.platform = Platform::LinuxX86_64;
        assert!(wrong_kind.validate().is_err());

        let mut changed_layout = patch();
        changed_layout.fingerprint = Some("1".repeat(64));
        assert!(changed_layout.validate().is_err());
    }

    #[test]
    fn a_bundle_payload_holds_the_app_and_the_privileged_parts() {
        let bundle = Manifest {
            platform: Platform::DarwinAarch64,
            kind: Kind::Bundle,
            base_fingerprint: None,
            fingerprint: None,
            files: vec![
                entry("Clash.Verge.app.tar.gz", Role::Bundle),
                entry("verge-mihomo", Role::Core),
                entry("verge-mihomo-alpha", Role::Core),
                entry("clash-verge-service", Role::Service),
                entry("clash-verge-service-install", Role::ServiceTool),
                entry("clash-verge-updater", Role::Updater),
            ],
            bundle: Some(BundleMeta { signature: "s".into() }),
            ..patch()
        };
        bundle.validate().unwrap();
        let mut unsigned = bundle.clone();
        unsigned.bundle = None;
        assert!(unsigned.validate().is_err());
        let mut no_tool = bundle;
        no_tool.files.retain(|file| file.role != Role::ServiceTool);
        assert!(no_tool.validate().is_err());
    }

    #[test]
    fn a_deb_payload_holds_one_package() {
        let deb = Manifest {
            platform: Platform::LinuxX86_64,
            kind: Kind::Deb,
            base_fingerprint: None,
            fingerprint: None,
            files: vec![entry("Clash.Verge_2.5.10-custom.6_amd64.deb", Role::Package)],
            ..patch()
        };
        deb.validate().unwrap();
        let mut two = deb;
        two.files.push(entry("other.deb", Role::Package));
        assert!(two.validate().is_err());
    }
}
