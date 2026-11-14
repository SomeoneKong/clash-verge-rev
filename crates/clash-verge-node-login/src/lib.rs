//! The nodes of the profiles that log in as a device of an account, organized by profile as
//! the node login dialog shows them.
//!
//! Only the nodes of the running config run in the core, so only they, and the nodes of other
//! profiles that share a device identity with exactly one of them, can be logged in or out.

use serde::{Deserialize, Serialize};
use serde_yaml_ng::{Mapping, Value};
use specta::Type;

/// What the config of a node says about its login, without secrets.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct NodeConfig {
    /// Empty for the control server of Tailscale.
    pub control_url: String,
    /// As the core resolved it for a node of the running config; as the config writes it, or
    /// empty, for a node of another profile.
    pub state_dir: String,
    pub ephemeral: bool,
    /// The config has an auth key, which logs the node in again whenever it starts logged out.
    pub config_auth_key: bool,
}

/// A node as the config of a profile writes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ConfigNode {
    pub name: String,
    pub config: NodeConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    NotStarted,
    Starting,
    NeedsLogin,
    /// Waits for the admin of the control server to approve the device.
    NeedsApproval,
    Running,
    Stopped,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct NodeStatus {
    pub state: NodeState,
    /// The device has an identity, whatever the state, as one whose key expired.
    pub identity: bool,
    pub device: String,
    pub user: String,
    pub tailnet: String,
    pub addresses: Vec<String>,
    pub key_expiry: Option<String>,
    pub auth_url: String,
    pub error: String,
}

/// A node of the running config, which logins and logouts act on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct RunningNode {
    pub name: String,
    /// Tells this node apart from another one of the same name; logins and logouts carry it.
    pub instance: String,
    pub config: NodeConfig,
    pub status: NodeStatus,
}

/// What the core found for the state-dir of a node of another profile.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DirMatch {
    /// The core does not allow the dir.
    pub invalid: bool,
    /// The nodes of the running config on the same dir, by name and instance.
    pub nodes: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct NodeRow {
    pub name: String,
    /// The node as its own profile writes it.
    pub config: NodeConfig,
    /// What the row shows the state of and acts on: the node itself in the running config, or
    /// for another profile, the node of the running config it shares its identity with.
    pub target: Option<RunningNode>,
    /// The nodes of the running config a node of another profile shares its identity with,
    /// when there are several, so none is the one to act on.
    pub conflicts: Vec<String>,
    /// The core does not allow the state-dir of a node of another profile.
    pub invalid: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ProfileNodes {
    pub uid: String,
    pub name: String,
    pub current: bool,
    pub nodes: Vec<NodeRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct NodeLogins {
    /// The core can log nodes in.
    pub supported: bool,
    pub profiles: Vec<ProfileNodes>,
}

/// How a login or logout of a node of the running config ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum NodeLoginOutcome {
    Done {
        node: Box<RunningNode>,
    },
    /// The core told why, with a code such as `logged_in`, `rejected`, `replaced` or `timeout`.
    Failed {
        code: String,
        message: String,
    },
}

/// A profile with the nodes its config writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename = "LoginProfile")]
pub struct Profile {
    pub uid: String,
    pub name: String,
    pub nodes: Vec<ConfigNode>,
}

/// The tailscale nodes of a config, in the order the config writes them, with their fields
/// read as the core reads them. A node with a field the core cannot read is left out, as the
/// core does not load such a config.
pub fn tailscale_nodes(config: &Mapping) -> Vec<ConfigNode> {
    let Some(Value::Sequence(proxies)) = config.get("proxies") else {
        return Vec::new();
    };
    proxies
        .iter()
        .filter_map(|proxy| {
            let proxy = proxy.as_mapping()?;
            if field(proxy, "type", text)? != "tailscale" {
                return None;
            }
            let name = field(proxy, "name", text)?;
            if name.is_empty() {
                return None;
            }
            Some(ConfigNode {
                name,
                config: NodeConfig {
                    control_url: field(proxy, "control-url", text)?,
                    state_dir: field(proxy, "state-dir", text)?,
                    ephemeral: field(proxy, "ephemeral", flag)?,
                    config_auth_key: !field(proxy, "auth-key", text)?.trim().is_empty(),
                },
            })
        })
        .collect()
}

/// A field of a proxy decoded as the core finds and decodes it: under its key as written, or
/// else under a key that differs only in case or in `_` written for `-`. Among such keys the
/// core takes any, so they must decode alike; `None` when they don't, or when a value is one
/// the core rejects.
fn field<T: PartialEq>(proxy: &Mapping, key: &str, decode: fn(Option<&Value>) -> Option<T>) -> Option<T> {
    if let Some(value) = proxy.get(key) {
        return decode(Some(value));
    }
    let key = fold(key);
    let mut found: Option<T> = None;
    for (k, value) in proxy {
        if k.as_str().map(fold).as_deref() != Some(key.as_str()) {
            continue;
        }
        let decoded = decode(Some(value))?;
        match &found {
            Some(other) if *other != decoded => return None,
            Some(_) => {}
            None => found = Some(decoded),
        }
    }
    found.or_else(|| decode(None))
}

/// A key as the core compares it with the name of a field: `_` written for `-`, and case
/// folded as Go does, under which `ſ` is an `s` and the Kelvin sign a `k`. The names of the
/// fields are ASCII, so no other letter matches one of theirs.
fn fold(key: &str) -> String {
    key.chars()
        .map(|c| match c {
            '_' => '-',
            '\u{17F}' => 's',
            '\u{212A}' => 'k',
            c => c.to_ascii_lowercase(),
        })
        .collect()
}

/// A text field as the core reads it, with numbers written as text; empty when absent or
/// null, `None` for a value the core rejects.
fn text(value: Option<&Value>) -> Option<String> {
    match value {
        None | Some(Value::Null) => Some(String::new()),
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(if let Some(i) = n.as_i64() {
            i.to_string()
        } else if let Some(u) = n.as_u64() {
            u.to_string()
        } else {
            go_exponent(n.as_f64()?)
        }),
        Some(_) => None,
    }
}

/// A float as the core writes it as text, like `1.5E+00`.
fn go_exponent(f: f64) -> String {
    if f.is_infinite() {
        return if f > 0.0 { "+Inf" } else { "-Inf" }.to_owned();
    }
    let s = format!("{f:E}");
    match s.split_once('E').map(|(m, e)| (m, e.parse::<i32>())) {
        Some((mantissa, Ok(exp))) => {
            let sign = if exp < 0 { '-' } else { '+' };
            format!("{mantissa}E{sign}{:02}", exp.abs())
        }
        _ => s,
    }
}

/// A flag as the core reads it, with a nonzero integer as true; false when absent or null,
/// `None` for a value the core rejects.
fn flag(value: Option<&Value>) -> Option<bool> {
    match value {
        None | Some(Value::Null) => Some(false),
        Some(Value::Bool(b)) => Some(*b),
        Some(Value::Number(n)) => n.as_i64().map(|i| i != 0).or_else(|| n.as_u64().map(|u| u != 0)),
        Some(_) => None,
    }
}

/// The state-dirs of the nodes of the other profiles, as the core is asked about them; the
/// matches it returns are in the same order.
pub fn state_dirs(others: &[Profile]) -> Vec<String> {
    others
        .iter()
        .flat_map(|profile| profile.nodes.iter().map(|node| node.config.state_dir.clone()))
        .collect()
}

/// Organizes the nodes by profile, the running config first.
///
/// `running` is what the core lists for the running config, `None` for a core without logins,
/// in which case `current` gives the nodes of the current profile as its config writes them.
/// `matches` has one entry for each of `state_dirs(&others)`.
pub fn assemble(
    current: Option<Profile>,
    running: Option<Vec<RunningNode>>,
    others: Vec<Profile>,
    matches: &[DirMatch],
) -> NodeLogins {
    let mut profiles = Vec::new();
    let supported = running.is_some();
    let running = running.unwrap_or_default();

    if let Some(current) = current {
        let nodes: Vec<NodeRow> = if supported {
            running
                .iter()
                .map(|node| NodeRow {
                    name: node.name.clone(),
                    config: node.config.clone(),
                    target: Some(node.clone()),
                    conflicts: Vec::new(),
                    invalid: false,
                })
                .collect()
        } else {
            current.nodes.into_iter().map(unlinked_row).collect()
        };
        if !nodes.is_empty() {
            profiles.push(ProfileNodes {
                uid: current.uid,
                name: current.name,
                current: true,
                nodes,
            });
        }
    }

    let mut matches = matches.iter();
    for profile in others {
        let nodes: Vec<NodeRow> = profile
            .nodes
            .into_iter()
            .map(|node| {
                let found = matches.next().filter(|_| supported);
                let mut row = unlinked_row(node);
                let Some(found) = found else {
                    return row;
                };
                if found.invalid {
                    row.invalid = true;
                } else if let [(_, instance)] = found.nodes.as_slice() {
                    row.target = running.iter().find(|node| &node.instance == instance).cloned();
                } else if found.nodes.len() > 1 {
                    row.conflicts = found.nodes.iter().map(|(name, _)| name.clone()).collect();
                }
                row
            })
            .collect();
        if !nodes.is_empty() {
            profiles.push(ProfileNodes {
                uid: profile.uid,
                name: profile.name,
                current: false,
                nodes,
            });
        }
    }

    NodeLogins { supported, profiles }
}

fn unlinked_row(node: ConfigNode) -> NodeRow {
    NodeRow {
        name: node.name,
        config: node.config,
        target: None,
        conflicts: Vec::new(),
        invalid: false,
    }
}

#[cfg(test)]
mod tests;
