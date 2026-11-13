//! The Service session that owns the running core, kept on disk so that the next app adopts the
//! core instead of restarting it: after an update, or after the app ended without stopping it.
//!
//! The record holds the session proof, so it is private to the current user. Another process of
//! that user can already restart the core and load any configuration as its owner; the proof only
//! lets it do so without a restart.

use crate::utils::dirs;
use anyhow::{Context as _, Result};
use clash_verge_logging::{Type, logging};
use clash_verge_service_ipc::RuntimeBundle;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_yaml_ng::{Mapping, Value};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::UNIX_EPOCH;

const RECORD_FILE: &str = "service-session.json";
const FORMAT: u32 = 1;

/// Top-level keys mihomo applies only when it starts (`hub.applyRoute`): a reload keeps the
/// controller the core started with.
const STARTUP_KEYS: &[&str] = &[
    "external-controller",
    "external-controller-tls",
    "external-controller-unix",
    "external-controller-pipe",
    "external-controller-routing-mark",
    "external-controller-cors",
    "secret",
    "external-ui",
    "external-doh-server",
    "tls",
];

/// The core process of a session; the Service watchdog replaces it within the same session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CoreProcess {
    pub pid: u32,
    pub started_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SessionRecord {
    pub format: u32,
    pub generation: u64,
    pub token: String,
    /// The core the session started, so that a switch to another one restarts.
    pub core: String,
    pub process: Option<CoreProcess>,
    /// Digest of the start-only settings the core process runs with. Every staged runtime keeps
    /// them, so a watchdog restart from the staged configuration runs with them too.
    pub startup: String,
    /// A staging began and was not seen to end: a reload the app sent may still reach the core.
    pub applying: bool,
    /// The runtime the core is known to have loaded; `None` when that is uncertain.
    pub fingerprint: Option<String>,
    /// Node selections the profile holds were not all put back on the core yet.
    #[serde(default)]
    pub restoring_selections: bool,
}

impl SessionRecord {
    pub(crate) const fn new(
        generation: u64,
        token: String,
        core: String,
        process: Option<CoreProcess>,
        startup: String,
        fingerprint: Option<String>,
    ) -> Self {
        Self {
            format: FORMAT,
            generation,
            token,
            core,
            process,
            startup,
            applying: false,
            fingerprint,
            restoring_selections: false,
        }
    }

    fn is_well_formed(&self) -> bool {
        self.format == FORMAT
            && self.token.len() == 64
            && self.token.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
            && !self.startup.is_empty()
    }
}

/// The session of this app, mirrored on disk. The file is written under this lock, so writes
/// land in the order the record changed.
static CURRENT: Mutex<Option<SessionRecord>> = Mutex::new(None);

/// A reload this app sent to its core may still be on its way. Kept apart from the record, which
/// can be dropped while the core keeps running.
static RELOAD_PENDING: AtomicBool = AtomicBool::new(false);

/// The start-only settings digest of the session this app owns.
pub(crate) fn current_startup() -> Option<String> {
    CURRENT.lock().as_ref().map(|record| record.startup.clone())
}

/// Takes a session the app now owns, started or adopted.
pub(crate) fn record(record: SessionRecord) {
    RELOAD_PENDING.store(false, Ordering::Release);
    let mut current = CURRENT.lock();
    persist(&record);
    *current = Some(record);
}

/// Marks a staging before the core's configuration can change.
#[allow(
    clippy::significant_drop_tightening,
    reason = "the file changes under the lock, in the order the record does"
)]
pub(crate) fn begin_applying() {
    RELOAD_PENDING.store(true, Ordering::Release);
    let mut current = CURRENT.lock();
    let Some(record) = current.as_mut() else {
        return;
    };
    record.applying = true;
    record.fingerprint = None;
    if let Err(error) = write(record) {
        logging!(
            warn,
            Type::Service,
            "failed to mark the service session as changing: {error:#}"
        );
        // The record on disk still claims a configuration the core is about to leave.
        remove_file();
    }
}

/// Whether a reload this app sent may still reach the core.
pub(crate) fn applying() -> bool {
    RELOAD_PENDING.load(Ordering::Acquire)
}

/// Ends a staging: the core runs `fingerprint`, or what it runs is uncertain. A process the
/// staging found replaced is recorded with it.
pub(crate) fn finish_applying(fingerprint: Option<String>, process: Option<CoreProcess>) {
    RELOAD_PENDING.store(false, Ordering::Release);
    update(|record| {
        record.applying = false;
        record.fingerprint = fingerprint;
        if process.is_some() {
            record.process = process;
        }
        true
    });
}

/// Notes whether the profile's node selections are still being put back on the core.
pub(crate) fn set_restoring_selections(restoring: bool) {
    update(|record| {
        let changed = record.restoring_selections != restoring;
        record.restoring_selections = restoring;
        changed
    });
}

/// Ends the session: its core stopped, or it is no longer this app's.
pub(crate) fn discard() {
    RELOAD_PENDING.store(false, Ordering::Release);
    forget();
}

/// Stops vouching for the running core, so that the next app starts a new one. The session and
/// what this app knows of its pending reloads stay.
#[allow(
    clippy::significant_drop_tightening,
    reason = "the file changes under the lock, in the order the record does"
)]
pub(crate) fn forget() {
    let mut current = CURRENT.lock();
    current.take();
    remove_file();
}

/// The record a previous app left behind, when it is readable, private and well-formed; anything
/// else is removed.
pub(crate) fn left_behind() -> Option<SessionRecord> {
    let path = record_path().ok()?;
    if !path.exists() {
        return None;
    }
    match read(&path) {
        Ok(record) if record.is_well_formed() => Some(record),
        Ok(_) => {
            logging!(warn, Type::Service, "discarding a malformed service session record");
            remove_file();
            None
        }
        Err(error) => {
            logging!(
                warn,
                Type::Service,
                "discarding an unreadable service session record: {error:#}"
            );
            remove_file();
            None
        }
    }
}

/// Changes the record, saving it when `change` reports a change.
fn update(change: impl FnOnce(&mut SessionRecord) -> bool) {
    let mut current = CURRENT.lock();
    if let Some(record) = current.as_mut()
        && change(record)
    {
        persist(record);
    }
}

/// A failed write leaves the app running; the next app then restarts the core.
fn persist(record: &SessionRecord) {
    if let Err(error) = write(record) {
        logging!(warn, Type::Service, "failed to save the service session: {error:#}");
        remove_file();
    }
}

fn record_path() -> Result<PathBuf> {
    Ok(dirs::app_home_dir()?.join(RECORD_FILE))
}

/// Replaces the record whole, so an app that ends midway leaves the old or the new one. A machine
/// that loses power loses the core too, so nothing is flushed.
fn write(record: &SessionRecord) -> Result<()> {
    let path = record_path()?;
    let temp = path.with_extension("json.tmp");
    if temp.exists() {
        std::fs::remove_file(&temp).with_context(|| format!("failed to remove {temp:?}"))?;
    }
    let mut file = create_private(&temp)?;
    file.write_all(&serde_json::to_vec(record)?)
        .with_context(|| format!("failed to write {temp:?}"))?;
    drop(file);
    std::fs::rename(&temp, &path).with_context(|| format!("failed to replace {path:?}"))
}

fn remove_file() {
    let Ok(path) = record_path() else {
        return;
    };
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => logging!(warn, Type::Service, "failed to remove {path:?}: {error}"),
    }
}

#[cfg(unix)]
fn create_private(path: &std::path::Path) -> Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("failed to create {path:?}"))
}

#[cfg(windows)]
fn create_private(path: &std::path::Path) -> Result<std::fs::File> {
    crate::core::owner_identity::create_private_current_user_file(path)
}

#[cfg(unix)]
fn read(path: &std::path::Path) -> Result<SessionRecord> {
    use std::os::unix::fs::MetadataExt as _;
    let mut file = std::fs::File::open(path).with_context(|| format!("failed to open {path:?}"))?;
    let metadata = file.metadata()?;
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
        anyhow::bail!("{path:?} is not private to the current user");
    }
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(serde_json::from_str(&text)?)
}

#[cfg(windows)]
fn read(path: &std::path::Path) -> Result<SessionRecord> {
    let mut file = crate::core::owner_identity::open_private_current_user_file(path)?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(serde_json::from_str(&text)?)
}

/// What identifies a runtime bundle and its start-only settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeDigests {
    /// `None` when an asset's identity cannot be read, so the runtime cannot be recognized later.
    pub runtime: Option<String>,
    pub startup: String,
}

/// Digests `bundle`, reading its assets' sizes and modification times as they are now. The
/// Service reads the assets itself later, so a digest is trusted only when taken again after the
/// Service answered and still equal.
pub(crate) fn digests(bundle: &RuntimeBundle) -> Result<RuntimeDigests> {
    Ok(RuntimeDigests {
        runtime: runtime_digest(bundle),
        startup: startup_digest(&bundle.yaml)?,
    })
}

fn runtime_digest(bundle: &RuntimeBundle) -> Option<String> {
    #[derive(Serialize)]
    struct Asset<'a> {
        destination: &'a str,
        source: &'a str,
        len: u64,
        modified_ns: u64,
    }
    #[derive(Serialize)]
    struct Provider<'a> {
        destination: &'a str,
        url: &'a str,
    }
    #[derive(Serialize)]
    struct Runtime<'a> {
        yaml: &'a str,
        assets: Vec<Asset<'a>>,
        remote_providers: Vec<Provider<'a>>,
        core_path: &'a str,
    }

    let mut assets = bundle
        .assets
        .iter()
        .map(|asset| {
            let metadata = std::fs::metadata(&asset.source).ok()?;
            let modified = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
            Some(Asset {
                destination: &asset.destination,
                source: &asset.source,
                len: metadata.len(),
                modified_ns: u64::try_from(modified.as_nanos()).ok()?,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    assets.sort_by(|a, b| a.destination.cmp(b.destination).then_with(|| a.source.cmp(b.source)));
    let mut remote_providers: Vec<_> = bundle
        .remote_providers
        .iter()
        .map(|provider| Provider {
            destination: &provider.destination,
            url: &provider.url,
        })
        .collect();
    remote_providers.sort_by(|a, b| a.destination.cmp(b.destination).then_with(|| a.url.cmp(b.url)));
    let encoded = serde_json::to_vec(&Runtime {
        yaml: &bundle.yaml,
        assets,
        remote_providers,
        core_path: &bundle.core_path,
    })
    .ok()?;
    Some(hex(&Sha256::digest(encoded)))
}

/// Besides the controller keys, whether the log level is debug: mihomo then serves `/debug`
/// without the secret. Other log levels reload.
fn startup_digest(yaml: &str) -> Result<String> {
    #[derive(Serialize)]
    struct Startup<'a> {
        fields: BTreeMap<&'a str, String>,
        debug: bool,
    }

    let config: Value = serde_yaml_ng::from_str(yaml).context("failed to parse the runtime config")?;
    // mihomo resolves merge keys, so settings inherited through `<<` take effect as well.
    let config = resolve_merges(config);
    let config = config.as_mapping().context("the runtime config is not a mapping")?;
    let mut fields = BTreeMap::new();
    for key in STARTUP_KEYS {
        let value = match config.get(*key) {
            Some(value) => serde_yaml_ng::to_string(value)?,
            None => String::new(),
        };
        fields.insert(*key, value);
    }
    let debug = config
        .get("log-level")
        .and_then(Value::as_str)
        .is_some_and(|level| level.eq_ignore_ascii_case("debug"));
    Ok(hex(&Sha256::digest(serde_json::to_vec(&Startup { fields, debug })?)))
}

/// Resolves `<<` merge keys bottom-up, nested sources included: keys of the mapping itself win,
/// then the sources in order.
fn resolve_merges(value: Value) -> Value {
    match value {
        Value::Mapping(mapping) => {
            let mut resolved = Mapping::new();
            let mut sources = Vec::new();
            for (key, value) in mapping {
                let value = resolve_merges(value);
                if key.as_str() == Some("<<") {
                    match value {
                        Value::Mapping(source) => sources.push(source),
                        Value::Sequence(items) => sources.extend(items.into_iter().filter_map(|item| match item {
                            Value::Mapping(source) => Some(source),
                            _ => None,
                        })),
                        _ => {}
                    }
                } else {
                    resolved.insert(key, value);
                }
            }
            for source in sources {
                for (key, value) in source {
                    resolved.entry(key).or_insert(value);
                }
            }
            Value::Mapping(resolved)
        }
        Value::Sequence(items) => Value::Sequence(items.into_iter().map(resolve_merges).collect()),
        // mihomo's decoder takes a tagged value for its content, merge sources included.
        Value::Tagged(tagged) => resolve_merges(tagged.value),
        other => other,
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Why a core the Service still runs for this app's session is not adopted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// A reload the previous app sent may still reach the core.
    Applying,
    /// The Service no longer runs the core for the recorded session.
    NotRunning,
    /// Another core is selected.
    CoreSwitched,
    /// The core binary is not the one a restart would run.
    CoreReplaced,
    /// Start-only settings changed, which only a restart applies.
    StartupChanged,
}

/// What the app sees of the Service, the cores and its own settings when it starts.
#[derive(Debug, Clone)]
pub(crate) struct Observed {
    pub active_generation: Option<u64>,
    pub running: bool,
    pub core: String,
    pub running_version: Option<String>,
    pub bundled_version: Option<String>,
    pub startup: String,
}

/// Whether the core of `record` can be adopted as it runs.
pub(crate) fn admit(record: &SessionRecord, observed: &Observed) -> Result<(), Refusal> {
    if record.applying {
        return Err(Refusal::Applying);
    }
    if observed.active_generation != Some(record.generation) || !observed.running {
        return Err(Refusal::NotRunning);
    }
    if observed.core != record.core {
        return Err(Refusal::CoreSwitched);
    }
    if !same_version(observed.running_version.as_deref(), observed.bundled_version.as_deref()) {
        return Err(Refusal::CoreReplaced);
    }
    if observed.startup != record.startup {
        return Err(Refusal::StartupChanged);
    }
    Ok(())
}

/// Fork core versions are unique, so equal versions are the same binary. An unknown version is
/// never equal.
pub(crate) fn same_version(running: Option<&str>, bundled: Option<&str>) -> bool {
    match (running, bundled) {
        (Some(running), Some(bundled)) => running.trim_start_matches('v') == bundled.trim_start_matches('v'),
        _ => false,
    }
}

/// Whether the adopted core must reload: what it loaded is unknown, or differs from `runtime`.
pub(crate) fn needs_reload(record: &SessionRecord, runtime: Option<&str>) -> bool {
    match (record.fingerprint.as_deref(), runtime) {
        (Some(loaded), Some(runtime)) => loaded != runtime,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> Result<SessionRecord> {
        Ok(SessionRecord::new(
            7,
            "ab".repeat(32),
            "verge-mihomo".into(),
            Some(CoreProcess {
                pid: 10,
                started_at: 20,
            }),
            startup_digest("secret: a\n")?,
            Some("loaded".into()),
        ))
    }

    fn observed() -> Result<Observed> {
        Ok(Observed {
            active_generation: Some(7),
            running: true,
            core: "verge-mihomo".into(),
            running_version: Some("v1.19.34-custom.2".into()),
            bundled_version: Some("1.19.34-custom.2".into()),
            startup: startup_digest("secret: a\n")?,
        })
    }

    fn admit_with(change: impl FnOnce(&mut Observed)) -> Result<Result<(), Refusal>> {
        let mut seen = observed()?;
        change(&mut seen);
        Ok(admit(&record()?, &seen))
    }

    #[test]
    fn a_matching_core_is_admitted() -> Result<()> {
        assert_eq!(admit_with(|_| {})?, Ok(()));
        Ok(())
    }

    #[test]
    fn each_mismatch_refuses_adoption() -> Result<()> {
        let mut applying = record()?;
        applying.applying = true;
        assert_eq!(admit(&applying, &observed()?), Err(Refusal::Applying));

        assert_eq!(
            admit_with(|seen| seen.active_generation = Some(8))?,
            Err(Refusal::NotRunning)
        );
        assert_eq!(admit_with(|seen| seen.running = false)?, Err(Refusal::NotRunning));
        assert_eq!(
            admit_with(|seen| seen.core = "verge-mihomo-alpha".into())?,
            Err(Refusal::CoreSwitched)
        );
        assert_eq!(
            admit_with(|seen| seen.bundled_version = Some("v1.19.35-custom.1".into()))?,
            Err(Refusal::CoreReplaced)
        );
        assert_eq!(
            admit_with(|seen| seen.running_version = None)?,
            Err(Refusal::CoreReplaced)
        );
        let other = startup_digest("secret: b\n")?;
        assert_eq!(admit_with(|seen| seen.startup = other)?, Err(Refusal::StartupChanged));
        Ok(())
    }

    #[test]
    fn only_entering_or_leaving_debug_changes_the_startup_digest() -> Result<()> {
        let info = startup_digest("log-level: info\nmode: rule\n")?;
        assert_eq!(info, startup_digest("log-level: warning\nmode: global\n")?);
        assert_ne!(info, startup_digest("log-level: debug\nmode: rule\n")?);
        assert_ne!(
            info,
            startup_digest("log-level: info\nexternal-controller: 127.0.0.1:9090\n")?
        );
        Ok(())
    }

    #[test]
    fn settings_inherited_through_merge_keys_count() -> Result<()> {
        let explicit = startup_digest("external-controller-tls: 127.0.0.1:9443\n")?;
        assert_eq!(
            explicit,
            startup_digest("<<: {external-controller-tls: 127.0.0.1:9443}\n")?
        );
        assert_ne!(
            explicit,
            startup_digest("<<: {external-controller-tls: 127.0.0.1:9444}\n")?
        );
        assert_eq!(
            startup_digest("log-level: debug\n")?,
            startup_digest("<<: {log-level: info}\nlog-level: debug\n")?
        );
        assert_ne!(
            startup_digest("log-level: info\n")?,
            startup_digest("<<: {log-level: debug}\n")?
        );
        Ok(())
    }

    #[test]
    fn nested_and_listed_merge_sources_resolve_like_mihomo() -> Result<()> {
        let explicit = startup_digest("external-controller-tls: 127.0.0.1:9443\n")?;
        let nested = "<<:\n  <<:\n    external-controller-tls: 127.0.0.1:9443\n";
        assert_eq!(explicit, startup_digest(nested)?);
        assert_ne!(explicit, startup_digest(&nested.replace("9443", "9444"))?);
        assert_ne!(explicit, startup_digest("<<:\n  <<: {}\n")?);
        let listed =
            "<<:\n  - {external-controller-tls: 127.0.0.1:9443}\n  - {external-controller-tls: 127.0.0.1:9444}\n";
        assert_eq!(explicit, startup_digest(listed)?);
        Ok(())
    }

    #[test]
    fn tagged_merge_sources_count() -> Result<()> {
        let explicit = startup_digest(
            "external-controller-tls: 127.0.0.1:9443
",
        )?;
        for tagged in [
            "<<:
  <<: !cfg
    external-controller-tls: 127.0.0.1:9443
",
            "<<: !cfg
  - {external-controller-tls: 127.0.0.1:9443}
",
            "<<:
  - !cfg {external-controller-tls: 127.0.0.1:9443}
",
        ] {
            assert_eq!(explicit, startup_digest(tagged)?);
            assert_ne!(explicit, startup_digest(&tagged.replace("9443", "9444"))?);
        }
        Ok(())
    }

    #[test]
    fn an_uncertain_or_different_runtime_reloads() -> Result<()> {
        let mut loaded = record()?;
        assert!(!needs_reload(&loaded, Some("loaded")));
        assert!(needs_reload(&loaded, Some("other")));
        assert!(needs_reload(&loaded, None));
        loaded.fingerprint = None;
        assert!(needs_reload(&loaded, Some("loaded")));
        Ok(())
    }

    #[test]
    fn the_runtime_digest_ignores_declaration_order() {
        use clash_verge_service_ipc::RemoteProvider;
        let provider = |destination: &str| RemoteProvider {
            destination: destination.into(),
            url: format!("https://example.com/{destination}"),
        };
        let bundle = |providers: Vec<RemoteProvider>| RuntimeBundle {
            yaml: "mode: rule\n".into(),
            assets: Vec::new(),
            remote_providers: providers,
            core_path: "/core".into(),
        };
        let forward = runtime_digest(&bundle(vec![provider("a"), provider("b")]));
        assert!(forward.is_some());
        assert_eq!(forward, runtime_digest(&bundle(vec![provider("b"), provider("a")])));
        assert_ne!(forward, runtime_digest(&bundle(vec![provider("a")])));
    }

    #[test]
    fn a_record_with_a_malformed_token_is_rejected() -> Result<()> {
        assert!(record()?.is_well_formed());
        let mut short = record()?;
        short.token = "ab".into();
        assert!(!short.is_well_formed());
        let mut upper = record()?;
        upper.token = "AB".repeat(32);
        assert!(!upper.is_well_formed());
        Ok(())
    }
}
