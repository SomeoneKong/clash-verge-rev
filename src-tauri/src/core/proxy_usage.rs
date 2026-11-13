//! Keeps the traffic the core counts per proxy, collected into `traffic/` in the app data
//! directory: every minute while a core runs, and from a core about to be stopped or replaced.
//!
//! The store is owned by whoever holds `STORE`; its file work runs in `block_in_place`, and a
//! collection runs as a task of its own, so a caller with a time budget moves on when the
//! budget is spent while the collection still finishes its writes in order.

use std::{
    path::Path,
    sync::{
        LazyLock,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context as _, Result, bail};
use clash_verge_logging::{Type, logging};
use clash_verge_usage::{CoreBucket, CoreEntry, CoreUsage, Identity, InitParams, Store, UsageQuery, UsageReport};
use tauri_plugin_mihomo::models::ProxyUsage;
use tokio::sync::Mutex;

use crate::{
    core::{
        handle,
        manager::{CoreManager, RunningMode},
    },
    process::AsyncHandler,
    utils::dirs,
};

/// How long a collection from a core about to end may hold up ending it.
pub const FINAL_COLLECTION: Duration = Duration::from_secs(3);
/// A core replaced when the app starts may hold hours no app has collected yet.
pub const REPLACEMENT_COLLECTION: Duration = Duration::from_secs(15);
const PERIODIC_COLLECTION: Duration = Duration::from_secs(30);
const FIRST_COLLECTION_DELAY: Duration = Duration::from_secs(10);
const COLLECTION_INTERVAL: Duration = Duration::from_secs(60);

static STORE: LazyLock<Mutex<Option<Store>>> = LazyLock::new(|| Mutex::new(None));
/// Cores a final collection read from, waiting to be noted in the store as stopped, with
/// whether their stop or replacement is confirmed yet.
static PENDING_STOPS: LazyLock<parking_lot::Mutex<Vec<(String, bool)>>> = LazyLock::new(Default::default);
static SKIP_FINAL_COLLECTIONS: AtomicBool = AtomicBool::new(false);
/// The statistics day the store was last archived on.
static ARCHIVED_DAY: AtomicI64 = AtomicI64::new(i64::MIN);

pub fn start() {
    AsyncHandler::spawn(|| async {
        tokio::time::sleep(FIRST_COLLECTION_DELAY).await;
        loop {
            if core_running() && !handle::Handle::global().is_exiting() {
                collect_within(PERIODIC_COLLECTION).await;
            }
            tokio::time::sleep(COLLECTION_INTERVAL).await;
        }
    });
}

/// The end of a core that a final collection read from. [`PendingStop::confirm`] it once the
/// core is gone; dropping it unconfirmed means the core kept running.
#[must_use]
pub struct PendingStop(Option<String>);

impl PendingStop {
    pub fn confirm(mut self) {
        if let Some(instance) = self.0.take() {
            for (pending, confirmed) in PENDING_STOPS.lock().iter_mut() {
                if *pending == instance {
                    *confirmed = true;
                }
            }
        }
    }
}

impl Drop for PendingStop {
    fn drop(&mut self) {
        if let Some(instance) = self.0.take() {
            PENDING_STOPS
                .lock()
                .retain(|(pending, confirmed)| *confirmed || *pending != instance);
        }
    }
}

/// Collects from the core about to be stopped or replaced. Its end is registered right away, so
/// that a collection seeing the next core, which can only start after this, notes it first.
pub async fn before_core_ends(budget: Duration) -> PendingStop {
    if SKIP_FINAL_COLLECTIONS.load(Ordering::Relaxed) {
        return PendingStop(None);
    }
    let collected = collect_within(budget).await;
    if let Some(instance) = &collected {
        PENDING_STOPS.lock().push((instance.clone(), false));
    }
    PendingStop(collected)
}

/// Notes the ends of stopped cores still waiting, before the app leaves.
pub async fn finish() {
    if !PENDING_STOPS.lock().iter().any(|(_, confirmed)| *confirmed) {
        return;
    }
    let task = tauri::async_runtime::spawn(async {
        let mut slot = STORE.lock().await;
        let store = blocking(|| open_store(&mut slot))?;
        blocking(|| note_stops(store, None).map(drop))
    });
    match tokio::time::timeout(FINAL_COLLECTION, task).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(error))) => logging!(warn, Type::Core, "Failed to record stopped cores: {error:#}"),
        Ok(Err(error)) => logging!(warn, Type::Core, "Recording stopped cores failed: {error}"),
        Err(_) => logging!(warn, Type::Core, "Recording stopped cores took too long"),
    }
}

/// Marks the cores whose stop is confirmed as stopped, except `running`, which a probe still
/// reached. Returns false, noting nothing, while the stop of a core other than `running` is not
/// confirmed yet: whether its end is a gap depends on that, so the core reached has to wait.
fn note_stops(store: &mut Store, running: Option<&str>) -> Result<bool> {
    let pending = PENDING_STOPS.lock().clone();
    let undecided = pending
        .iter()
        .any(|(instance, confirmed)| !confirmed && Some(instance.as_str()) != running);
    if running.is_some() && undecided {
        return Ok(false);
    }
    for (instance, confirmed) in pending {
        if confirmed && Some(instance.as_str()) != running {
            store.mark_stopped(&instance)?;
            PENDING_STOPS.lock().retain(|(pending, _)| *pending != instance);
        }
    }
    Ok(true)
}

/// Collects before the app leaves a core running for the next app to take over.
pub async fn before_app_exit() {
    if !SKIP_FINAL_COLLECTIONS.load(Ordering::Relaxed) {
        collect_within(FINAL_COLLECTION).await;
    }
}

/// The session is ending and the core has to stop within the time the system gives.
pub fn skip_final_collections() {
    SKIP_FINAL_COLLECTIONS.store(true, Ordering::Relaxed);
}

pub async fn query(request: UsageQuery) -> Result<UsageReport> {
    if core_running() {
        collect_within(FINAL_COLLECTION).await;
    }
    let mut slot = STORE.lock().await;
    blocking(|| open_store(&mut slot)?.query(request, now_hour()))
}

pub async fn export(path: &Path) -> Result<()> {
    let mut slot = STORE.lock().await;
    blocking(|| open_store(&mut slot)?.export(path, &chrono::Utc::now().to_rfc3339()))
}

fn blocking<T>(operation: impl FnOnce() -> T) -> T {
    tokio::task::block_in_place(operation)
}

fn core_running() -> bool {
    !matches!(*CoreManager::global().get_running_mode(), RunningMode::NotRunning)
}

/// The instance collected from, or `None` when nothing was or the budget ran out first; then
/// the collection goes on in the background.
async fn collect_within(budget: Duration) -> Option<String> {
    match tokio::time::timeout(budget, tauri::async_runtime::spawn(collect())).await {
        Ok(Ok(Ok(instance))) => instance,
        Ok(Ok(Err(error))) => {
            logging!(warn, Type::Core, "Failed to collect proxy usage: {error:#}");
            None
        }
        Ok(Err(error)) => {
            logging!(warn, Type::Core, "Collecting proxy usage failed: {error}");
            None
        }
        Err(_) => {
            logging!(
                warn,
                Type::Core,
                "Collecting proxy usage did not finish within {} s",
                budget.as_secs()
            );
            None
        }
    }
}

async fn collect() -> Result<Option<String>> {
    let mut slot = STORE.lock().await;
    let store = blocking(|| open_store(&mut slot))?;
    collect_into(store).await
}

/// Collects everything the reachable core has not reported yet, and returns its instance.
async fn collect_into(store: &mut Store) -> Result<Option<String>> {
    let mihomo = handle::Handle::mihomo();
    let probe = match mihomo.get_proxy_usage(0, -1).await {
        Ok(probe) => core_usage(probe)?,
        Err(error) => {
            logging!(debug, Type::Core, "No core to collect proxy usage from: {error}");
            return Ok(None);
        }
    };
    let now = chrono::Utc::now().timestamp();
    if !blocking(|| note_stops(store, Some(&probe.instance)))? {
        logging!(debug, Type::Core, "Proxy usage waits for a core stop to be confirmed");
        return Ok(None);
    }
    blocking(|| store.observe(&probe, now))?;
    while let Some((since, until)) = store.next_range() {
        let usage = core_usage(mihomo.get_proxy_usage(since, until).await?)?;
        if usage.instance != probe.instance {
            bail!("the core changed while its usage was being collected");
        }
        blocking(|| store.apply(&usage, until, now))?;
        // Done once the current hour is read and every settled hour is on disk; reading an
        // unreadable day file moves the progress back.
        let done = store.state().instance.as_ref().map_or(i64::MIN, |current| current.done);
        if until >= usage.hour && done >= usage.settled {
            break;
        }
    }
    blocking(|| archive_once_a_day(store))?;
    Ok(Some(probe.instance))
}

/// Archives after the first full collection of a run, then whenever the statistics day changes.
fn archive_once_a_day(store: &mut Store) -> Result<()> {
    let now_hour = now_hour();
    let today = (now_hour + store.meta().day_offset).div_euclid(24);
    if ARCHIVED_DAY.load(Ordering::Relaxed) != today {
        store.archive(now_hour, false)?;
        ARCHIVED_DAY.store(today, Ordering::Relaxed);
    }
    Ok(())
}

fn open_store(slot: &mut Option<Store>) -> Result<&mut Store> {
    if slot.is_none() {
        let dir = dirs::app_home_dir()?.join("traffic");
        *slot = Some(Store::open(&dir, init_params, now_hour())?);
    }
    slot.as_mut().context("the proxy usage store is not open")
}

fn init_params() -> InitParams {
    let offset = chrono::Local::now().offset().local_minus_utc();
    InitParams {
        client: client_id(),
        day_offset: i64::from(offset).div_euclid(3600),
        created_at: chrono::Utc::now().to_rfc3339(),
    }
}

/// A random version 4 UUID.
fn client_id() -> String {
    let mut bytes = [0_u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        return nanoid::nanoid!(32);
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

fn now_hour() -> i64 {
    chrono::Utc::now().timestamp().div_euclid(3600)
}

fn core_usage(usage: ProxyUsage) -> Result<CoreUsage> {
    let started_at = chrono::DateTime::parse_from_rfc3339(&usage.started_at)
        .with_context(|| format!("invalid core start time {:?}", usage.started_at))?;
    Ok(CoreUsage {
        instance: usage.instance,
        started_hour: started_at.timestamp().div_euclid(3600),
        hour: usage.hour,
        settled: usage.settled,
        retained_since: usage.retained_since,
        entries: usage
            .entries
            .into_iter()
            .map(|entry| CoreEntry {
                identity: Identity {
                    provider: entry.provider,
                    name: entry.name,
                    proxy_type: entry.proxy_type,
                    addr: entry.addr,
                    incomplete: entry.incomplete,
                },
                buckets: entry
                    .buckets
                    .into_iter()
                    .map(|bucket| CoreBucket {
                        hour: bucket.hour,
                        up: bucket.up,
                        down: bucket.down,
                    })
                    .collect(),
            })
            .collect(),
    })
}
