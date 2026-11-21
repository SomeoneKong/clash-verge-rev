use super::{
    Connection, Finished, Hello, Kind, Outcome, Request, Response, WindowAfterUpdate, automatic, choose, client,
    component_hello, repairable_here,
};
use crate::cmd::{CmdResult, CommandFailure, StringifyErr as _, WithErrorCode as _};
use anyhow::{Context as _, Result, ensure};
use clash_verge_updater::{audit::Record, automatic::Eligibility, version};
use serde::Serialize;

pub(crate) fn unattended_reply() -> impl warp::Reply {
    use clash_verge_logging::{Type, logging};
    match super::forward_unattended() {
        Ok(()) => warp::reply::with_status("ok".to_owned(), warp::http::StatusCode::OK),
        Err(error) => {
            logging!(warn, Type::System, "Queuing unattended update: {error:#}");
            warp::reply::with_status(
                "queue unavailable".to_owned(),
                warp::http::StatusCode::SERVICE_UNAVAILABLE,
            )
        }
    }
}

#[derive(Debug, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct UpdateOverview {
    auto: Option<AutoStatus>,
    app_repair: Option<RepairItem>,
    core_repair: Option<String>,
    next: NextStep,
}

#[derive(Debug, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct RepairItem {
    txn: String,
    version: String,
    available: bool,
}

#[derive(Debug, Serialize, specta::Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum NextStep {
    Busy,
    Unknown,
    Bootstrap,
    Component,
    MacComponent,
    Repair { version: String },
    FullRepair { version: String },
    Fallback { reason: String },
    NoPayload,
}

#[derive(Debug, Serialize, specta::Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum AutoStatus {
    Waiting { version: String, after: i64 },
    Confirm { version: String },
    Ready { version: String },
    Failed { version: String, reason: String },
    Unavailable { reason: String },
    Candidate { version: String },
}

fn auto_status(hello: Option<&Hello>) -> Option<AutoStatus> {
    let state = automatic::snapshot().ok()?;
    let now = automatic::now();
    let c = state.candidate?;
    let version = c.identity.version.clone();
    let last = state.last.filter(|r| r.acquisition == c.acquisition);
    let installed = hello.and_then(|h| h.installed_version.as_deref());
    match c.eligibility(now, installed) {
        Eligibility::Installed => None,
        Eligibility::Waiting(after) => Some(AutoStatus::Waiting { version, after }),
        Eligibility::Failed => Some(AutoStatus::Failed {
            version,
            reason: last.map(|r| r.reason).unwrap_or_default(),
        }),
        Eligibility::Unconfirmed => Some(AutoStatus::Confirm { version }),
        Eligibility::Ready => {
            if let Some(r) = last.filter(|r| r.result == "unavailable" && r.time >= c.deadline() && r.time <= now) {
                Some(AutoStatus::Unavailable { reason: r.reason })
            } else if state.attempt.is_none()
                && installed.is_some_and(|v| version::compare(&version, v).is_ok_and(|o| o.is_gt()))
                && hello.is_some_and(|h| {
                    !h.staged
                        && h.current.is_none()
                        && h.needs_repair.as_ref().is_none_or(|p| {
                            super::repair_attempted(&p.version) && automatic::newer_than_pending(&version, Some(p))
                        })
                })
                && c.downloaded
                && automatic::cached()
            {
                Some(AutoStatus::Ready { version })
            } else {
                Some(AutoStatus::Candidate { version })
            }
        }
    }
}

pub(super) fn bootstrap_available() -> bool {
    cfg!(target_os = "macos")
        && matches!(
            clash_verge_updater::paths::Paths::system().program().try_exists(),
            Ok(false)
        )
}

fn next_step(status: &Result<Hello>, payload: bool) -> NextStep {
    if super::take_gate().is_err() {
        return NextStep::Busy;
    }
    let h = match status {
        Ok(h) => h,
        Err(e) => {
            return match client::read_state() {
                Err(_) => NextStep::Unknown,
                Ok(s) if s.txn.is_some() => NextStep::Busy,
                Ok(_) if bootstrap_available() && payload => NextStep::Bootstrap,
                Ok(_) => NextStep::Fallback {
                    reason: format!("{e:#}"),
                },
            };
        }
    };
    if super::install_busy(h, super::TriggerKind::Manual) {
        NextStep::Busy
    } else if let Some(p) = h.needs_repair.as_ref().filter(|p| repairable_here(p)) {
        let version = p.version.clone();
        if p.kind == Kind::Full {
            NextStep::FullRepair { version }
        } else {
            NextStep::Repair { version }
        }
    } else if !payload {
        NextStep::NoPayload
    } else if cfg!(target_os = "macos") {
        NextStep::MacComponent
    } else {
        NextStep::Component
    }
}

#[derive(Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct CheckedRelease {
    rid: u32,
    current_version: String,
    version: String,
    body: Option<String>,
    #[specta(type = std::collections::HashMap<String, crate::ipc::JsonValue>)]
    raw_json: serde_json::Value,
}

#[tauri::command]
#[specta::specta]
pub async fn fork_check_update(
    webview: tauri::Webview,
    headers: Vec<(String, String)>,
    timeout: Option<u32>,
    proxy: Option<String>,
    target: Option<String>,
) -> CmdResult<Option<CheckedRelease>> {
    use tauri::Manager as _;
    use tauri_plugin_updater::UpdaterExt as _;
    let mut builder = webview
        .updater_builder()
        .header("Cache-Control", "no-cache")
        .stringify_err()?;
    for (name, value) in headers {
        builder = builder.header(name, value).stringify_err()?;
    }
    if let Some(ms) = timeout {
        builder = builder.timeout(std::time::Duration::from_millis(u64::from(ms)));
    }
    if let Some(url) = proxy {
        builder = builder.proxy(tauri::Url::parse(&url).stringify_err()?);
    }
    let local_target = target.is_none();
    if let Some(target) = target {
        builder = builder.target(target);
    }
    let update = automatic::inspect_offer(
        &builder.build().stringify_err()?,
        &tokio_util::sync::CancellationToken::new(),
        local_target,
    )
    .await
    .stringify_err()?;
    Ok(update.map(|u| CheckedRelease {
        current_version: u.current_version.clone(),
        version: u.version.clone(),
        body: u.body.clone(),
        raw_json: u.raw_json.clone(),
        rid: webview.resources_table().add(u),
    }))
}

#[derive(serde::Deserialize, specta::Type)]
#[serde(transparent)]
pub struct ReleaseMetadata(#[specta(type = crate::ipc::JsonValue)] serde_json::Value);

#[tauri::command]
#[specta::specta]
pub async fn fork_update_overview(release: Option<ReleaseMetadata>) -> UpdateOverview {
    let hello = component_hello().await;
    let next = next_step(&hello, release.as_ref().and_then(|r| choose(&r.0)).is_some());
    let h = hello.as_ref().ok();
    UpdateOverview {
        auto: auto_status(h),
        app_repair: h.and_then(|h| h.needs_repair.as_ref()).map(|p| RepairItem {
            txn: p.id.clone(),
            version: p.version.clone(),
            available: repairable_here(p),
        }),
        core_repair: h.and_then(|h| h.core_needs_repair.as_ref()).map(|p| p.version.clone()),
        next,
    }
}

const OFFLINE: &str = "UPDATE_COMPONENT_UNAVAILABLE";

pub(super) async fn audit(before: Option<u64>) -> CmdResult<Vec<Record>> {
    let response = Connection::connect()
        .await
        .with_error_code(OFFLINE)?
        .request(&Request::Audit {
            before_seq: before,
            limit: 50,
        })
        .await;
    audit_response(before, response)
}

fn audit_response(before: Option<u64>, response: Result<Response>) -> CmdResult<Vec<Record>> {
    match response.map_err(|error| {
        if error.is::<std::io::Error>() || error.is::<tokio::time::error::Elapsed>() {
            CommandFailure::coded(OFFLINE, error)
        } else {
            CommandFailure::plain(error)
        }
    })? {
        Response::Audit { records } => checked_page(before, records).stringify_err(),
        Response::Error { message } => Err(CommandFailure::plain(message)),
        other => Err(CommandFailure::plain(format!("unexpected audit response {other:?}"))),
    }
}

fn checked_page(before: Option<u64>, mut records: Vec<Record>) -> Result<Vec<Record>> {
    records.dedup();
    ensure!(
        records.first().is_none_or(|r| before.is_none_or(|b| r.seq < b))
            && records.windows(2).all(|w| w[0].seq > w[1].seq),
        "invalid audit page order"
    );
    Ok(records)
}

#[derive(Serialize, specta::Type)]
pub struct HistoryEntry {
    seq: String,
    time: String,
    source: String,
    event: String,
    trigger: Option<String>,
    from: Option<String>,
    to: Option<String>,
    result: Option<String>,
    error: Option<String>,
}

#[tauri::command]
#[specta::specta]
pub async fn fork_update_history(before: Option<String>) -> CmdResult<Vec<HistoryEntry>> {
    let before = before.map(|s| s.parse()).transpose().stringify_err()?;
    let records = audit(before).await?;
    Ok(records
        .into_iter()
        .map(|r| {
            use clash_verge_updater::audit::Source;
            let claim = r
                .detail
                .as_ref()
                .and_then(|d| d.get("claim").or_else(|| d.pointer("/detail/claim")));
            let field = |key: &str| claim.and_then(|v| v[key].as_str()).map(str::to_owned);
            HistoryEntry {
                seq: r.seq.to_string(),
                time: r.time,
                source: match r.source {
                    Source::Actual => "actual",
                    Source::Installer => "installer",
                    Source::Claimed => "claimed",
                    Source::Observed => "observed",
                }
                .into(),
                event: field("event").unwrap_or(r.event),
                trigger: r.trigger.map(|t| t.kind).or_else(|| {
                    if matches!(r.source, Source::Installer) {
                        r.detail.as_ref()?.get("source")?.as_str().map(str::to_owned)
                    } else {
                        None
                    }
                }),
                from: r.from_version,
                to: r.to_version.or_else(|| field("version")),
                result: r.result.or_else(|| field("result")),
                error: r.error.or_else(|| field("reason")),
            }
        })
        .collect())
}

#[tauri::command]
#[specta::specta]
pub async fn fork_repair_update(txn: String) -> CmdResult<()> {
    repair_by_id(&txn).await.stringify_err()
}

async fn repair_by_id(id: &str) -> Result<()> {
    let _gate = super::take_gate()?;
    let hello = component_hello().await?;
    ensure!(
        !hello.staged && hello.current.is_none(),
        "an update is already in progress"
    );
    let p: Finished = hello
        .needs_repair
        .filter(|p| p.id == id && repairable_here(p))
        .context("the repair is no longer available")?;
    if matches!(super::repair(&p, None, false).await?, Outcome::Applied) && p.kind != Kind::Bundle {
        WindowAfterUpdate::show().keep();
        crate::feat::restart_after_update().await;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test(start_paused = true)]
    async fn audit_distinguishes_transport_failures_from_a_log_read_error() {
        let timed_out = tokio::time::timeout(std::time::Duration::from_secs(60), std::future::pending())
            .await
            .context("the update component did not answer in time");
        let disconnected = Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe).into());
        for response in [timed_out, disconnected] {
            let failure = audit_response(Some(51), response).unwrap_err();
            assert_eq!(failure.code.as_deref(), Some("UPDATE_COMPONENT_UNAVAILABLE"));
        }
        let failure = audit_response(
            Some(51),
            Ok(Response::Error {
                message: "could not read the audit log".into(),
            }),
        )
        .unwrap_err();
        assert_eq!(failure.code, None);
        assert_eq!(failure.detail, "could not read the audit log");
    }

    #[test]
    fn old_audit_pages_can_repeat_an_identical_adjacent_record() {
        let mut newer = Record::new(clash_verge_updater::audit::Source::Actual, "applied");
        newer.seq = 51;
        let mut older = newer.clone();
        older.seq = 50;
        let page = checked_page(Some(52), vec![newer.clone(), newer.clone(), older.clone()]).unwrap();
        assert_eq!(page, vec![newer.clone(), older.clone()]);
        assert!(checked_page(Some(51), vec![newer.clone()]).is_err());
        assert!(checked_page(None, vec![older, newer.clone()]).is_err());
        assert!(checked_page(None, vec![newer.clone(), newer.result("different")]).is_err());
    }

    #[test]
    fn next_step_prioritizes_running_then_repair_then_payload() {
        let mut h: Hello = serde_json::from_value(json!({
            "protocol": 1, "updater_version":"1", "platform":"windows-x86_64",
            "needs_repair": {"id":"tx","version":"2.0.0","kind":"full","outcome":"needs_repair","finished_at":"now",
                "initiator":{"user":"user","id":"user","pid":1}}
        }))
        .unwrap();
        {
            let _gate = super::super::take_gate().unwrap();
            assert!(matches!(next_step(&Ok(h.clone()), true), NextStep::Busy));
        }
        h.staged = true;
        assert!(matches!(next_step(&Ok(h.clone()), false), NextStep::Busy));
        h.staged_by_caller = true;
        let pending = h.needs_repair.take();
        assert!(!super::super::install_busy(&h, super::super::TriggerKind::Manual));
        assert!(matches!(
            next_step(&Ok(h.clone()), true),
            NextStep::Component | NextStep::MacComponent
        ));
        assert!(super::super::install_busy(&h, super::super::TriggerKind::Unattended));
        h.needs_repair = pending;
        h.staged = false;
        assert!(matches!(next_step(&Ok(h.clone()),false),NextStep::FullRepair {version} if version == "2.0.0"));
        let repair = h.needs_repair.as_mut().unwrap();
        repair.kind = Kind::Bundle;
        repair.version = env!("CARGO_PKG_VERSION").to_owned();
        assert!(repairable_here(repair));
        repair.error = Some("the app was neither replaced nor restored; reinstall it".into());
        assert!(repairable_here(repair));
        for other in ["0.0.1", "99.0.0"] {
            repair.version = other.into();
            assert!(!repairable_here(repair));
        }
        assert!(matches!(next_step(&Ok(h.clone()), false), NextStep::NoPayload));
        assert!(matches!(
            next_step(&Ok(h), true),
            NextStep::Component | NextStep::MacComponent
        ));
    }
}
