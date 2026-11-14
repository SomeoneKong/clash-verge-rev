use clash_verge_node_login::{NodeLoginOutcome, NodeLogins, Profile};

use super::{CmdResult, StringifyErr as _};

#[tauri::command]
#[specta::specta]
pub async fn get_profile_login_nodes() -> CmdResult<Vec<Profile>> {
    Ok(crate::core::node_login::profile_nodes().await)
}

#[tauri::command]
#[specta::specta]
pub async fn get_node_logins(profiles: Vec<Profile>) -> CmdResult<NodeLogins> {
    crate::core::node_login::list(profiles).await.stringify_err()
}

#[tauri::command]
#[specta::specta]
pub async fn login_node(
    name: std::string::String,
    instance: std::string::String,
    auth_key: Option<std::string::String>,
) -> CmdResult<NodeLoginOutcome> {
    crate::core::node_login::login(&name, &instance, auth_key.as_deref())
        .await
        .stringify_err()
}

#[tauri::command]
#[specta::specta]
pub async fn logout_node(name: std::string::String, instance: std::string::String) -> CmdResult<NodeLoginOutcome> {
    crate::core::node_login::logout(&name, &instance).await.stringify_err()
}
