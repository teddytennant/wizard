//! MCP servers from a project's `.mcp.json` and from active plugins.
//!
//! A server entry is a command to run, so a `.mcp.json` that arrives with a
//! `git clone` is held back until the project is trusted: the same decision
//! `crate::trust` records for the project's hooks, or `WIZARD_TRUST_PROJECT`.
//! Plugin servers come from something the user installed or named with
//! `--plugin-dir`, so they load.

use std::path::Path;

use serde_json::Value;

use crate::mcp::McpServerConfig;

#[derive(Debug, Default)]
pub struct Found {
    /// Servers that will be connected.
    pub servers: Vec<McpServerConfig>,
    /// Names in a `.mcp.json` that is waiting for the project to be trusted.
    pub pending: Vec<String>,
    /// Names Wizard cannot represent (SSE transport, no command).
    pub unsupported: Vec<String>,
    /// `--strict-mcp-config`: connect only `servers`, not `mcp.toml`'s.
    pub strict: bool,
}

/// Servers from `--mcp-config`: each value is a JSON file or a JSON string.
fn cli_servers() -> (Vec<McpServerConfig>, Vec<String>) {
    let Some(raw) = std::env::var_os(crate::cli_claude::env::MCP_CONFIG) else {
        return (Vec::new(), Vec::new());
    };
    let raw = raw.to_string_lossy().into_owned();
    let mut servers = Vec::new();
    let mut unsupported = Vec::new();
    for value in raw.split(crate::cli_claude::SEP).filter(|v| !v.is_empty()) {
        let json = read_json(Path::new(value)).or_else(|| serde_json::from_str(value).ok());
        match json {
            Some(json) => {
                let (found, skipped) = crate::import_claude::parse_mcp_servers(&json);
                servers.extend(found);
                unsupported.extend(skipped);
            }
            None => unsupported.push(format!("--mcp-config {value}")),
        }
    }
    (servers, unsupported)
}

fn project_trusted(project_root: &Path) -> bool {
    crate::trust::status(project_root) == crate::trust::Status::Trusted
        || std::env::var_os(crate::trust::TRUST_ENV).is_some_and(|v| !v.is_empty())
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Everything `.mcp.json` and active plugins contribute for `project_root`.
pub fn discover(project_root: &Path) -> Found {
    discover_with(project_root, project_trusted(project_root))
}

fn discover_with(project_root: &Path, trusted: bool) -> Found {
    let mut found = Found::default();
    let (named, skipped) = cli_servers();
    found.servers.extend(named);
    found.unsupported.extend(skipped);
    found.strict = std::env::var_os(crate::cli_claude::env::STRICT_MCP).is_some();
    if found.strict {
        return found;
    }
    for json in super::plugins::mcp_json(project_root) {
        let (servers, unsupported) = crate::import_claude::parse_mcp_servers(&json);
        found.servers.extend(servers);
        found.unsupported.extend(unsupported);
    }
    if super::bare() {
        return found;
    }
    if let Some(json) = read_json(&project_root.join(".mcp.json")) {
        let (servers, unsupported) = crate::import_claude::parse_mcp_servers(&json);
        found.unsupported.extend(unsupported);
        if trusted {
            found.servers.extend(servers);
        } else {
            found.pending.extend(servers.into_iter().map(|s| s.name));
        }
    }
    found
}

/// `config` (from `mcp.toml`) with `.mcp.json`, plugin and `--mcp-config`
/// servers added. A name already in `config` keeps the user's entry; with
/// `--strict-mcp-config` only the command-line servers remain.
pub fn overlay(config: crate::mcp::McpConfig, project_root: &Path) -> crate::mcp::McpConfig {
    let found = discover(project_root);
    let base = if found.strict {
        crate::mcp::McpConfig::default()
    } else {
        config
    };
    crate::import_claude::merge_mcp(&base, found.servers).0
}

#[cfg(test)]
mod tests {
    use super::super::testing::Tmp;
    use super::*;

    #[test]
    fn project_servers_wait_for_trust() {
        let tmp = Tmp::new("mcp");
        tmp.write(
            ".mcp.json",
            r#"{"mcpServers":{"fs":{"command":"echo","args":["x"]},"remote":{"type":"sse","url":"http://x"}}}"#,
        );
        let held = discover_with(tmp.path(), false);
        assert!(held.servers.is_empty());
        assert_eq!(held.pending, ["fs"]);
        assert_eq!(held.unsupported, ["remote"]);

        let trusted = discover_with(tmp.path(), true);
        assert_eq!(trusted.servers.len(), 1);
        assert_eq!(trusted.servers[0].name, "fs");
        assert!(trusted.pending.is_empty());
    }

    #[test]
    fn no_file_finds_nothing() {
        let tmp = Tmp::new("mcp2");
        let found = discover_with(tmp.path(), true);
        assert!(found.servers.is_empty() && found.pending.is_empty());
    }
}
