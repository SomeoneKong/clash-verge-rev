#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

fn config(yaml: &str) -> Mapping {
    serde_yaml_ng::from_str(yaml).expect("valid yaml")
}

fn config_node(name: &str, state_dir: &str) -> ConfigNode {
    ConfigNode {
        name: name.into(),
        config: NodeConfig {
            state_dir: state_dir.into(),
            ..NodeConfig::default()
        },
    }
}

fn running_node(name: &str, instance: &str) -> RunningNode {
    RunningNode {
        name: name.into(),
        instance: instance.into(),
        config: NodeConfig {
            control_url: "http://hs.example".into(),
            state_dir: format!("/home/{name}"),
            ..NodeConfig::default()
        },
        status: NodeStatus {
            state: NodeState::Running,
            identity: true,
            device: "laptop".into(),
            user: "kong".into(),
            tailnet: String::new(),
            addresses: vec!["100.64.0.1".into()],
            key_expiry: None,
            auth_url: String::new(),
            error: String::new(),
        },
    }
}

fn profile(uid: &str, nodes: Vec<ConfigNode>) -> Profile {
    Profile {
        uid: uid.into(),
        name: format!("profile {uid}"),
        nodes,
    }
}

fn found(nodes: &[(&str, &str)]) -> DirMatch {
    DirMatch {
        invalid: false,
        nodes: nodes.iter().map(|(n, i)| ((*n).into(), (*i).into())).collect(),
    }
}

#[test]
fn reads_the_tailscale_nodes_of_a_config() {
    let nodes = tailscale_nodes(&config(
        r#"
proxies:
  - name: hs-tcp
    type: tailscale
    control-url: http://hs.example
    state-dir: ./hs-tcp
    ephemeral: true
    auth-key: tskey-auth-secret
  - name: vless
    type: vless
    server: 1.2.3.4
  - name: plain
    type: tailscale
    auth-key: "  "
  - type: tailscale
  - not a mapping
"#,
    ));
    assert_eq!(
        nodes,
        vec![
            ConfigNode {
                name: "hs-tcp".into(),
                config: NodeConfig {
                    control_url: "http://hs.example".into(),
                    state_dir: "./hs-tcp".into(),
                    ephemeral: true,
                    config_auth_key: true,
                },
            },
            config_node("plain", ""),
        ]
    );
    assert!(tailscale_nodes(&config("rules: []")).is_empty());
}

#[test]
fn reads_the_fields_of_a_node_as_the_core_does() {
    let nodes = tailscale_nodes(&config(
        r#"
proxies:
  - NAME: by-case
    Type: tailscale
    state_dir: ./isolated
    Control_URL: http://hs.example
    EPHEMERAL: 1
  - name: exact-first
    type: tailscale
    state-dir: exact
    state_dir: alias
  - name: 123
    type: tailscale
    state-dir: 456
    auth-key: 789
  - name: float-dir
    type: tailscale
    state-dir: 1.5
  - name: null-dir
    type: tailscale
    state-dir:
    ephemeral:
  - name: unreadable-dir
    type: tailscale
    state-dir: [a]
  - name: unreadable-flag
    type: tailscale
    ephemeral: "yes"
  - name: ambiguous-dir
    type: tailscale
    state_dir: ./a
    STATE-DIR: ./b
  - name: same-aliases
    type: tailscale
    state_dir: ./a
    STATE-DIR: ./a
  - name: infinite-dir
    type: tailscale
    state-dir: .inf
  - name: negative-infinite-dir
    type: tailscale
    state-dir: -.inf
  - name: folded-key
    type: tailscale
    ſtate-dir: isolated
  - name: folded-key-in-conflict
    type: tailscale
    ſtate-dir: isolated
    state_dir: other
  - name: signed-zero
    type: tailscale
    state_dir: -0.0
    STATE-DIR: 0.0
  - name: zero-signed
    type: tailscale
    STATE-DIR: 0.0
    state_dir: -0.0
  - name: text-and-number-alike
    type: tailscale
    state_dir: "12"
    STATE-DIR: 12
  - name: exact-over-conflict
    type: tailscale
    state-dir: exact
    ſtate_dir: one
    STATE-DIR: two
"#,
    ));
    let read: Vec<_> = nodes
        .iter()
        .map(|n| (n.name.as_str(), n.config.state_dir.as_str()))
        .collect();
    assert_eq!(
        read,
        vec![
            ("by-case", "./isolated"),
            ("exact-first", "exact"),
            ("123", "456"),
            ("float-dir", "1.5E+00"),
            ("null-dir", ""),
            ("same-aliases", "./a"),
            ("infinite-dir", "+Inf"),
            ("negative-infinite-dir", "-Inf"),
            ("folded-key", "isolated"),
            ("text-and-number-alike", "12"),
            ("exact-over-conflict", "exact"),
        ],
        "a node the core cannot read is left out rather than taken for one on the default dir"
    );
    assert_eq!(nodes[0].config.control_url, "http://hs.example");
    assert!(nodes[0].config.ephemeral);
    assert!(nodes[2].config.config_auth_key);
    assert!(!nodes[4].config.ephemeral);
}

#[test]
fn lists_the_running_config_first_and_omits_profiles_without_nodes() {
    let logins = assemble(
        Some(profile("cur", vec![config_node("ignored", "")])),
        Some(vec![running_node("a", "i-a")]),
        vec![profile("empty", vec![]), profile("other", vec![config_node("x", "x")])],
        &[DirMatch::default()],
    );
    assert!(logins.supported);
    let uids: Vec<_> = logins.profiles.iter().map(|p| (p.uid.as_str(), p.current)).collect();
    assert_eq!(uids, vec![("cur", true), ("other", false)]);
    let current = &logins.profiles[0].nodes[0];
    assert_eq!(current.name, "a", "the running config as the core lists it");
    assert_eq!(current.target.as_ref().map(|t| t.instance.as_str()), Some("i-a"));
}

#[test]
fn links_a_node_of_another_profile_only_to_a_single_shared_identity() {
    let others = vec![profile(
        "other",
        vec![
            config_node("shared", "a"),
            config_node("conflict", ""),
            config_node("alone", "z"),
            config_node("outside", "/etc/x"),
        ],
    )];
    assert_eq!(state_dirs(&others), vec!["a", "", "z", "/etc/x"]);
    let logins = assemble(
        Some(profile("cur", vec![])),
        Some(vec![
            running_node("a", "i-a"),
            running_node("b", "i-b"),
            running_node("c", "i-c"),
        ]),
        others,
        &[
            found(&[("a", "i-a")]),
            found(&[("b", "i-b"), ("c", "i-c")]),
            found(&[]),
            DirMatch {
                invalid: true,
                nodes: Vec::new(),
            },
        ],
    );
    let rows = &logins.profiles[1].nodes;
    assert_eq!(rows[0].target.as_ref().map(|t| t.name.as_str()), Some("a"));
    assert_eq!(rows[0].config.state_dir, "a", "its own config stays with the row");
    assert!(rows[1].target.is_none());
    assert_eq!(rows[1].conflicts, vec!["b", "c"], "no node is picked among several");
    assert!(rows[2].target.is_none() && rows[2].conflicts.is_empty() && !rows[2].invalid);
    assert!(rows[3].invalid && rows[3].target.is_none());
}

#[test]
fn a_match_for_a_node_that_is_gone_links_nothing() {
    let logins = assemble(
        None,
        Some(vec![running_node("a", "i-a")]),
        vec![profile("other", vec![config_node("x", "a")])],
        &[found(&[("a", "i-old")])],
    );
    assert!(logins.profiles[0].nodes[0].target.is_none());
}

#[test]
fn a_core_without_logins_shows_the_config_of_every_profile() {
    let logins = assemble(
        Some(profile("cur", vec![config_node("a", "a")])),
        None,
        vec![profile("other", vec![config_node("x", "a")])],
        &[],
    );
    assert!(!logins.supported);
    assert_eq!(logins.profiles.len(), 2);
    for profile in &logins.profiles {
        assert!(profile.nodes.iter().all(|row| row.target.is_none()));
    }
    assert_eq!(logins.profiles[0].nodes[0].name, "a");
}
