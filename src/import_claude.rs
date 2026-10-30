//! Import artifacts from an existing Claude Code install (`~/.claude/`).
//!
//! Wizard and Claude Code overlap in three concepts a user is likely to have
//! already configured: MCP servers, custom slash commands, and spinner verbs.
//! This module reads those out of `~/.claude.json` / `~/.claude/` and folds
//! them into Wizard's own state, **never clobbering** anything that already
//! exists (servers and command files are skipped by name).
//!
//! The module is split into **pure parsing/mapping** (fully unit-tested
//! without touching the filesystem) and a thin [`run_import`] orchestrator that
//! does the actual IO. Both the blocking onboarding TUI
//! ([`crate::onboarding`]) and the in-app `/settings` menu call [`run_import`],
//! so the import behaves identically wherever it is triggered.
//!
//! Claude Code's *conversations* are a separate concern with a separate,
//! strictly read-only reader: see [`crate::claude_session`]. The `~/.claude`
//! path detectors live here, next to each other, so there is one place that
//! knows the layout.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::config::Config;
use crate::mcp::{McpConfig, McpServerConfig, McpTransport};

/// Which Claude Code artifacts to bring over. Each flag maps to one section of
/// [`run_import`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ImportSelection {
    /// MCP servers from `~/.claude.json` → `~/.wizard/mcp.toml`.
    pub mcp: bool,
    /// Custom commands from `~/.claude/commands/` → `~/.wizard/commands/`.
    pub commands: bool,
    /// Spinner verbs from `~/.claude/settings.json` → `config.ui.spinner_verbs`.
    pub verbs: bool,
}

impl ImportSelection {
    /// True when nothing was selected (the caller can short-circuit).
    pub fn is_empty(self) -> bool {
        !self.mcp && !self.commands && !self.verbs
    }
}

/// What an import actually did, for a user-facing summary. Counts are of items
/// added; `*_skipped` names anything left untouched (already present, or — for
/// MCP — an unsupported transport).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportOutcome {
    /// Spinner verbs read from Claude Code (empty unless `verbs` was selected).
    /// The caller folds these into `config.ui.spinner_verbs`.
    pub spinner_verbs: Vec<String>,
    pub mcp_added: usize,
    /// MCP servers skipped: already-present names and `sse`/unsupported ones.
    pub mcp_skipped: Vec<String>,
    pub cmds_added: usize,
    /// Command files skipped because a same-named file already existed.
    pub cmds_skipped: Vec<String>,
}

impl ImportOutcome {
    /// A one-line-per-artifact summary suitable for a notice / onboarding
    /// printout. Empty string when nothing happened.
    pub fn summary(&self) -> String {
        let mut lines = Vec::new();
        if self.mcp_added > 0 || !self.mcp_skipped.is_empty() {
            let mut line = format!("MCP servers: {} imported", self.mcp_added);
            if !self.mcp_skipped.is_empty() {
                line.push_str(&format!(
                    " ({} skipped: {})",
                    self.mcp_skipped.len(),
                    self.mcp_skipped.join(", ")
                ));
            }
            lines.push(line);
        }
        if self.cmds_added > 0 || !self.cmds_skipped.is_empty() {
            let mut line = format!("commands: {} imported", self.cmds_added);
            if !self.cmds_skipped.is_empty() {
                line.push_str(&format!(" ({} already present)", self.cmds_skipped.len()));
            }
            lines.push(line);
        }
        if !self.spinner_verbs.is_empty() {
            lines.push(format!(
                "spinner verbs: {} imported",
                self.spinner_verbs.len()
            ));
        }
        lines.join("\n")
    }
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

/// `~/.claude` if it exists — the marker that Claude Code is installed.
pub fn claude_home() -> Option<PathBuf> {
    let dir = crate::claude_dir::user_dir()?;
    dir.is_dir().then_some(dir)
}

/// `~/.claude.json` if it exists (Claude Code's primary config, holds
/// `mcpServers`). Under `$CLAUDE_CONFIG_DIR` Claude Code keeps it inside that
/// directory instead.
pub fn claude_json_path() -> Option<PathBuf> {
    let path = match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir).join(".claude.json"),
        None => dirs::home_dir()?.join(".claude.json"),
    };
    path.is_file().then_some(path)
}

/// `~/.claude/projects` if it exists — one subdirectory per working directory,
/// each holding that project's session transcripts. Read by
/// [`crate::claude_session`], which never writes there.
pub fn claude_projects_dir() -> Option<PathBuf> {
    let dir = claude_home()?.join("projects");
    dir.is_dir().then_some(dir)
}

/// `~/.claude/commands/**/*.md` (recursive), each paired with its flattened
/// destination filename. Claude Code namespaces commands by subdirectory
/// (`ns/cmd.md` → `/ns:cmd`) while Wizard scans its commands directory flat,
/// so `ns/cmd.md` imports as `ns-cmd.md`. Sorted by destination name; empty
/// when the directory is absent.
pub fn claude_command_files() -> Vec<(PathBuf, String)> {
    let Some(home) = claude_home() else {
        return Vec::new();
    };
    md_files_flattened(&home.join("commands"))
}

/// How many MCP servers, command files, and spinner verbs are available to
/// import — used to label the picker rows (`"MCP servers (12)"`).
pub fn counts() -> (usize, usize, usize) {
    let mcp = claude_json_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .map(|json| parse_mcp_servers(&json).0.len())
        .unwrap_or(0);
    let commands = claude_command_files().len();
    let verbs = claude_home()
        .and_then(|home| std::fs::read_to_string(home.join("settings.json")).ok())
        .map(|raw| parse_spinner_verbs(&raw).len())
        .unwrap_or(0);
    (mcp, commands, verbs)
}

// ---------------------------------------------------------------------------
// Pure parsing / mapping (unit-tested)
// ---------------------------------------------------------------------------

/// Parse Claude Code's `mcpServers` (both the top-level map and every
/// `projects.<path>.mcpServers` map) into [`McpServerConfig`] entries.
///
/// Returns `(servers, unsupported)` where `unsupported` names servers that
/// could not be represented — `sse` transports and entries with no usable
/// `command`/`url` (Wizard's [`McpTransport`] has only `Stdio` and `Http`).
/// HTTP `headers` (auth tokens) carry over. Duplicate names across maps keep
/// the first occurrence.
pub fn parse_mcp_servers(claude_json: &Value) -> (Vec<McpServerConfig>, Vec<String>) {
    let mut servers = Vec::new();
    let mut unsupported = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let mut ingest = |map: &serde_json::Map<String, Value>| {
        for (name, spec) in map {
            if !seen.insert(name.clone()) {
                continue; // first definition wins
            }
            match map_server(name, spec) {
                Some(server) => servers.push(server),
                None => unsupported.push(name.clone()),
            }
        }
    };

    if let Some(map) = claude_json.get("mcpServers").and_then(Value::as_object) {
        ingest(map);
    }
    if let Some(projects) = claude_json.get("projects").and_then(Value::as_object) {
        for project in projects.values() {
            if let Some(map) = project.get("mcpServers").and_then(Value::as_object) {
                ingest(map);
            }
        }
    }
    (servers, unsupported)
}

/// Map one Claude Code MCP server entry to a [`McpServerConfig`], or `None`
/// when the transport is unsupported (`sse`) or the entry is malformed.
fn map_server(name: &str, spec: &Value) -> Option<McpServerConfig> {
    let kind = spec.get("type").and_then(Value::as_str);
    if kind == Some("sse") {
        return None; // Wizard has no SSE transport
    }

    let command = spec.get("command").and_then(Value::as_str);
    let url = spec.get("url").and_then(Value::as_str);

    // HTTP when explicitly typed http, or when only a url is present.
    if kind == Some("http") || (command.is_none() && url.is_some()) {
        let url = url?.to_string();
        let headers = spec
            .get("headers")
            .and_then(Value::as_object)
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        return Some(McpServerConfig {
            name: name.to_string(),
            transport: McpTransport::Http,
            command: None,
            args: Vec::new(),
            url: Some(url),
            env: HashMap::new(),
            headers,
        });
    }

    // Otherwise stdio: needs a command.
    let command = command?.to_string();
    let args = spec
        .get("args")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let env = spec
        .get("env")
        .and_then(Value::as_object)
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    Some(McpServerConfig {
        name: name.to_string(),
        transport: McpTransport::Stdio,
        command: Some(command),
        args,
        url: None,
        env,
        headers: HashMap::new(),
    })
}

/// Read `spinnerVerbs.verbs` out of a Claude Code `settings.json`. Tolerant of
/// missing keys / wrong shapes (returns an empty list).
pub fn parse_spinner_verbs(settings_json: &str) -> Vec<String> {
    let Ok(json) = serde_json::from_str::<Value>(settings_json) else {
        return Vec::new();
    };
    json.get("spinnerVerbs")
        .and_then(|sv| sv.get("verbs"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Merge `incoming` MCP servers into `existing`, **skipping any whose name is
/// already present** (Wizard's config wins — an import never overwrites a
/// server the user configured). Returns `(merged, added, skipped_names)`.
pub fn merge_mcp(
    existing: &McpConfig,
    incoming: Vec<McpServerConfig>,
) -> (McpConfig, usize, Vec<String>) {
    let mut merged = existing.clone();
    let present: std::collections::HashSet<String> =
        merged.servers.iter().map(|s| s.name.clone()).collect();
    let mut added = 0;
    let mut skipped = Vec::new();
    for server in incoming {
        if present.contains(&server.name) {
            skipped.push(server.name);
            continue;
        }
        merged.servers.push(server);
        added += 1;
    }
    (merged, added, skipped)
}

/// Decide which `(source, destination-name)` command files to copy into
/// `dst_dir`, **skipping any whose destination filename already exists
/// there**. Returns `(to_copy, skipped_names)`.
pub fn plan_command_copies(
    src_files: &[(PathBuf, String)],
    dst_dir: &Path,
) -> (Vec<(PathBuf, String)>, Vec<String>) {
    let mut to_copy = Vec::new();
    let mut skipped = Vec::new();
    for (src, dest) in src_files {
        if dst_dir.join(dest).exists() {
            skipped.push(dest.clone());
        } else {
            to_copy.push((src.clone(), dest.clone()));
        }
    }
    (to_copy, skipped)
}

/// Walk `root` for `*.md` files, pairing each with a flattened filename built
/// from its path relative to `root` (`ns/cmd.md` → `ns-cmd.md`). Sorted by
/// that name; missing/unreadable directories yield nothing. Depth is capped
/// so a symlink cycle cannot spin the walk.
fn md_files_flattened(root: &Path) -> Vec<(PathBuf, String)> {
    const MAX_DEPTH: usize = 8;
    fn walk(dir: &Path, prefix: &str, depth: usize, out: &mut Vec<(PathBuf, String)>) {
        if depth > MAX_DEPTH {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if path.is_dir() {
                walk(&path, &format!("{prefix}{name}-"), depth + 1, out);
            } else if path.is_file()
                && path
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
            {
                out.push((path.clone(), format!("{prefix}{name}")));
            }
        }
    }
    let mut files = Vec::new();
    walk(root, "", 0, &mut files);
    files.sort_by(|a, b| a.1.cmp(&b.1));
    files
}

// ---------------------------------------------------------------------------
// IO orchestration
// ---------------------------------------------------------------------------

/// Perform the import described by `sel`, returning what it did. Writes
/// `~/.wizard/mcp.toml` and `~/.wizard/commands/` as needed; spinner verbs are
/// returned in the outcome for the caller to fold into `config.ui` (so this
/// function never touches `config.toml`).
///
/// Errors only on hard IO failures (e.g. an unreadable/locked target); a
/// missing source is treated as "nothing to import".
pub fn run_import(sel: &ImportSelection) -> Result<ImportOutcome> {
    let mut outcome = ImportOutcome::default();

    if sel.mcp {
        import_mcp(&mut outcome)?;
    }
    if sel.commands {
        import_commands(&mut outcome)?;
    }
    if sel.verbs {
        outcome.spinner_verbs = claude_home()
            .and_then(|home| std::fs::read_to_string(home.join("settings.json")).ok())
            .map(|raw| parse_spinner_verbs(&raw))
            .unwrap_or_default();
    }

    Ok(outcome)
}

fn import_mcp(outcome: &mut ImportOutcome) -> Result<()> {
    let Some(path) = claude_json_path() else {
        return Ok(());
    };
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let Ok(json) = serde_json::from_str::<Value>(&raw) else {
        return Ok(()); // not our place to hard-fail on Claude's config
    };
    let (incoming, unsupported) = parse_mcp_servers(&json);

    let mcp_path = Config::mcp_config_path()?;
    let existing = McpConfig::load(&mcp_path)?;
    let (merged, added, mut skipped) = merge_mcp(&existing, incoming);
    skipped.extend(unsupported);
    if added > 0 {
        merged.save(&mcp_path)?;
    }
    outcome.mcp_added = added;
    outcome.mcp_skipped = skipped;
    Ok(())
}

fn import_commands(outcome: &mut ImportOutcome) -> Result<()> {
    let src_files = claude_command_files();
    if src_files.is_empty() {
        return Ok(());
    }
    let dst_dir = Config::wizard_dir()?.join("commands");
    std::fs::create_dir_all(&dst_dir).with_context(|| format!("creating {}", dst_dir.display()))?;
    let (to_copy, skipped) = plan_command_copies(&src_files, &dst_dir);
    for (src, dest) in &to_copy {
        std::fs::copy(src, dst_dir.join(dest))
            .with_context(|| format!("copying {}", src.display()))?;
    }
    outcome.cmds_added = to_copy.len();
    outcome.cmds_skipped = skipped;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_mcp_maps_stdio_with_args_and_env() {
        let json = json!({
            "mcpServers": {
                "fs": {
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
                    "env": { "FOO": "bar" }
                }
            }
        });
        let (servers, unsupported) = parse_mcp_servers(&json);
        assert!(unsupported.is_empty());
        assert_eq!(servers.len(), 1);
        let s = &servers[0];
        assert_eq!(s.name, "fs");
        assert_eq!(s.transport, McpTransport::Stdio);
        assert_eq!(s.command.as_deref(), Some("npx"));
        assert_eq!(s.args.len(), 3);
        assert_eq!(s.env.get("FOO").map(String::as_str), Some("bar"));
    }

    #[test]
    fn parse_mcp_maps_http_by_type_or_bare_url() {
        let json = json!({
            "mcpServers": {
                "typed": { "type": "http", "url": "https://a.example/mcp" },
                "bare":  { "url": "https://b.example/mcp" }
            }
        });
        let (mut servers, unsupported) = parse_mcp_servers(&json);
        assert!(unsupported.is_empty());
        servers.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(servers.len(), 2);
        assert!(servers.iter().all(|s| s.transport == McpTransport::Http));
        assert_eq!(servers[0].url.as_deref(), Some("https://b.example/mcp"));
    }

    #[test]
    fn parse_mcp_imports_http_headers() {
        let json = json!({
            "mcpServers": {
                "authed": {
                    "type": "http",
                    "url": "https://a.example/mcp",
                    "headers": { "Authorization": "Bearer tok-123", "X-Team": "blue" }
                }
            }
        });
        let (servers, unsupported) = parse_mcp_servers(&json);
        assert!(unsupported.is_empty());
        assert_eq!(servers.len(), 1);
        assert_eq!(
            servers[0].headers.get("Authorization").map(String::as_str),
            Some("Bearer tok-123")
        );
        assert_eq!(
            servers[0].headers.get("X-Team").map(String::as_str),
            Some("blue")
        );
    }

    #[test]
    fn parse_mcp_skips_sse_and_reports_it() {
        let json = json!({
            "mcpServers": {
                "stream": { "type": "sse", "url": "https://c.example/sse" }
            }
        });
        let (servers, unsupported) = parse_mcp_servers(&json);
        assert!(servers.is_empty());
        assert_eq!(unsupported, vec!["stream".to_string()]);
    }

    #[test]
    fn parse_mcp_merges_toplevel_and_per_project() {
        let json = json!({
            "mcpServers": { "global": { "command": "g" } },
            "projects": {
                "/home/u/proj": { "mcpServers": { "local": { "command": "l" } } }
            }
        });
        let (mut servers, _) = parse_mcp_servers(&json);
        servers.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["global", "local"]);
    }

    #[test]
    fn parse_mcp_empty_when_no_servers() {
        let (servers, unsupported) = parse_mcp_servers(&json!({}));
        assert!(servers.is_empty());
        assert!(unsupported.is_empty());
    }

    #[test]
    fn parse_spinner_verbs_reads_list() {
        let raw = r#"{ "spinnerVerbs": { "mode": "replace", "verbs": ["A", "B"] } }"#;
        assert_eq!(
            parse_spinner_verbs(raw),
            vec!["A".to_string(), "B".to_string()]
        );
    }

    #[test]
    fn parse_spinner_verbs_tolerates_missing_and_garbage() {
        assert!(parse_spinner_verbs("{}").is_empty());
        assert!(parse_spinner_verbs(r#"{ "spinnerVerbs": {} }"#).is_empty());
        assert!(parse_spinner_verbs("not json").is_empty());
    }

    fn server(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            transport: McpTransport::Stdio,
            command: Some("x".to_string()),
            args: Vec::new(),
            url: None,
            env: HashMap::new(),
            headers: HashMap::new(),
        }
    }

    #[test]
    fn merge_mcp_skips_existing_names() {
        let existing = McpConfig {
            servers: vec![server("keep")],
        };
        let (merged, added, skipped) = merge_mcp(&existing, vec![server("keep"), server("new")]);
        assert_eq!(added, 1);
        assert_eq!(skipped, vec!["keep".to_string()]);
        let names: Vec<&str> = merged.servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["keep", "new"]);
    }

    #[test]
    fn plan_command_copies_skips_existing_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("dup.md"), b"x").expect("write");
        let src = tempfile::tempdir().expect("src tempdir");
        let a = src.path().join("new.md");
        let b = src.path().join("dup.md");
        std::fs::write(&a, b"x").expect("write");
        std::fs::write(&b, b"x").expect("write");

        let (to_copy, skipped) = plan_command_copies(
            &[(a.clone(), "new.md".to_string()), (b, "dup.md".to_string())],
            dir.path(),
        );
        assert_eq!(to_copy, vec![(a, "new.md".to_string())]);
        assert_eq!(skipped, vec!["dup.md".to_string()]);
    }

    #[test]
    fn md_files_flattened_walks_subdirs_and_namespaces_names() {
        let src = tempfile::tempdir().expect("tempdir");
        std::fs::write(src.path().join("top.md"), b"x").expect("write");
        std::fs::write(src.path().join("notes.txt"), b"x").expect("write");
        let ns = src.path().join("ns");
        std::fs::create_dir_all(ns.join("deep")).expect("mkdir");
        std::fs::write(ns.join("cmd.md"), b"x").expect("write");
        std::fs::write(ns.join("deep").join("inner.md"), b"x").expect("write");

        let files = md_files_flattened(src.path());
        let names: Vec<&str> = files.iter().map(|(_, name)| name.as_str()).collect();
        assert_eq!(names, vec!["ns-cmd.md", "ns-deep-inner.md", "top.md"]);
        assert_eq!(files[0].0, ns.join("cmd.md"));

        // A missing directory yields nothing.
        assert!(md_files_flattened(&src.path().join("absent")).is_empty());
    }
}
