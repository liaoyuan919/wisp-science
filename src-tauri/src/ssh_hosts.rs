//! SSH host registry: model, pure transforms, and tauri commands. The agent
//! reaches these hosts with its existing `shell` tool (`ssh <alias> '<cmd>'`);
//! this module just tracks which hosts exist and tells the agent about them.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SshHost {
    pub alias: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

pub fn upsert_host(mut hosts: Vec<SshHost>, host: SshHost) -> Vec<SshHost> {
    if let Some(existing) = hosts.iter_mut().find(|h| h.alias == host.alias) {
        *existing = host;
    } else {
        hosts.push(host);
    }
    hosts
}

pub fn remove_host(mut hosts: Vec<SshHost>, alias: &str) -> Vec<SshHost> {
    hosts.retain(|h| h.alias != alias);
    hosts
}

/// Parse `Host` aliases from an ~/.ssh/config body. Skips wildcard patterns
/// (`*`, `?` — those are match rules, not connectable hosts) and dedupes,
/// preserving first-seen order.
pub fn parse_ssh_config_aliases(config: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in config.lines() {
        let line = line.trim();
        let mut parts = line.split_whitespace();
        let Some(kw) = parts.next() else { continue };
        if !kw.eq_ignore_ascii_case("host") {
            continue;
        }
        for alias in parts {
            if alias.contains('*') || alias.contains('?') {
                continue;
            }
            if !out.iter().any(|a| a == alias) {
                out.push(alias.to_string());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(alias: &str, notes: Option<&str>) -> SshHost {
        SshHost { alias: alias.into(), user: None, port: None, identity_file: None, notes: notes.map(Into::into) }
    }

    #[test]
    fn upsert_adds_new_and_replaces_by_alias_in_place() {
        let list = vec![host("a", Some("first")), host("b", None)];
        let added = upsert_host(list, host("c", None));
        assert_eq!(added.iter().map(|h| h.alias.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);

        let replaced = upsert_host(added, host("a", Some("second")));
        assert_eq!(replaced.iter().map(|h| h.alias.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);
        assert_eq!(replaced[0].notes.as_deref(), Some("second"));
    }

    #[test]
    fn remove_drops_matching_alias() {
        let list = vec![host("a", None), host("b", None)];
        let out = remove_host(list, "a");
        assert_eq!(out.iter().map(|h| h.alias.as_str()).collect::<Vec<_>>(), ["b"]);
    }

    #[test]
    fn parses_host_aliases_skips_wildcards_and_dedupes() {
        let cfg = "\
Host gpu-box lab-gpu
    HostName 10.0.0.5
    User alice

Host *
    ForwardAgent yes

Host biowulf
    HostName biowulf.nih.gov

Host gpu-box
    Port 2222
";
        assert_eq!(
            parse_ssh_config_aliases(cfg),
            vec!["gpu-box".to_string(), "lab-gpu".to_string(), "biowulf".to_string()]
        );
    }
}
