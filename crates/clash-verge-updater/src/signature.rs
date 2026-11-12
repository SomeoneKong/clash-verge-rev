//! Minisign signatures in Tauri's encoding: both the public key and the signature are the
//! base64 of the respective minisign file.

use anyhow::{Context as _, Result, bail};
use base64::Engine as _;
use sha2::{Digest as _, Sha256};

fn decode_text(value: &str) -> Result<String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value.trim())
        .context("not base64")?;
    String::from_utf8(bytes).context("not text")
}

/// A short, stable name for a trusted key, recorded in the audit log.
pub fn key_id(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    digest[..8].iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Verifies `bytes` against `signatures` with `keys`; returns the id of the key that matched.
pub fn verify_with(bytes: &[u8], signatures: &[String], keys: &[&str]) -> Result<String> {
    for key in keys {
        let Ok(public_key) = decode_text(key)
            .and_then(|text| minisign_verify::PublicKey::decode(&text).map_err(|error| anyhow::anyhow!("{error}")))
        else {
            continue;
        };
        for signature in signatures {
            let Ok(signature) = decode_text(signature)
                .and_then(|text| minisign_verify::Signature::decode(&text).map_err(|error| anyhow::anyhow!("{error}")))
            else {
                continue;
            };
            if public_key.verify(bytes, &signature, true).is_ok() {
                return Ok(key_id(key));
            }
        }
    }
    bail!("no signature matches a trusted key")
}

pub fn verify(bytes: &[u8], signatures: &[String]) -> Result<String> {
    verify_with(bytes, signatures, crate::keys::TRUSTED_KEYS)
}
