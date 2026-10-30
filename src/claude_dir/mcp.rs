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
    /// Where each of `servers` came from, by name, for `/mcp`.
    pub origins: std::collections::HashMap<String, &'static str>,
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

pub(crate) fn project_trusted(project_root: &Path) -> bool {
    crate::trust::status(project_root) == crate::trust::Status::Trusted
        || std::env::var_os(crate::trust::TRUST_ENV).is_some_and(|v| !v.is_empty())
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Everything `.mcp.json`, active plugins and the user's own Claude Code and
/// `wizard mcp add` entries contribute for `project_root`.
pub fn discover(project_root: &Path) -> Found {
    let mut found = discover_with(
        project_root,
        project_trusted(project_root),
        &Approvals::read(project_root),
    );
    if !found.strict && !super::bare() {
        for (label, path) in [
            ("local", local_store_path()),
            ("claude.json", crate::import_claude::claude_json_path()),
        ] {
            let Some(json) = path.and_then(|path| read_json(&path)) else {
                continue;
            };
            let (servers, unsupported) = servers_for_project(&json, project_root);
            for server in &servers {
                found.origins.entry(server.name.clone()).or_insert(label);
            }
            found.servers.extend(servers);
            found.unsupported.extend(unsupported);
        }
    }
    let mut seen = std::collections::HashSet::new();
    found
        .servers
        .retain(|server| seen.insert(server.name.clone()));
    for server in &mut found.servers {
        expand_server(server, &|name| std::env::var(name).ok());
    }
    found
}

/// `${VAR}` and `${VAR:-default}` in a string, as Claude Code expands them in
/// `.mcp.json`. A variable that is unset and has no default is left as
/// written, so the failure shows up as the server's own error.
pub(crate) fn expand_vars(text: &str, lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let body = &after[..end];
        let (name, default) = match body.split_once(":-") {
            Some((name, default)) => (name, Some(default)),
            None => (body, None),
        };
        match lookup(name).or_else(|| default.map(str::to_string)) {
            Some(value) => out.push_str(&value),
            None => out.push_str(&rest[start..start + 2 + end + 1]),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

fn expand_server(server: &mut McpServerConfig, lookup: &dyn Fn(&str) -> Option<String>) {
    let expand = |text: &str| expand_vars(text, lookup);
    server.command = server.command.as_deref().map(expand);
    server.url = server.url.as_deref().map(expand);
    for arg in &mut server.args {
        *arg = expand(arg);
    }
    for value in server.env.values_mut().chain(server.headers.values_mut()) {
        *value = expand(value);
    }
}

/// `~/.wizard/local-mcp.json`: servers added with `wizard mcp add` at the
/// default `local` scope, kept per project. Same shape as the `projects` map
/// in `~/.claude.json`.
pub fn local_store_path() -> Option<std::path::PathBuf> {
    crate::config::Config::wizard_dir()
        .ok()
        .map(|dir| dir.join("local-mcp.json"))
}

/// The key a project goes under in a `projects` map.
pub fn project_key(project_root: &Path) -> String {
    project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf())
        .display()
        .to_string()
}

/// Servers a Claude-style JSON document holds for `project_root`: its
/// top-level `mcpServers` (user scope) and `projects.<root>.mcpServers`
/// (local scope), and no other project's.
pub fn servers_for_project(
    json: &Value,
    project_root: &Path,
) -> (Vec<McpServerConfig>, Vec<String>) {
    let key = project_key(project_root);
    let mut scoped = serde_json::Map::new();
    if let Some(top) = json.get("mcpServers") {
        scoped.insert("mcpServers".into(), top.clone());
    }
    if let Some(project) = json.get("projects").and_then(|p| p.get(&key)) {
        scoped.insert("projects".into(), serde_json::json!({ key: project }));
    }
    crate::import_claude::parse_mcp_servers(&Value::Object(scoped))
}

// The user's own servers (Claude Code's `~/.claude.json`, user and local
// scopes, read in place, and Wizard's `local-mcp.json`) are the user's files,
// not something that arrived with a clone, so neither waits for project trust.

/// What the user's own settings say about `.mcp.json` servers. Claude Code
/// records these when you approve a server in its dialog, and Wizard honors
/// them so a project you already approved there does not ask again.
///
/// Read from `~/.claude/settings.json` and the project's
/// `.claude/settings.local.json` only. The shared `.claude/settings.json`
/// arrives with a clone, so it cannot approve its own servers.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Approvals {
    /// `enableAllProjectMcpServers`.
    pub all: bool,
    /// `enabledMcpjsonServers`.
    pub enabled: Vec<String>,
    /// `disabledMcpjsonServers`: never connected, even when trusted.
    pub disabled: Vec<String>,
}

impl Approvals {
    pub fn read(project_root: &Path) -> Self {
        let mut files = Vec::new();
        if let Some(user) = super::user_dir() {
            files.push(user.join("settings.json"));
        }
        files.push(project_root.join(".claude/settings.local.json"));
        Self::from_documents(files.iter().filter_map(|path| read_json(path)))
    }

    /// Whether a `.mcp.json` server named `name` may connect, given whether
    /// the whole project is trusted.
    pub fn connects(&self, name: &str, project_trusted: bool) -> bool {
        !self.disabled.iter().any(|n| n == name)
            && (project_trusted || self.all || self.enabled.iter().any(|n| n == name))
    }

    fn from_documents(documents: impl Iterator<Item = Value>) -> Self {
        let mut out = Self::default();
        for doc in documents {
            if doc
                .get("enableAllProjectMcpServers")
                .and_then(Value::as_bool)
                == Some(true)
            {
                out.all = true;
            }
            for (key, list) in [
                ("enabledMcpjsonServers", &mut out.enabled),
                ("disabledMcpjsonServers", &mut out.disabled),
            ] {
                if let Some(names) = doc.get(key).and_then(Value::as_array) {
                    list.extend(names.iter().filter_map(Value::as_str).map(str::to_string));
                }
            }
        }
        out
    }
}

fn discover_with(project_root: &Path, trusted: bool, approvals: &Approvals) -> Found {
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
        for server in &servers {
            found.origins.insert(server.name.clone(), "plugin");
        }
        found.servers.extend(servers);
        found.unsupported.extend(unsupported);
    }
    if super::bare() {
        return found;
    }
    if let Some(json) = read_json(&project_root.join(".mcp.json")) {
        let (servers, unsupported) = crate::import_claude::parse_mcp_servers(&json);
        found.unsupported.extend(unsupported);
        for server in servers {
            if approvals.disabled.contains(&server.name) {
                continue;
            }
            if approvals.connects(&server.name, trusted) {
                found.origins.insert(server.name.clone(), ".mcp.json");
                found.servers.push(server);
            } else {
                found.pending.push(server.name);
            }
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
        let held = discover_with(tmp.path(), false, &Approvals::default());
        assert!(held.servers.is_empty());
        assert_eq!(held.pending, ["fs"]);
        assert_eq!(held.unsupported, ["remote"]);

        let trusted = discover_with(tmp.path(), true, &Approvals::default());
        assert_eq!(trusted.servers.len(), 1);
        assert_eq!(trusted.servers[0].name, "fs");
        assert!(trusted.pending.is_empty());
    }

    #[test]
    fn claude_json_gives_user_and_this_projects_servers_only() {
        let tmp = Tmp::new("mcp3");
        let key = project_key(tmp.path());
        let json = serde_json::json!({
            "mcpServers": {"user": {"command": "u"}},
            "projects": {
                key: {"mcpServers": {"here": {"command": "h"}}},
                "/elsewhere": {"mcpServers": {"there": {"command": "t"}}},
            }
        });
        let (servers, unsupported) = servers_for_project(&json, tmp.path());
        let mut names: Vec<_> = servers.iter().map(|s| s.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["here", "user"]);
        assert!(unsupported.is_empty());
    }

    #[test]
    fn settings_approve_and_disable_project_servers_by_name() {
        let tmp = Tmp::new("mcp4");
        tmp.write(
            ".mcp.json",
            r#"{"mcpServers":{"a":{"command":"x"},"b":{"command":"x"},"c":{"command":"x"}}}"#,
        );
        let approvals = Approvals::from_documents(
            [serde_json::json!({
                "enabledMcpjsonServers": ["a"],
                "disabledMcpjsonServers": ["c"],
            })]
            .into_iter(),
        );
        let found = discover_with(tmp.path(), false, &approvals);
        let names: Vec<_> = found.servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["a"]);
        assert_eq!(found.pending, ["b"]);

        let all = Approvals::from_documents(
            [serde_json::json!({"enableAllProjectMcpServers": true})].into_iter(),
        );
        let found = discover_with(tmp.path(), false, &all);
        assert_eq!(found.servers.len(), 3);
    }

    #[test]
    fn dollar_brace_variables_expand_like_claude_code() {
        let lookup = |name: &str| (name == "TOKEN").then(|| "abc".to_string());
        assert_eq!(expand_vars("Bearer ${TOKEN}", &lookup), "Bearer abc");
        assert_eq!(expand_vars("${NOPE:-fallback}/x", &lookup), "fallback/x");
        assert_eq!(expand_vars("${NOPE}", &lookup), "${NOPE}");
        assert_eq!(expand_vars("a ${unterminated", &lookup), "a ${unterminated");
        assert_eq!(expand_vars("no vars", &lookup), "no vars");
    }

    #[test]
    fn no_file_finds_nothing() {
        let tmp = Tmp::new("mcp2");
        let found = discover_with(tmp.path(), true, &Approvals::default());
        assert!(found.servers.is_empty() && found.pending.is_empty());
    }
}
