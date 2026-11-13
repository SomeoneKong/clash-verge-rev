use clash_verge_usage::{UsageQuery, UsageReport};

use super::{CmdResult, StringifyErr as _};

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
