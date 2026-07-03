//! SSH host registry: model, pure transforms, and tauri commands. The agent
//! reaches these hosts with its existing `shell` tool (`ssh <alias> '<cmd>'`);
//! this module just tracks which hosts exist and tells the agent about them.

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
