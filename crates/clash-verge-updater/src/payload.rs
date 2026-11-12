//! A signed update payload: a zip holding `manifest.json` and exactly the files it lists.

use crate::manifest::{MANIFEST_NAME, Manifest};
use anyhow::{Context as _, Result, ensure};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeSet;
use std::io::{Cursor, Read as _, Write as _};
use std::path::Path;

pub const MAX_PAYLOAD: u64 = 256 * 1024 * 1024;
const MAX_MANIFEST: u64 = 1024 * 1024;

pub struct Payload {
    pub manifest: Manifest,
    pub sha256: String,
    pub size: u64,
    pub key_id: String,
    bytes: Vec<u8>,
}

impl Payload {
    /// Verifies the signature before reading anything inside, then the manifest and every file
    /// against it.
    pub fn open(bytes: Vec<u8>, signatures: &[String]) -> Result<Self> {
        ensure!(bytes.len() as u64 <= MAX_PAYLOAD, "payload is too large");
        let key_id = crate::signature::verify(&bytes, signatures)?;
        Self::open_verified(bytes, key_id)
    }

    fn open_verified(bytes: Vec<u8>, key_id: String) -> Result<Self> {
        let sha256 = hex(&Sha256::digest(&bytes));
        let size = bytes.len() as u64;
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes.as_slice())).context("payload is not a zip")?;

        let manifest: Manifest = {
            let entry = archive.by_name(MANIFEST_NAME).context("payload has no manifest")?;
            let mut text = String::new();
            entry.take(MAX_MANIFEST).read_to_string(&mut text)?;
            serde_json::from_str(&text).context("invalid manifest")?
        };
        manifest.validate()?;

        let mut listed: BTreeSet<&str> = manifest.files.iter().map(|file| file.path.as_str()).collect();
        listed.insert(MANIFEST_NAME);
        let present = archive
            .file_names()
            .map(|name| name.map(|name| name.into_owned()))
            .collect::<Result<BTreeSet<String>, _>>()?;
        ensure!(
            present.iter().map(String::as_str).eq(listed.iter().copied()),
            "payload entries do not match its manifest"
        );

        for file in &manifest.files {
            let mut entry = archive.by_name(&file.path)?;
            ensure!(entry.is_file(), "{} is not a file", file.path);
            let mut hasher = Sha256::new();
            std::io::copy(&mut entry, &mut HashWriter(&mut hasher))?;
            ensure!(
                hex(&hasher.finalize()) == file.sha256,
                "{} does not match its digest",
                file.path
            );
        }

        Ok(Self {
            manifest,
            sha256,
            size,
            key_id,
            bytes,
        })
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Reads one listed file, checked against its digest.
    pub fn read(&self, name: &str) -> Result<Vec<u8>> {
        let file = self
            .manifest
            .file(name)
            .with_context(|| format!("{name} is not in the payload"))?;
        let mut archive = zip::ZipArchive::new(Cursor::new(self.bytes.as_slice()))?;
        let mut bytes = Vec::new();
        archive.by_name(name)?.read_to_end(&mut bytes)?;
        ensure!(
            hex(&Sha256::digest(&bytes)) == file.sha256,
            "{name} changed while reading"
        );
        Ok(bytes)
    }

    /// Writes one listed file to `dest`, checks it again and flushes it to disk.
    pub fn extract(&self, name: &str, dest: &Path) -> Result<()> {
        let file = self
            .manifest
            .file(name)
            .with_context(|| format!("{name} is not in the payload"))?;
        let mut archive = zip::ZipArchive::new(Cursor::new(self.bytes.as_slice()))?;
        let mut entry = archive.by_name(name)?;
        let mut out = std::fs::File::create(dest).with_context(|| format!("failed to create {}", dest.display()))?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0; 64 * 1024];
        loop {
            let read = entry.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            out.write_all(&buffer[..read])?;
        }
        out.sync_all()?;
        ensure!(
            hex(&hasher.finalize()) == file.sha256,
            "{name} changed while extracting"
        );
        Ok(())
    }
}

struct HashWriter<'a>(&'a mut Sha256);

impl std::io::Write for HashWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut HashWriter(&mut hasher))?;
    Ok(hex(&hasher.finalize()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::manifest::{FORMAT, FileEntry, Kind, Platform, Role};

    fn zip_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(&mut out);
        for (name, data) in files {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap();
        out.into_inner()
    }

    fn deb_manifest(digest: String) -> Vec<u8> {
        serde_json::to_vec(&Manifest {
            format: FORMAT,
            version: "2.5.10-custom.6".into(),
            platform: Platform::LinuxX86_64,
            kind: Kind::Deb,
            base_fingerprint: None,
            fingerprint: None,
            files: vec![FileEntry {
                path: "app.deb".into(),
                sha256: digest,
                role: Role::Package,
            }],
            cores: Vec::new(),
            service: None,
            updater: None,
            bundle: None,
            commit: "c".into(),
            built_at: "t".into(),
        })
        .unwrap()
    }

    #[test]
    fn checks_entries_and_digests() {
        let deb = b"package";
        let good = deb_manifest(hex(&Sha256::digest(deb)));
        let payload = Payload::open_verified(zip_of(&[("manifest.json", &good), ("app.deb", deb)]), "k".into());
        assert!(payload.is_ok());

        let tampered = Payload::open_verified(zip_of(&[("manifest.json", &good), ("app.deb", b"other")]), "k".into());
        assert!(tampered.is_err());

        let extra = Payload::open_verified(
            zip_of(&[("manifest.json", &good), ("app.deb", deb), ("x", b"x")]),
            "k".into(),
        );
        assert!(extra.is_err());
    }

    #[test]
    fn rejects_unsigned_payloads() {
        assert!(Payload::open(zip_of(&[("manifest.json", b"{}")]), &["AAAA".into()]).is_err());
    }
}
