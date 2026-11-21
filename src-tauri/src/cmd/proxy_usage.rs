use clash_verge_usage::{HistoryQuery, UsageQuery, UsageReport};

use super::{CmdResult, StringifyErr as _};
use crate::{core::handle, ipc::JsonValue};

/// The view of the probe strategy, passed on as the core made it.
#[derive(Debug, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ProbeView {
    /// `ok`, `unsupported` for a core without the view, or `unreachable`.
    pub status: String,
    #[specta(type = Option<JsonValue>)]
    pub view: Option<serde_json::Value>,
    pub error: Option<String>,
}

#[tauri::command]
#[specta::specta]
pub async fn get_probe_view() -> CmdResult<ProbeView> {
    Ok(match handle::Handle::mihomo().get_probe_view().await {
        Ok(Some(view)) => ProbeView {
            status: "ok".to_owned(),
            view: Some(view),
            error: None,
        },
        Ok(None) => ProbeView {
            status: "unsupported".to_owned(),
            view: None,
            error: None,
        },
        Err(error) => ProbeView {
            status: "unreachable".to_owned(),
            view: None,
            error: Some(error.to_string()),
        },
    })
}

#[derive(Debug, serde::Deserialize, serde::Serialize, specta::Type)]
pub struct ProbeMemoryClear {
    pub clear: u64,
    pub groups: u64,
    pub pending: u64,
}

#[derive(Debug, serde::Serialize, specta::Type)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum ClearProbeMemoryOutcome {
    Ok { result: ProbeMemoryClear },
    Unsupported,
    Failed { error: String },
}

#[tauri::command]
#[specta::specta]
pub async fn clear_probe_memory() -> CmdResult<ClearProbeMemoryOutcome> {
    let result = match handle::Handle::mihomo().clear_probe_memory().await {
        Ok(Some(result)) => serde_json::from_value(result).map_err(|error| error.to_string()),
        Ok(None) => return Ok(ClearProbeMemoryOutcome::Unsupported),
        Err(error) => Err(error.to_string()),
    };
    Ok(match result {
        Ok(result) => ClearProbeMemoryOutcome::Ok { result },
        Err(error) => ClearProbeMemoryOutcome::Failed { error },
    })
}

/// The events of the timeline a query asks for, with the coverage of each core; the page is
/// passed on as JSON.
#[tauri::command]
#[specta::specta]
pub async fn query_probe_history(query: HistoryQuery) -> CmdResult<JsonValueOf> {
    let page = crate::core::proxy_usage::probe_history(query).await.stringify_err()?;
    serde_json::to_value(page).map(JsonValueOf).stringify_err()
}

#[tauri::command]
#[specta::specta]
pub async fn refresh_probe_history() -> CmdResult<crate::core::proxy_usage::ProbeRefresh> {
    crate::core::proxy_usage::refresh_probes().await.stringify_err()
}

/// A JSON value handed to the frontend as it is.
#[derive(Debug, serde::Serialize, specta::Type)]
#[serde(transparent)]
pub struct JsonValueOf(#[specta(type = JsonValue)] pub serde_json::Value);

#[tauri::command]
#[specta::specta]
pub async fn get_proxy_usage(request: UsageQuery) -> CmdResult<UsageReport> {
    crate::core::proxy_usage::query(request).await.stringify_err()
}

#[tauri::command]
#[specta::specta]
pub async fn export_proxy_usage(path: std::string::String) -> CmdResult<()> {
    crate::core::proxy_usage::export(std::path::Path::new(&path))
        .await
        .stringify_err()
}

#[tauri::command]
#[specta::specta]
pub async fn get_connection_stats_status() -> CmdResult<crate::core::connection_stats::ConnStatsStatus> {
    crate::core::connection_stats::status().await.stringify_err()
}

#[tauri::command]
#[specta::specta]
pub async fn set_connection_stats(enabled: bool) -> CmdResult<()> {
    crate::core::connection_stats::set_recording(enabled)
        .await
        .stringify_err()
}

#[tauri::command]
#[specta::specta]
pub async fn query_connection_stats(query: clash_verge_usage::ConnQuery) -> CmdResult<clash_verge_usage::ConnReport> {
    crate::core::connection_stats::query(query).await.stringify_err()
}

#[tauri::command]
#[specta::specta]
pub async fn clear_connection_stats() -> CmdResult<()> {
    crate::core::connection_stats::clear().await.stringify_err()
}
