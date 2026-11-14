//! The nodes of the profiles that log in as a device of an account, listed with what the core
//! says of those it runs, and their logins and logouts through the core.

use anyhow::Result;
use clash_verge_logging::{Type, logging};
use clash_verge_node_login::{
    self as node_login, ConfigNode, DirMatch, NodeConfig, NodeLoginOutcome, NodeLogins, NodeState, NodeStatus, Profile,
    RunningNode,
};
use tauri_plugin_mihomo::models::{LoginEntry, LoginOutcome, LoginState, StateDirMatch};

use crate::{
    config::{Config, IProfiles, PrfItem, profiles::PROFILE_WRITE_LOCK},
    core::handle,
    enhance::seq::{SeqMap, use_seq},
    utils::{dirs, help},
};

/// The nodes each profile writes, in the order of the profiles.
pub async fn profile_nodes() -> Vec<Profile> {
    let profiles = Config::profiles().await.data_arc();
    let mut result = Vec::new();
    for item in profiles.items.iter().flatten() {
        if !matches!(item.itype.as_deref(), Some("remote" | "local")) {
            continue;
        }
        let Some(uid) = item.uid.as_ref() else {
            continue;
        };
        result.push(Profile {
            uid: uid.to_string(),
            name: item.name.clone().unwrap_or_default().to_string(),
            nodes: config_nodes(&profiles, item).await,
        });
    }
    result
}

/// The nodes by profile. The nodes of the running config are what the core lists; those of
/// the other profiles are what `profiles`, as `profile_nodes` read them, write.
pub async fn list(mut profiles: Vec<Profile>) -> Result<NodeLogins> {
    // A switch of profiles updates the core before it commits the current profile; held so
    // that the current profile and the nodes the core lists belong together.
    let _profile_write_guard = PROFILE_WRITE_LOCK.lock().await;
    let current = {
        let config = Config::profiles().await.data_arc();
        config.current.as_ref().and_then(|uid| {
            let item = config.get_item(uid).ok()?;
            Some(Profile {
                uid: uid.to_string(),
                name: item.name.clone().unwrap_or_default().to_string(),
                nodes: Vec::new(),
            })
        })
    };
    let current = current.map(|mut current| {
        if let Some(index) = profiles.iter().position(|profile| profile.uid == current.uid) {
            current.nodes = profiles.remove(index).nodes;
        }
        current
    });
    let others = profiles;

    let logins = handle::Handle::mihomo()
        .get_logins(&node_login::state_dirs(&others))
        .await?;
    let (running, matches) = match logins {
        Some(logins) => (
            Some(logins.logins.into_iter().map(running_node).collect()),
            logins.matches.into_iter().map(dir_match).collect(),
        ),
        None => (None, Vec::new()),
    };
    Ok(node_login::assemble(current, running, others, &matches))
}

pub async fn login(name: &str, instance: &str, auth_key: Option<&str>) -> Result<NodeLoginOutcome> {
    let auth_key = auth_key.map(str::trim).filter(|key| !key.is_empty());
    let outcome = handle::Handle::mihomo().login_node(name, instance, auth_key).await?;
    Ok(login_outcome(outcome))
}

pub async fn logout(name: &str, instance: &str) -> Result<NodeLoginOutcome> {
    let outcome = handle::Handle::mihomo().logout_node(name, instance).await?;
    Ok(login_outcome(outcome))
}

/// The tailscale nodes of a profile as generating its config would write them: its file, with
/// the proxies enhancement it uses applied. Merge and script enhancements are not run.
async fn config_nodes(profiles: &IProfiles, item: &PrfItem) -> Vec<ConfigNode> {
    let (Some(file), Ok(dir)) = (item.file.as_ref(), dirs::app_profiles_dir()) else {
        return Vec::new();
    };
    let mut config = match help::read_mapping(&dir.join(file.as_str())).await {
        Ok(config) => config,
        Err(err) => {
            logging!(
                warn,
                Type::Config,
                "failed to read profile {file} for node logins: {err:#}"
            );
            return Vec::new();
        }
    };
    let proxies_uid = item.current_proxies().map_or("Proxies", |uid| uid.as_str());
    if let Ok(proxies) = profiles.get_item(proxies_uid)
        && let Some(file) = proxies.file.as_ref()
        && let Ok(seq) = help::read_yaml::<SeqMap>(&dir.join(file.as_str())).await
    {
        config = use_seq(seq, config, "proxies");
    }
    node_login::tailscale_nodes(&config)
}

fn running_node(entry: LoginEntry) -> RunningNode {
    RunningNode {
        name: entry.name,
        instance: entry.instance,
        config: NodeConfig {
            control_url: entry.control_url,
            state_dir: entry.state_dir,
            ephemeral: entry.ephemeral,
            config_auth_key: entry.config_auth_key,
        },
        status: NodeStatus {
            state: match entry.state {
                LoginState::NotStarted => NodeState::NotStarted,
                LoginState::Starting => NodeState::Starting,
                LoginState::NeedsLogin => NodeState::NeedsLogin,
                LoginState::NeedsApproval => NodeState::NeedsApproval,
                LoginState::Running => NodeState::Running,
                LoginState::Stopped => NodeState::Stopped,
                LoginState::Failed => NodeState::Failed,
                LoginState::Unknown => NodeState::Unknown,
            },
            identity: entry.identity,
            device: entry.device,
            user: entry.user,
            tailnet: entry.tailnet,
            addresses: entry.addresses,
            key_expiry: entry.key_expiry,
            auth_url: entry.auth_url,
            error: entry.error,
        },
    }
}

fn dir_match(found: StateDirMatch) -> DirMatch {
    DirMatch {
        invalid: found.invalid,
        nodes: found.nodes.into_iter().map(|node| (node.name, node.instance)).collect(),
    }
}

fn login_outcome(outcome: LoginOutcome) -> NodeLoginOutcome {
    match outcome {
        LoginOutcome::Done(entry) => NodeLoginOutcome::Done {
            node: Box::new(running_node(*entry)),
        },
        LoginOutcome::Failed(failure) => NodeLoginOutcome::Failed {
            code: failure.code,
            message: failure.message,
        },
    }
}
