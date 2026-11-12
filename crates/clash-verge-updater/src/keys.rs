/// Public keys whose signatures the component accepts, in Tauri's encoding (base64 of the minisign
/// public key file). A key rotation adds the new key here before releases are signed with it.
pub const TRUSTED_KEYS: &[&str] = &[
    "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDBBNjkzRDcwNjJEMzY0RTcKUldUblpOTmljRDFwQ21CU2RiM2krZ1pQS3dtUS9ITVlqVEFWWWt5RTRvd205bTdiSDZwOS9SWm4K",
];

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    #[test]
    fn the_app_updater_key_is_trusted() {
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../../../src-tauri/tauri.conf.json")).unwrap();
        let key = config["plugins"]["updater"]["pubkey"].as_str().unwrap();
        assert!(super::TRUSTED_KEYS.contains(&key));
    }
}
