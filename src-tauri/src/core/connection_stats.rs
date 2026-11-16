//! Keeps the connection stats the core counts per host, process, rule and route, collected
//! into `traffic/connections/` while recording is on, along with the per-proxy usage.
//!
//! Recording is on only as the store's state says; the app writes it into the runtime config
//! and keeps the core's switch in line with it on every collection, whether or not anything is
//! collected. Turning recording off has the core stop counting first; what it counted before
//! is read in full before collecting stops.

use std::{
    collections::HashMap,
    sync::{
        LazyLock,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
};

use anyhow::{Context as _, Result, bail};
use clash_verge_logging::{Type, logging};
use clash_verge_usage::{ConnKey, ConnQuery, ConnReport, ConnStore, CoreConnBucket, CoreConnEntry, CoreConns};
use serde_yaml_ng::{Mapping, Value};
use tauri_plugin_mihomo::models::ConnectionUsage;
use tokio::{sync::Mutex, time::Instant};

use crate::{
    config::Config,
    core::{handle, proxy_usage},
    utils::dirs,
};

static CONNS: LazyLock<Mutex<Option<ConnStore>>> = LazyLock::new(|| Mutex::new(None));
/// Whether recording is on, as the store's state says; read before the store is opened, as
/// the runtime config is made early.
static RECORDING: LazyLock<AtomicBool> =
    LazyLock::new(|| AtomicBool::new(store_dir().is_ok_and(|dir| clash_verge_usage::recording_on_disk(&dir))));
/// Cores the usage noted as stopped, waiting to be noted in the connection stats, with the
/// final collection before the stop.
static STOPS: LazyLock<parking_lot::Mutex<Vec<(String, u64)>>> = LazyLock::new(Default::default);
/// Cores about to be stopped, with the final collection before.
static STOPPING: LazyLock<parking_lot::Mutex<HashMap<String, u64>>> = LazyLock::new(Default::default);
/// The statistics day old days were last dropped on.
static RETAINED_DAY: AtomicI64 = AtomicI64::new(i64::MIN);

/// A range of hours no request covers, which still tells the instance and its clock.
const HEAD_HOUR: i64 = 1 << 60;

fn store_dir() -> Result<std::path::PathBuf> {
    Ok(dirs::app_home_dir()?.join("traffic").join("connections"))
}

pub fn recording() -> bool {
    RECORDING.load(Ordering::Relaxed)
}

/// Notes that `instance` is about to be stopped after final collection `attempt`; only a
/// read in full by that collection, with nothing done to the store since, makes its end no
/// gap.
pub fn before_stop(instance: &str, attempt: u64) {
    STOPPING.lock().insert(instance.to_owned(), attempt);
}

/// Notes a core the usage found stopped after a final collection.
pub fn note_stop(instance: String) {
    // final collections are numbered from 1
    let attempt = STOPPING.lock().remove(&instance).unwrap_or(0);
    STOPS.lock().push((instance, attempt));
}

/// What the settings show.
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ConnStatsStatus {
    pub recording: bool,
    /// The core is still read up to where recording was turned off.
    pub finishing: bool,
    /// Recording was turned off as the state was lost with its backup.
    pub setting_lost: bool,
    /// Whether the running core counts connections, none when it cannot or did not answer.
    pub core_counting: Option<bool>,
    /// Anything was ever recorded, so the hour ranges can be queried.
    pub has_history: bool,
}

pub async fn status() -> Result<ConnStatsStatus> {
    let core_counting = handle::Handle::mihomo()
        .get_base_config()
        .await
        .ok()
        .and_then(|config| config.connection_stats);
    with_store(|store| {
        Ok(ConnStatsStatus {
            recording: store.recording(),
            finishing: store.finishing(),
            setting_lost: store.state().setting_lost,
            core_counting,
            has_history: !store.state().recording.is_empty() || store.state().coverage_unknown_before.is_some(),
        })
    })
    .await
}

/// Turns recording on or off: accepted once on disk, then put into the runtime config and
/// passed to the core; a core that does not take it is brought in line by the next collection.
pub async fn set_recording(on: bool) -> Result<()> {
    with_store(|store| store.set_recording(on, now_secs())).await?;
    RECORDING.store(on, Ordering::Relaxed);
    if let Err(error) = update_runtime_config(on).await {
        logging!(
            warn,
            Type::Core,
            "Failed to put connection stats into the runtime config: {error:#}"
        );
    }
    if let Err(error) = patch_core(on).await {
        logging!(
            debug,
            Type::Core,
            "The core did not take the connection stats switch yet: {error}"
        );
    }
    // the core is read up to the switch now rather than at the next collection
    crate::process::AsyncHandler::spawn(|| async {
        proxy_usage::collect_now().await;
    });
    Ok(())
}

async fn update_runtime_config(on: bool) -> Result<()> {
    let _config_write = Config::lock_config_write().await;
    let mut patch = Mapping::new();
    patch.insert(Value::from("connection-stats"), Value::from(on));
    let runtime = Config::runtime().await;
    runtime.edit_draft(|draft| draft.patch_config(&patch));
    runtime.apply();
    Config::generate_file().await.map(drop)
}

async fn patch_core(on: bool) -> Result<()> {
    handle::Handle::mihomo()
        .patch_base_config(&serde_json::json!({ "connection-stats": on }))
        .await?;
    Ok(())
}

pub async fn query(query: ConnQuery) -> Result<ConnReport> {
    with_store(|store| store.query(&query, now_secs())).await
}

pub async fn clear() -> Result<()> {
    with_store(|store| store.clear(now_secs())).await
}

/// Writes the connection stats beside an export of the usage, unless nothing was ever recorded.
pub async fn export(path: &std::path::Path, client: &str, exported_at: &str) -> Result<()> {
    with_store(|store| {
        if store.state().recording.is_empty() && store.state().coverage_unknown_before.is_none() {
            return Ok(());
        }
        store.export(path, client, exported_at, now_secs().div_euclid(3600))
    })
    .await
}

/// Saves what is pending and notes the cores found stopped, before the app leaves.
pub async fn finish() -> Result<()> {
    with_store(|store| {
        note_stops(store)?;
        store.flush(now_secs())
    })
    .await
}

/// Collects from the core the usage was just collected from, and keeps its switch in line
/// with the setting. A final collection, numbered `final_attempt`, reads every hour of the
/// core not appended yet, so its end, once confirmed, is no gap. No request starts after
/// `deadline`.
pub async fn collect(instance: &str, final_attempt: Option<u64>, deadline: Instant) -> Result<()> {
    let day_offset = proxy_usage::day_offset().await?;
    let mut slot = CONNS.lock().await;
    let result = collect_into(
        blocking(|| open(&mut slot, day_offset))?,
        instance,
        final_attempt,
        deadline,
    )
    .await;
    drop(slot);
    result
}

async fn collect_into(
    store: &mut ConnStore,
    instance: &str,
    final_attempt: Option<u64>,
    deadline: Instant,
) -> Result<()> {
    let final_read = final_attempt.is_some();
    blocking(|| note_stops(store))?;
    let mihomo = handle::Handle::mihomo();
    if !store.collecting() {
        // keeps a core with the switch turned elsewhere in line
        if let Some(Some(counting)) = within(deadline, mihomo.get_base_config())
            .await
            .map(|config| config.ok().and_then(|config| config.connection_stats))
            && counting != store.recording()
        {
            patch_core(store.recording()).await?;
        }
        blocking(|| maintain(store));
        return Ok(());
    }
    let Some(head) = within(deadline, mihomo.get_connection_usage(HEAD_HOUR, HEAD_HOUR, None)).await else {
        return Ok(());
    };
    let Some(head) = head? else {
        return Ok(()); // a core without connection stats
    };
    if head.instance != instance {
        bail!("the core changed after its usage was collected");
    }
    if head.enabled != store.recording() {
        patch_core(store.recording()).await?;
    }
    let head = core_conns(head)?;
    let now = now_secs();
    blocking(|| store.observe(&head, now))?;
    let result = read(store, &head, final_read, deadline).await;
    if result.is_err() {
        store.forget_baseline();
    }
    let complete = result?;
    if complete && let Some(attempt) = final_attempt {
        blocking(|| store.flush(now_secs()))?;
        store.read_in_full(&head.instance, attempt);
    }
    if complete && store.finishing() && !head.enabled {
        blocking(|| store.finish(&head.instance, now_secs()))?;
    }
    blocking(|| maintain(store));
    Ok(())
}

/// Reads the settled hours not appended yet, then the hours the core may still add to. A
/// final read or one finishing reads every settled batch, and the rest in full; otherwise a
/// collection reads one batch and the rest from the tick of the last read. Returns whether
/// everything was read.
async fn read(store: &mut ConnStore, head: &CoreConns, final_read: bool, deadline: Instant) -> Result<bool> {
    let mihomo = handle::Handle::mihomo();
    let in_full = final_read || store.finishing();
    let (mut settled, mut hour) = (head.settled, head.hour);
    let mut backlog = false;
    while let Some((since, until)) = store.settled_range(settled) {
        let Some(response) = within(deadline, mihomo.get_connection_usage(since, until, None)).await else {
            return Ok(false);
        };
        let core = checked(response?, &head.instance)?;
        blocking(|| store.apply_settled(&core, since, until, now_secs()))?;
        (settled, hour) = (settled.max(core.settled), hour.max(core.hour));
        if !in_full {
            backlog = store.settled_range(settled).is_some();
            break;
        }
    }
    // the core's clock may move on meanwhile: the new hour is read before the rest is whole
    for _ in 0..2 {
        let Some((since, until, touched)) = store.pending_request(settled, hour) else {
            return Ok(!backlog);
        };
        let touched = if in_full { None } else { touched };
        let Some(response) = within(deadline, mihomo.get_connection_usage(since, until, touched)).await else {
            return Ok(false);
        };
        let core = checked(response?, &head.instance)?;
        blocking(|| store.apply_pending(&core, since, until, now_secs()))?;
        if core.hour <= until {
            return Ok(!backlog);
        }
        hour = core.hour;
    }
    Ok(false)
}

fn checked(response: Option<ConnectionUsage>, instance: &str) -> Result<CoreConns> {
    let response = response.context("the core no longer has connection stats")?;
    if response.instance != instance {
        bail!("the core changed while its connection stats were being collected");
    }
    core_conns(response)
}

/// Marks the cores the usage noted as stopped; each leaves the queue once on disk. One not
/// read in full before is a gap once the next core is seen.
fn note_stops(store: &mut ConnStore) -> Result<()> {
    let stops = STOPS.lock().clone();
    for (instance, attempt) in stops {
        store.mark_stopped(&instance, attempt, now_secs())?;
        STOPS.lock().retain(|(pending, _)| *pending != instance);
    }
    Ok(())
}

/// Drops old days once a day and lets the cached hours go when queries stopped.
fn maintain(store: &mut ConnStore) {
    let now_hour = now_secs().div_euclid(3600);
    let today = now_hour.div_euclid(24);
    if RETAINED_DAY.load(Ordering::Relaxed) != today {
        match store.retain(now_hour) {
            Ok(()) => RETAINED_DAY.store(today, Ordering::Relaxed),
            Err(error) => logging!(warn, Type::Core, "Failed to drop old connection stats: {error:#}"),
        }
    }
    store.release_idle_cache(now_secs());
}

async fn within<T>(deadline: Instant, request: impl Future<Output = T>) -> Option<T> {
    if Instant::now() >= deadline {
        return None;
    }
    tokio::time::timeout_at(deadline, request).await.ok()
}

async fn with_store<T>(operation: impl FnOnce(&mut ConnStore) -> Result<T>) -> Result<T> {
    let day_offset = proxy_usage::day_offset().await?;
    let mut slot = CONNS.lock().await;
    let result = blocking(|| operation(open(&mut slot, day_offset)?));
    drop(slot);
    result
}

fn open(slot: &mut Option<ConnStore>, day_offset: i64) -> Result<&mut ConnStore> {
    if slot.is_none() {
        let store = ConnStore::open(&store_dir()?, day_offset, now_secs().div_euclid(3600))?;
        RECORDING.store(store.recording(), Ordering::Relaxed);
        *slot = Some(store);
    }
    slot.as_mut().context("the connection stats store is not open")
}

fn blocking<T>(operation: impl FnOnce() -> T) -> T {
    tokio::task::block_in_place(operation)
}

fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

fn core_conns(usage: ConnectionUsage) -> Result<CoreConns> {
    let started_at = chrono::DateTime::parse_from_rfc3339(&usage.started_at)
        .with_context(|| format!("invalid core start time {:?}", usage.started_at))?;
    Ok(CoreConns {
        instance: usage.instance,
        started_hour: started_at.timestamp().div_euclid(3600),
        hour: usage.hour,
        settled: usage.settled,
        retained_since: usage.retained_since,
        enabled: usage.enabled,
        tick: usage.tick,
        counting_on: usage.counting.on,
        counting_partial: usage.counting.partial,
        entries: usage
            .entries
            .into_iter()
            .map(|entry| CoreConnEntry {
                key: ConnKey {
                    in_type: entry.key.in_type,
                    in_name: entry.key.in_name,
                    in_user: entry.key.in_user,
                    src_ip: entry.key.src_ip,
                    process: entry.key.process,
                    target: entry.key.target,
                    target_is_ip: entry.key.target_is_ip,
                    network: entry.key.network,
                    rule: entry.key.rule,
                    rule_payload: entry.key.rule_payload,
                    provider: entry.key.provider,
                    chain: entry.key.chain,
                },
                buckets: entry
                    .buckets
                    .into_iter()
                    .map(|bucket| CoreConnBucket {
                        hour: bucket.hour,
                        up: bucket.up,
                        down: bucket.down,
                        conns: bucket.conns,
                    })
                    .collect(),
            })
            .collect(),
    })
}
