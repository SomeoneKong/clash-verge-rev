//! Keeps the traffic the core counts per proxy, collected into `traffic/` in the app data
//! directory: every minute while a core runs, and from a core about to be stopped or replaced.
//! The decision records of the core's probe strategy are collected along, into `probes/`.
//!
//! The store is owned by whoever holds `STORE`; its file work runs in `block_in_place`, and a
//! collection runs as a task of its own, so a caller with a time budget moves on when the
//! budget is spent while the collection still finishes its writes in order. The usage is
//! collected first and its result kept apart, then the decision records: their requests hold
//! no store, and `PROBES` only while their writes are made.

use std::{
    path::{Path, PathBuf},
    sync::{
        LazyLock,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context as _, Result, bail};
use clash_verge_logging::{Type, logging};
use clash_verge_usage::{
    CoreBucket, CoreEntry, CoreUsage, Identity, InitParams, PROBE_PAGE_SIZE, PROBE_PAGES, ProbeStore, Route, RouteKind,
    SeqGapReason, Store, UsageQuery, UsageReport,
};
use tauri_plugin_mihomo::models::{ProxyUsage, ProxyUsageKind};
use tokio::{sync::Mutex, time::Instant};

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

static PROBES: LazyLock<Mutex<Option<ProbeStore>>> = LazyLock::new(|| Mutex::new(None));
/// One collection of the decision records at a time, the periodic one and a refresh alike.
static PROBE_COLLECTION: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
/// Cores noted as stopped in the usage store, waiting to be noted in the decision records.
static PROBE_STOPS: LazyLock<parking_lot::Mutex<Vec<String>>> = LazyLock::new(Default::default);
/// The statistics day the decision records were last trimmed to their retention on.
static RETAINED_DAY: AtomicI64 = AtomicI64::new(i64::MIN);

pub fn start() {
    AsyncHandler::spawn(|| async {
        tokio::time::sleep(FIRST_COLLECTION_DELAY).await;
        loop {
            if core_running() && !handle::Handle::global().is_exiting() {
                collect_within(PERIODIC_COLLECTION, false).await;
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
/// Returns once the decision records are collected too, or the budget is spent: stopping the
/// core earlier would race their requests.
pub async fn before_core_ends(budget: Duration) -> PendingStop {
    if SKIP_FINAL_COLLECTIONS.load(Ordering::Relaxed) {
        return PendingStop(None);
    }
    let collected = collect_within(budget, true).await;
    if let Some(instance) = &collected {
        PENDING_STOPS.lock().push((instance.clone(), false));
    }
    PendingStop(collected)
}

/// Notes the ends of stopped cores still waiting, before the app leaves.
pub async fn finish() {
    if !PENDING_STOPS.lock().iter().any(|(_, confirmed)| *confirmed) && PROBE_STOPS.lock().is_empty() {
        return;
    }
    let task = tauri::async_runtime::spawn(async {
        let mut slot = STORE.lock().await;
        let store = blocking(|| open_store(&mut slot))?;
        let day_offset = store.meta().day_offset;
        blocking(|| note_stops(store, None).map(drop))?;
        drop(slot);
        let mut probes = PROBES.lock().await;
        blocking(|| {
            let probes = open_probes(&mut probes, day_offset)?;
            note_probe_changes(probes)?;
            probes.give_up_ended()
        })
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
            PROBE_STOPS.lock().push(instance);
        }
    }
    Ok(true)
}

/// Notes in the decision records the stops of cores the usage noted; each leaves the queue
/// only once it is on disk.
fn note_probe_changes(store: &mut ProbeStore) -> Result<()> {
    let stops = PROBE_STOPS.lock().clone();
    for instance in stops {
        store.mark_stopped(&instance)?;
        PROBE_STOPS.lock().retain(|pending| *pending != instance);
    }
    Ok(())
}

/// Collects before the app leaves a core running for the next app to take over.
pub async fn before_app_exit() {
    if !SKIP_FINAL_COLLECTIONS.load(Ordering::Relaxed) {
        collect_within(FINAL_COLLECTION, true).await;
    }
}

/// The session is ending and the core has to stop within the time the system gives.
pub fn skip_final_collections() {
    SKIP_FINAL_COLLECTIONS.store(true, Ordering::Relaxed);
}

pub async fn query(request: UsageQuery) -> Result<UsageReport> {
    if core_running() {
        collect_within(FINAL_COLLECTION, false).await;
    }
    let mut slot = STORE.lock().await;
    blocking(|| open_store(&mut slot)?.query(request, now_hour()))
}

/// Exports the usage to `path`, and beside it the decision records, as `<name>.probes.jsonl`,
/// and what explains them, as `<name>.probes-meta.json`.
pub async fn export(path: &Path) -> Result<()> {
    let exported_at = chrono::Utc::now().to_rfc3339();
    let mut slot = STORE.lock().await;
    let store = blocking(|| open_store(&mut slot))?;
    let (client, day_offset) = (store.meta().client.clone(), store.meta().day_offset);
    blocking(|| store.export(path, &exported_at))?;
    // the usage collections need not wait for the decision records
    drop(slot);
    let (events_path, meta_path) = probe_export_paths(path);
    let mut probes = PROBES.lock().await;
    blocking(|| {
        let probes = open_probes(&mut probes, day_offset)?;
        note_probe_changes(probes)?;
        probes.export(&events_path, &meta_path, &client, &exported_at)
    })
}

fn probe_export_paths(path: &Path) -> (PathBuf, PathBuf) {
    let stem = path
        .file_stem()
        .map_or_else(|| "usage".to_owned(), |stem| stem.to_string_lossy().into_owned());
    let dir = path.parent().unwrap_or_else(|| Path::new(""));
    (
        dir.join(format!("{stem}.probes.jsonl")),
        dir.join(format!("{stem}.probes-meta.json")),
    )
}

fn blocking<T>(operation: impl FnOnce() -> T) -> T {
    tokio::task::block_in_place(operation)
}

fn core_running() -> bool {
    !matches!(*CoreManager::global().get_running_mode(), RunningMode::NotRunning)
}

/// The instance the usage was collected from, or `None` when nothing was or the budget ran out
/// first; then the collection goes on in the background. The decision records are collected
/// after the usage, whose result does not wait for them; with `wait_records`, this returns once
/// they are collected or the budget is spent.
async fn collect_within(budget: Duration, wait_records: bool) -> Option<String> {
    let deadline = tokio::time::Instant::now() + budget;
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let task = tauri::async_runtime::spawn(async move {
        let _ = sender.send(collect().await);
        if let Err(error) = maintain_probes().await {
            logging!(warn, Type::Core, "Failed to maintain probe records: {error:#}");
        }
        if let Err(error) = collect_probes(deadline).await {
            logging!(debug, Type::Core, "Failed to collect probe records: {error:#}");
        }
    });
    let usage = tokio::time::timeout_at(deadline, receiver).await;
    if wait_records {
        let _ = tokio::time::timeout_at(deadline, task).await;
    }
    match usage {
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

/// Keeps the decision records up to date with what the usage knows of the cores, which is
/// on disk and so holds across restarts, and to their retention once a day, whether or not the
/// core has any records to collect.
async fn maintain_probes() -> Result<()> {
    let day_offset = day_offset().await?;
    let current = {
        let mut slot = STORE.lock().await;
        blocking(|| {
            let store = open_store(&mut slot)?;
            // stops confirmed while no collection ran, as when the next core failed to start
            note_stops(store, None)?;
            Ok::<_, anyhow::Error>(store.state().instance.as_ref().map(|current| current.id.clone()))
        })?
    };
    with_probes(day_offset, |store| {
        note_probe_changes(store)?;
        if let Some(current) = &current {
            store.end_before(current)?;
        }
        // Which cores ended is what the usage found, not which core a collection of events
        // read: one may have been overtaken by one reading the next core.
        store.give_up_ended()?;
        let today = (now_hour() + day_offset).div_euclid(24);
        if RETAINED_DAY.load(Ordering::Relaxed) != today {
            store.retain(today)?;
            RETAINED_DAY.store(today, Ordering::Relaxed);
        }
        Ok(())
    })
    .await
}

/// What a collection of the decision records came to.
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ProbeRefresh {
    /// `caughtUp` when every event the core had when asked is saved, `partial` when the
    /// budget ran out first or damage found meanwhile is to be read again, `changed` when the
    /// core restarted or dropped events meanwhile,
    /// `busy` when another collection took the budget, `unsupported` for a core without
    /// records, `unreachable` when no core answered in time.
    pub status: String,
    pub instance: Option<String>,
    /// The latest event the core had when asked, and the events saved up to it.
    pub observed_latest: Option<u64>,
    pub saved_to: Option<u64>,
    /// Events still to be read up to `observed_latest`, those read again for a damaged day
    /// file included.
    pub remaining: Option<u64>,
}

impl ProbeRefresh {
    fn status(status: &str) -> Self {
        Self {
            status: status.to_owned(),
            instance: None,
            observed_latest: None,
            saved_to: None,
            remaining: None,
        }
    }
}

/// Collects the decision records right away, as the periodic collection does and within its
/// budget, waiting for another collection included. What was started is stored all the same
/// once the budget is spent.
pub async fn refresh_probes() -> Result<ProbeRefresh> {
    let deadline = Instant::now() + PERIODIC_COLLECTION;
    let (sender, receiver) = tokio::sync::oneshot::channel();
    tauri::async_runtime::spawn(async move {
        if let Err(error) = maintain_probes().await {
            logging!(warn, Type::Core, "Failed to maintain probe records: {error:#}");
        }
        let _ = sender.send(collect_probes(deadline).await);
    });
    // the writes of the last page read before the deadline
    match tokio::time::timeout_at(deadline + Duration::from_secs(5), receiver).await {
        Ok(Ok(result)) => result,
        _ => Ok(ProbeRefresh::status("busy")),
    }
}

/// The decision records with a display a query asks for.
pub async fn probe_history(query: clash_verge_usage::HistoryQuery) -> Result<clash_verge_usage::HistoryPage> {
    if let Err(error) = maintain_probes().await {
        logging!(warn, Type::Core, "Failed to maintain probe records: {error:#}");
    }
    let day_offset = day_offset().await?;
    with_probes(day_offset, |store| store.history(&query)).await
}

/// Collects the decision records the reachable core has not reported yet, and the contexts
/// they refer to. A core without them is left alone. No request starts after `deadline`; what
/// was read is stored all the same.
async fn collect_probes(deadline: Instant) -> Result<ProbeRefresh> {
    let Ok(_collection) = tokio::time::timeout_at(deadline, PROBE_COLLECTION.lock()).await else {
        return Ok(ProbeRefresh::status("busy"));
    };
    if Instant::now() >= deadline {
        return Ok(ProbeRefresh::status("busy"));
    }
    let mihomo = handle::Handle::mihomo();
    // An empty page tells the instance and what the core keeps.
    let Some(head) = within(deadline, mihomo.get_probe_events(u64::MAX, 1)).await else {
        return Ok(ProbeRefresh::status("unreachable"));
    };
    let Some(head) = head? else {
        return Ok(ProbeRefresh::status("unsupported"));
    };
    let instance = head.instance.clone();
    let day_offset = day_offset().await?;
    let observed_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let plan = with_probes(day_offset, |store| {
        note_probe_changes(store)?;
        store.observe(&instance, head.latest, &observed_at)?;
        store.begin(&instance, &head.started_at, head.oldest)
    })
    .await?;
    // One budget of pages for reading corrupt ranges again and reading on.
    let mut pages = PROBE_PAGES;
    let reads = plan
        .refetch
        .iter()
        .map(|range| (range[0].saturating_sub(1), range[1]))
        .chain(std::iter::once((plan.after, head.latest)));
    let mut paging = PagingEnd::Done;
    let mut remaining = 0;
    for (after, until) in reads {
        if paging != PagingEnd::Done {
            remaining += until.saturating_sub(after);
            continue;
        }
        let (end, reached) = read_probe_events(&instance, after, until, day_offset, &mut pages, deadline).await?;
        paging = end;
        if end != PagingEnd::Done {
            remaining += until.saturating_sub(reached);
        }
    }
    let (saved_to, damaged) = with_probes(day_offset, |store| {
        let entry = store.meta().instances.get(&instance);
        // damage a query found meanwhile, which the core still has
        let damaged: u64 = entry.map_or(0, |entry| {
            entry
                .gaps
                .iter()
                .filter(|gap| gap.reason == SeqGapReason::Corrupt)
                .map(|gap| {
                    let (from, to) = (gap.from.max(head.oldest), gap.to.min(head.latest));
                    if from <= to { to - from + 1 } else { 0 }
                })
                .sum()
        });
        Ok((entry.map_or(0, |entry| entry.cursor), damaged))
    })
    .await?;
    if paging == PagingEnd::Done && damaged > 0 {
        paging = PagingEnd::Budget;
        remaining += damaged;
    }
    let refresh = ProbeRefresh {
        status: match paging {
            PagingEnd::Done => "caughtUp",
            PagingEnd::Budget => "partial",
            PagingEnd::Changed => "changed",
        }
        .to_owned(),
        instance: Some(instance.clone()),
        observed_latest: Some(head.latest),
        saved_to: Some(saved_to),
        remaining: Some(remaining),
    };

    let missing = with_probes(day_offset, |store| store.missing_contexts(&instance)).await?;
    if missing.is_empty() {
        return Ok(refresh);
    }
    let Some(response) = within(
        deadline,
        mihomo.get_probe_contexts(&missing.configs, &missing.exclusions),
    )
    .await
    else {
        return Ok(refresh);
    };
    // a core replaced between the requests numbers its versions anew
    if let Some(response) = response?
        && response.instance == instance
    {
        let unavailable: Vec<(String, u64)> = response
            .unavailable
            .iter()
            .map(|context| (context.kind.clone(), context.version))
            .collect();
        with_probes(day_offset, |store| {
            store.save_contexts(&instance, &response.contexts, &unavailable)
        })
        .await?;
    }
    Ok(refresh)
}

/// A request of the collection of events, `None` when the budget is spent before it starts or
/// before it is answered.
async fn within<T>(deadline: Instant, request: impl Future<Output = T>) -> Option<T> {
    if Instant::now() >= deadline {
        return None;
    }
    tokio::time::timeout_at(deadline, request).await.ok()
}

/// Why reading pages of events stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PagingEnd {
    /// The range was read, as far as the core had it.
    Done,
    /// The pages or the time of this collection are spent.
    Budget,
    /// The core changed or dropped events meanwhile: the next collection takes note of it.
    Changed,
}

/// Reads the events of `instance` after `after`, up to `until` or the core's latest at the
/// first page, taking the pages from `pages`; with why it stopped and the event it read up to.
/// Each page answered by the instance is an observation of it.
async fn read_probe_events(
    instance: &str,
    mut after: u64,
    until: u64,
    day_offset: i64,
    pages: &mut usize,
    deadline: Instant,
) -> Result<(PagingEnd, u64)> {
    let mihomo = handle::Handle::mihomo();
    let mut end = until;
    let mut first = true;
    while after < end {
        if *pages == 0 {
            return Ok((PagingEnd::Budget, after));
        }
        *pages -= 1;
        let Some(page) = within(deadline, mihomo.get_probe_events(after, PROBE_PAGE_SIZE)).await else {
            return Ok((PagingEnd::Budget, after));
        };
        let Some(page) = page? else {
            return Ok((PagingEnd::Changed, after));
        };
        if page.instance != instance {
            return Ok((PagingEnd::Changed, after));
        }
        let observed_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let latest = page.latest;
        with_probes(day_offset, |store| store.observe(instance, latest, &observed_at)).await?;
        if page.oldest > after + 1 {
            return Ok((PagingEnd::Changed, after));
        }
        if first {
            end = end.min(page.latest);
            first = false;
        }
        let events: Vec<serde_json::Value> = page
            .events
            .into_iter()
            .filter(|event| {
                event
                    .get("seq")
                    .and_then(serde_json::Value::as_u64)
                    .is_some_and(|seq| seq <= end)
            })
            .collect();
        let Some(last) = events
            .last()
            .and_then(|event| event.get("seq"))
            .and_then(serde_json::Value::as_u64)
        else {
            break;
        };
        let schema = page.schema;
        with_probes(day_offset, |store| {
            store.append(instance, schema, after, &events).map(drop)
        })
        .await?;
        after = last;
    }
    Ok((PagingEnd::Done, after))
}

async fn with_probes<T>(day_offset: i64, operation: impl FnOnce(&mut ProbeStore) -> Result<T>) -> Result<T> {
    let mut slot = PROBES.lock().await;
    blocking(|| operation(open_probes(&mut slot, day_offset)?))
}

fn open_probes(slot: &mut Option<ProbeStore>, day_offset: i64) -> Result<&mut ProbeStore> {
    if slot.is_none() {
        let dir = dirs::app_home_dir()?.join("probes");
        *slot = Some(ProbeStore::open(&dir, day_offset)?);
    }
    slot.as_mut().context("the probe record store is not open")
}

/// The statistics day offset of the usage, which the decision records share.
async fn day_offset() -> Result<i64> {
    static DAY_OFFSET: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    if let Some(offset) = DAY_OFFSET.get() {
        return Ok(*offset);
    }
    let mut slot = STORE.lock().await;
    let offset = blocking(|| open_store(&mut slot).map(|store| store.meta().day_offset))?;
    Ok(*DAY_OFFSET.get_or_init(|| offset))
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
    let routes = usage.routes;
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
                // a core that does not tell routes apart counts each proxy as a whole
                route: routes.then_some(Route {
                    kind: match entry.kind {
                        Some(ProxyUsageKind::Relay) => RouteKind::Relay,
                        _ => RouteKind::Exit,
                    },
                    groups: entry.groups,
                    dialers: entry.dialers,
                }),
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
