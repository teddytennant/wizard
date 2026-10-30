//! `wizard mcp add | add-json | add-from-claude-desktop | get | list | remove
//! | login | logout | reset-project-choices | serve`.
//!
//! Three places hold a server, named as Claude Code names them:
//!
//! | scope     | file                          | who sees it            |
//! | --------- | ----------------------------- | ---------------------- |
//! | `local`   | `~/.wizard/local-mcp.json`    | you, in this project   |
//! | `user`    | `~/.wizard/mcp.toml`          | you, everywhere        |
//! | `project` | `<project>/.mcp.json`         | everyone with the repo |
//!
//! `list` and `get` also show what Claude Code keeps in `~/.claude.json`, read
//! only: those servers load too, but this command never writes that file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use super::{McpCmd, Scope, Transport};
use crate::mcp::{McpConfig, McpServerConfig, McpTransport};

/// `KEY=value` pairs from `-e`.
pub fn parse_env(items: &[String]) -> Result<HashMap<String, String>> {
    items
        .iter()
        .map(|item| {
            item.split_once('=')
                .filter(|(key, _)| !key.is_empty())
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .ok_or_else(|| anyhow!("invalid environment variable '{item}': use KEY=value"))
        })
        .collect()
}

/// `"Name: value"` headers from `-H`.
pub fn parse_header(items: &[String]) -> Result<HashMap<String, String>> {
    items
        .iter()
        .map(|item| {
            item.split_once(':')
                .filter(|(key, _)| !key.trim().is_empty())
                .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
                .ok_or_else(|| anyhow!("invalid header '{item}': use \"Name: value\""))
        })
        .collect()
}

/// The entry as Claude Code writes it in `.mcp.json`.
fn to_json(server: &McpServerConfig) -> Value {
    match server.transport {
        McpTransport::Stdio => json!({
            "type": "stdio",
            "command": server.command,
            "args": server.args,
            "env": server.env,
        }),
        McpTransport::Http => {
            let mut entry = json!({"type": "http", "url": server.url});
            if !server.headers.is_empty() {
                entry["headers"] = json!(server.headers);
            }
            entry
        }
    }
}

/// The files the three scopes live in, so tests can point them at a temp dir.
pub struct Store {
    wizard_dir: PathBuf,
    project: PathBuf,
}

/// Where a listed server came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    Scope(Scope),
    /// Claude Code's `~/.claude.json`, top level.
    ClaudeUser,
    /// Claude Code's `~/.claude.json`, this project.
    ClaudeLocal,
}

impl Origin {
    fn describe(self) -> &'static str {
        match self {
            Origin::Scope(Scope::Local) => "Local config (private to you in this project)",
            Origin::Scope(Scope::User) => "User config (available in all your projects)",
            Origin::Scope(Scope::Project) => "Project config (shared via .mcp.json)",
            Origin::ClaudeUser => "Claude Code user config (~/.claude.json, read only)",
            Origin::ClaudeLocal => "Claude Code local config (~/.claude.json, read only)",
        }
    }
}

struct Entry {
    origin: Origin,
    name: String,
    /// `None` for an entry Wizard cannot connect to (SSE, no command).
    server: Option<McpServerConfig>,
    /// The `type` it declares, for the "cannot connect" line.
    kind: String,
}

impl Store {
    pub fn new(wizard_dir: PathBuf, project: PathBuf) -> Self {
        Self {
            wizard_dir,
            project,
        }
    }

    fn here() -> Result<Self> {
        Ok(Self::new(
            crate::config::Config::wizard_dir()?,
            std::env::current_dir()?,
        ))
    }

    fn path(&self, scope: Scope) -> PathBuf {
        match scope {
            Scope::Local => self.wizard_dir.join("local-mcp.json"),
            Scope::User => self.wizard_dir.join("mcp.toml"),
            Scope::Project => self.project.join(".mcp.json"),
        }
    }

    fn read_doc(path: &Path) -> Result<Value> {
        match std::fs::read_to_string(path) {
            Ok(raw) if raw.trim().is_empty() => Ok(json!({})),
            Ok(raw) => serde_json::from_str(&raw)
                .map_err(|err| anyhow!("{} is not valid JSON: {err}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
            Err(err) => Err(anyhow!("{}: {err}", path.display())),
        }
    }

    fn write_doc(path: &Path, doc: &Value) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut text = serde_json::to_string_pretty(doc)?;
        text.push('\n');
        std::fs::write(path, text).map_err(|err| anyhow!("{}: {err}", path.display()))
    }

    /// The `mcpServers` map a JSON scope keeps its entries in.
    fn json_map<'a>(
        &self,
        scope: Scope,
        doc: &'a mut Value,
    ) -> &'a mut serde_json::Map<String, Value> {
        if !doc.is_object() {
            *doc = json!({});
        }
        let mut node = doc;
        if scope == Scope::Local {
            let key = crate::claude_dir::mcp::project_key(&self.project);
            node = &mut node["projects"];
            if !node.is_object() {
                *node = json!({});
            }
            node = &mut node[key];
            if !node.is_object() {
                *node = json!({});
            }
        }
        let servers = &mut node["mcpServers"];
        if !servers.is_object() {
            *servers = json!({});
        }
        servers.as_object_mut().expect("just made an object")
    }

    fn contains(&self, scope: Scope, name: &str) -> Result<bool> {
        let path = self.path(scope);
        if scope == Scope::User {
            return Ok(McpConfig::load(&path)?
                .servers
                .iter()
                .any(|s| s.name == name));
        }
        let mut doc = Self::read_doc(&path)?;
        Ok(self.json_map(scope, &mut doc).contains_key(name))
    }

    /// Write `server` into `scope`; the file touched.
    fn insert(&self, scope: Scope, server: &McpServerConfig) -> Result<PathBuf> {
        let path = self.path(scope);
        if scope == Scope::User {
            let mut config = McpConfig::load(&path)?;
            config.servers.push(server.clone());
            config.save(&path)?;
            return Ok(path);
        }
        let mut doc = Self::read_doc(&path)?;
        self.json_map(scope, &mut doc)
            .insert(server.name.clone(), to_json(server));
        Self::write_doc(&path, &doc)?;
        Ok(path)
    }

    /// Take `name` out of `scope`; the file touched, or `None` when it was not there.
    fn remove_from(&self, scope: Scope, name: &str) -> Result<Option<PathBuf>> {
        let path = self.path(scope);
        if scope == Scope::User {
            let mut config = McpConfig::load(&path)?;
            let before = config.servers.len();
            config.servers.retain(|s| s.name != name);
            if config.servers.len() == before {
                return Ok(None);
            }
            config.save(&path)?;
            return Ok(Some(path));
        }
        let mut doc = Self::read_doc(&path)?;
        if self.json_map(scope, &mut doc).remove(name).is_none() {
            return Ok(None);
        }
        Self::write_doc(&path, &doc)?;
        Ok(Some(path))
    }

    fn json_entries(&self, origin: Origin, map: &serde_json::Map<String, Value>) -> Vec<Entry> {
        map.iter()
            .map(|(name, spec)| {
                let wrapped = json!({"mcpServers": {name: spec}});
                let server = crate::import_claude::parse_mcp_servers(&wrapped).0.pop();
                Entry {
                    origin,
                    name: name.clone(),
                    server,
                    kind: spec
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("stdio")
                        .to_string(),
                }
            })
            .collect()
    }

    fn entries(&self) -> Vec<Entry> {
        let mut out = Vec::new();
        for scope in [Scope::Local, Scope::Project] {
            if let Ok(mut doc) = Self::read_doc(&self.path(scope)) {
                let map = self.json_map(scope, &mut doc).clone();
                out.extend(self.json_entries(Origin::Scope(scope), &map));
            }
        }
        if let Ok(config) = McpConfig::load(&self.path(Scope::User)) {
            out.extend(config.servers.into_iter().map(|server| Entry {
                origin: Origin::Scope(Scope::User),
                name: server.name.clone(),
                kind: match server.transport {
                    McpTransport::Stdio => "stdio".into(),
                    McpTransport::Http => "http".into(),
                },
                server: Some(server),
            }));
        }
        if let Some(path) = crate::import_claude::claude_json_path()
            && let Ok(mut doc) = Self::read_doc(&path)
        {
            let local_key = crate::claude_dir::mcp::project_key(&self.project);
            let local = doc
                .get("projects")
                .and_then(|projects| projects.get(&local_key))
                .and_then(|project| project.get("mcpServers"))
                .and_then(Value::as_object)
                .cloned();
            if let Some(map) = local {
                out.extend(self.json_entries(Origin::ClaudeLocal, &map));
            }
            if let Some(map) = doc.get_mut("mcpServers").and_then(Value::as_object_mut) {
                let map = map.clone();
                out.extend(self.json_entries(Origin::ClaudeUser, &map));
            }
        }
        out
    }
}

fn names_of(entries: &[Entry]) -> String {
    let mut names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    names.join(", ")
}

fn describe_command(server: &McpServerConfig) -> String {
    match server.transport {
        McpTransport::Stdio => {
            let mut line = server.command.clone().unwrap_or_default();
            for arg in &server.args {
                line.push(' ');
                line.push_str(arg);
            }
            line
        }
        McpTransport::Http => format!("{} (HTTP)", server.url.as_deref().unwrap_or("")),
    }
}

/// Build the server `add` describes. Pure, so the argument handling is testable.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_server(
    name: &str,
    command_or_url: &str,
    args: &[String],
    transport: Option<Transport>,
    env: &[String],
    header: &[String],
) -> Result<McpServerConfig> {
    let transport = match transport {
        None | Some(Transport::Stdio) => McpTransport::Stdio,
        Some(Transport::Http) => McpTransport::Http,
        Some(Transport::Sse) => bail!(
            "Wizard's MCP client speaks stdio and streamable HTTP; SSE servers are not supported. \
             If the server also serves streamable HTTP, add it with --transport http."
        ),
    };
    let env = parse_env(env)?;
    let headers = parse_header(header)?;
    let mut server = McpServerConfig {
        name: name.to_string(),
        transport,
        command: None,
        args: Vec::new(),
        url: None,
        env: HashMap::new(),
        headers: HashMap::new(),
    };
    match transport {
        McpTransport::Stdio => {
            if !headers.is_empty() {
                bail!("--header only applies to http servers");
            }
            server.command = Some(command_or_url.to_string());
            server.args = args.to_vec();
            server.env = env;
        }
        McpTransport::Http => {
            if !args.is_empty() {
                bail!("an http server takes a URL and no arguments");
            }
            if !env.is_empty() {
                eprintln!("wizard: -e is ignored for an http server");
            }
            server.url = Some(command_or_url.to_string());
            server.headers = headers;
        }
    }
    Ok(server)
}

/// `add-json`'s argument, validated the way `.mcp.json` entries are.
pub(crate) fn server_from_json(name: &str, raw: &str) -> Result<McpServerConfig> {
    let spec: Value =
        serde_json::from_str(raw).map_err(|err| anyhow!("invalid JSON for {name}: {err}"))?;
    if !spec.is_object() {
        bail!("the JSON for {name} must be an object");
    }
    if spec.get("type").and_then(Value::as_str) == Some("sse") {
        bail!("Wizard's MCP client has no SSE transport; use type \"http\" or \"stdio\"");
    }
    let wrapped = json!({"mcpServers": {name: spec}});
    crate::import_claude::parse_mcp_servers(&wrapped)
        .0
        .pop()
        .ok_or_else(|| anyhow!("{name} needs a \"command\" (stdio) or a \"url\" (http)"))
}

fn added_message(server: &McpServerConfig, scope: Scope, path: &Path, project: &Path) -> String {
    let mut out = match server.transport {
        McpTransport::Stdio => format!(
            "Added stdio MCP server {} with command: {} to {} config\n",
            server.name,
            describe_command(server),
            scope.label()
        ),
        McpTransport::Http => format!(
            "Added HTTP MCP server {} with URL: {} to {} config\n",
            server.name,
            server.url.as_deref().unwrap_or(""),
            scope.label()
        ),
    };
    if !server.headers.is_empty() {
        let mut names: Vec<&String> = server.headers.keys().collect();
        names.sort();
        out.push_str("Headers: {\n");
        for name in names {
            out.push_str(&format!("  \"{name}\": \"[REDACTED]\"\n"));
        }
        out.push_str("}\n");
    }
    out.push_str(&format!("File modified: {}", path.display()));
    if scope == Scope::Local {
        out.push_str(&format!(" [project: {}]", project.display()));
    }
    out
}

fn add(store: &Store, server: McpServerConfig, scope: Scope) -> Result<i32> {
    if store.contains(scope, &server.name)? {
        eprintln!(
            "MCP server {} already exists in {} config",
            server.name,
            scope.label()
        );
        return Ok(1);
    }
    let path = store.insert(scope, &server)?;
    println!("{}", added_message(&server, scope, &path, &store.project));
    Ok(0)
}

fn remove(store: &Store, name: &str, scope: Option<Scope>) -> Result<i32> {
    let scopes = match scope {
        Some(scope) => vec![scope],
        None => vec![Scope::Local, Scope::Project, Scope::User],
    };
    for scope in scopes {
        if let Some(path) = store.remove_from(scope, name)? {
            println!(
                "Removed MCP server \"{name}\" from {} config",
                scope.label()
            );
            print!("File modified: {}", path.display());
            if scope == Scope::Local {
                print!(" [project: {}]", store.project.display());
            }
            println!();
            return Ok(0);
        }
    }
    let entries = store.entries();
    eprintln!(
        "No MCP server named \"{name}\". Configured servers: {}",
        names_of(&entries)
    );
    Ok(1)
}

/// The connection state of one server, for `list` and `get`.
enum Health {
    Connected(String),
    Failed(String),
    Pending,
    Unsupported(String),
}

impl Health {
    fn line(&self) -> String {
        match self {
            Health::Connected(_) => "\u{2713} Connected".into(),
            Health::Failed(why) => format!("\u{2718} Failed to connect \u{2014} {why}"),
            Health::Pending => "\u{23f8} Pending approval (run `wizard` to approve)".into(),
            Health::Unsupported(kind) => {
                format!("\u{2718} Not supported: Wizard has no {kind} transport")
            }
        }
    }
}

async fn health(entry: &Entry, project_trusted: bool) -> Health {
    let Some(server) = &entry.server else {
        return Health::Unsupported(entry.kind.clone());
    };
    if entry.origin == Origin::Scope(Scope::Project) && !project_trusted {
        return Health::Pending;
    }
    let probe = crate::mcp::probe(server);
    match tokio::time::timeout(Duration::from_secs(20), probe).await {
        Ok(Ok(sentence)) => Health::Connected(sentence),
        Ok(Err(err)) => Health::Failed(format!("{err:#}").lines().next().unwrap_or("").to_string()),
        Err(_) => Health::Failed("timed out".into()),
    }
}

async fn list(store: &Store) -> Result<i32> {
    let entries = store.entries();
    if entries.is_empty() {
        println!("No MCP servers configured. Use `wizard mcp add` to add a server.");
        return Ok(0);
    }
    println!("Checking MCP server health\u{2026}\n");
    let trusted = crate::claude_dir::mcp::project_trusted(&store.project);
    let mut tasks = tokio::task::JoinSet::new();
    let entries: Vec<std::sync::Arc<Entry>> =
        entries.into_iter().map(std::sync::Arc::new).collect();
    for (at, entry) in entries.iter().enumerate() {
        let entry = entry.clone();
        tasks.spawn(async move { (at, health(&entry, trusted).await) });
    }
    let mut results: Vec<Option<Health>> = (0..entries.len()).map(|_| None).collect();
    while let Some(done) = tasks.join_next().await {
        if let Ok((at, state)) = done {
            results[at] = Some(state);
        }
    }
    for (entry, state) in entries.iter().zip(results) {
        let command = entry
            .server
            .as_ref()
            .map(describe_command)
            .unwrap_or_else(|| format!("({})", entry.kind));
        let state = state.map(|s| s.line()).unwrap_or_default();
        println!("{}: {command} - {state}", entry.name);
    }
    Ok(0)
}

async fn get(store: &Store, name: &str) -> Result<i32> {
    let entries = store.entries();
    let Some(entry) = entries.iter().find(|e| e.name == name) else {
        eprintln!(
            "No MCP server named \"{name}\". Configured servers: {}",
            names_of(&entries)
        );
        return Ok(1);
    };
    let state = health(
        entry,
        crate::claude_dir::mcp::project_trusted(&store.project),
    )
    .await;
    println!("{name}:");
    println!("  Scope: {}", entry.origin.describe());
    println!("  Status: {}", state.line());
    match &state {
        Health::Failed(why) => println!("  Issue: {why}"),
        Health::Connected(info) => println!("  Server: {info}"),
        _ => {}
    }
    match &entry.server {
        Some(server) => match server.transport {
            McpTransport::Stdio => {
                println!("  Type: stdio");
                println!("  Command: {}", server.command.as_deref().unwrap_or(""));
                println!("  Args: {}", server.args.join(" "));
                if !server.env.is_empty() {
                    println!("  Environment:");
                    let mut pairs: Vec<_> = server.env.iter().collect();
                    pairs.sort();
                    for (key, value) in pairs {
                        println!("    {key}={value}");
                    }
                }
            }
            McpTransport::Http => {
                println!("  Type: http");
                println!("  URL: {}", server.url.as_deref().unwrap_or(""));
                if !server.headers.is_empty() {
                    println!("  Headers:");
                    let mut pairs: Vec<_> = server.headers.iter().collect();
                    pairs.sort();
                    for (key, value) in pairs {
                        println!("    {key}: {value}");
                    }
                }
            }
        },
        None => println!("  Type: {}", entry.kind),
    }
    if let Origin::Scope(scope) = entry.origin {
        println!(
            "\nTo remove this server, run: wizard mcp remove {name} -s {}",
            scope.label()
        );
    }
    Ok(0)
}

/// Claude Desktop's config, where it keeps one: macOS, Windows or Linux.
fn desktop_config() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    [
        home.join("Library/Application Support/Claude/claude_desktop_config.json"),
        home.join(".config/Claude/claude_desktop_config.json"),
    ]
    .into_iter()
    .find(|path| path.is_file())
}

fn add_from_desktop(store: &Store, scope: Scope) -> Result<i32> {
    let Some(path) = desktop_config() else {
        eprintln!("No Claude Desktop config found.");
        return Ok(1);
    };
    let doc = Store::read_doc(&path)?;
    let (servers, skipped) = crate::import_claude::parse_mcp_servers(&doc);
    let mut added = 0;
    for server in servers {
        if store.contains(scope, &server.name)? {
            println!(
                "{}: already in {} config, skipped",
                server.name,
                scope.label()
            );
            continue;
        }
        store.insert(scope, &server)?;
        println!("Added {} to {} config", server.name, scope.label());
        added += 1;
    }
    for name in skipped {
        println!("{name}: skipped, Wizard cannot connect to it");
    }
    println!("Imported {added} server(s) from {}", path.display());
    Ok(0)
}

pub async fn run(cmd: McpCmd) -> Result<i32> {
    match cmd {
        McpCmd::Add {
            name,
            command_or_url,
            args,
            scope,
            transport,
            env,
            header,
            callback_port,
            client_id,
            client_secret,
        } => {
            if callback_port.is_some() || client_id.is_some() || client_secret {
                eprintln!(
                    "wizard: --callback-port, --client-id and --client-secret are for MCP OAuth, \
                     which Wizard does not do; ignored"
                );
            }
            let server = build_server(&name, &command_or_url, &args, transport, &env, &header)?;
            add(&Store::here()?, server, scope)
        }
        McpCmd::AddJson {
            name, json, scope, ..
        } => add(&Store::here()?, server_from_json(&name, &json)?, scope),
        McpCmd::AddFromClaudeDesktop { scope } => add_from_desktop(&Store::here()?, scope),
        McpCmd::Get { name } => get(&Store::here()?, &name).await,
        McpCmd::List => list(&Store::here()?).await,
        McpCmd::Remove { name, scope } => remove(&Store::here()?, &name, scope),
        McpCmd::Login { name, .. } => {
            eprintln!(
                "\"{name}\" authenticates with the headers in its configuration; Wizard has no \
                 separate MCP login. Put the credential in a header (`-H \"Authorization: Bearer \
                 ...\"`, or `env:VAR` in mcp.toml) to change it."
            );
            Ok(1)
        }
        McpCmd::Logout { name } => {
            println!(
                "Wizard keeps no OAuth credentials for MCP servers; nothing to clear for \"{name}\"."
            );
            Ok(0)
        }
        McpCmd::ResetProjectChoices => {
            let root = std::env::current_dir()?;
            let was = crate::trust::forget(&root)?;
            println!(
                "{}",
                if was {
                    "The trust decision for this project has been reset. Wizard asks again the next \
                     time you start it here."
                } else {
                    "This project had no trust decision on record."
                }
            );
            Ok(0)
        }
        McpCmd::Serve { .. } => unreachable!("`mcp serve` is rewritten to `mcp-serve` first"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(tmp: &tempfile::TempDir) -> Store {
        let project = tmp.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        Store::new(tmp.path().join("home"), project)
    }

    fn stdio(name: &str) -> McpServerConfig {
        build_server(name, "echo", &["hi".into()], None, &["A=b".into()], &[]).unwrap()
    }

    #[test]
    fn env_and_headers_split_on_the_first_separator() {
        let env = parse_env(&["A=b=c".into(), "D=".into()]).unwrap();
        assert_eq!(env["A"], "b=c");
        assert_eq!(env["D"], "");
        assert!(parse_env(&["novalue".into()]).is_err());
        let headers = parse_header(&["X-Api-Key: abc:123".into()]).unwrap();
        assert_eq!(headers["X-Api-Key"], "abc:123");
        assert!(parse_header(&["nocolon".into()]).is_err());
    }

    #[test]
    fn add_builds_stdio_and_http_and_refuses_sse() {
        let server = stdio("s");
        assert_eq!(server.command.as_deref(), Some("echo"));
        assert_eq!(server.args, ["hi"]);
        assert_eq!(server.env["A"], "b");

        let http = build_server(
            "h",
            "https://x/mcp",
            &[],
            Some(Transport::Http),
            &[],
            &["Authorization: Bearer t".into()],
        )
        .unwrap();
        assert_eq!(http.url.as_deref(), Some("https://x/mcp"));
        assert_eq!(http.headers["Authorization"], "Bearer t");

        let sse = build_server("x", "https://x", &[], Some(Transport::Sse), &[], &[]);
        assert!(sse.unwrap_err().to_string().contains("SSE"));
        assert!(build_server("x", "echo", &[], None, &[], &["A: b".into()]).is_err());
    }

    #[test]
    fn each_scope_round_trips_through_its_file() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(&tmp);
        for scope in [Scope::Local, Scope::User, Scope::Project] {
            let name = format!("srv-{}", scope.label());
            assert!(!store.contains(scope, &name).unwrap());
            store.insert(scope, &stdio(&name)).unwrap();
            assert!(store.contains(scope, &name).unwrap());
        }
        let entries = store.entries();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        for want in ["srv-local", "srv-user", "srv-project"] {
            assert!(names.contains(&want), "{names:?}");
        }
        // The project file is Claude's own format, so Claude Code can read it.
        let raw = std::fs::read_to_string(tmp.path().join("proj/.mcp.json")).unwrap();
        let doc: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(doc["mcpServers"]["srv-project"]["command"], "echo");
        // The local file is keyed by project, like ~/.claude.json.
        let key = crate::claude_dir::mcp::project_key(&tmp.path().join("proj"));
        let raw = std::fs::read_to_string(tmp.path().join("home/local-mcp.json")).unwrap();
        let doc: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            doc["projects"][key]["mcpServers"]["srv-local"]["args"][0],
            "hi"
        );
    }

    #[test]
    fn remove_takes_the_server_out_of_the_scope_that_holds_it() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(&tmp);
        store.insert(Scope::Project, &stdio("p")).unwrap();
        store.insert(Scope::User, &stdio("u")).unwrap();
        assert_eq!(remove(&store, "u", None).unwrap(), 0);
        assert_eq!(remove(&store, "p", Some(Scope::Project)).unwrap(), 0);
        assert_eq!(remove(&store, "p", None).unwrap(), 1);
        assert!(
            store
                .entries()
                .iter()
                .all(|e| e.name != "u" && e.name != "p")
        );
    }

    #[test]
    fn a_duplicate_name_in_one_scope_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(&tmp);
        assert_eq!(add(&store, stdio("dup"), Scope::Local).unwrap(), 0);
        assert_eq!(add(&store, stdio("dup"), Scope::Local).unwrap(), 1);
        assert_eq!(add(&store, stdio("dup"), Scope::User).unwrap(), 0);
    }

    #[test]
    fn add_json_validates_like_mcp_json() {
        let server = server_from_json("j", r#"{"command":"echo","args":["a"]}"#).unwrap();
        assert_eq!(server.args, ["a"]);
        let http = server_from_json("h", r#"{"type":"http","url":"https://x"}"#).unwrap();
        assert_eq!(http.transport, McpTransport::Http);
        assert!(server_from_json("s", r#"{"type":"sse","url":"https://x"}"#).is_err());
        assert!(server_from_json("e", "{}").is_err());
        assert!(server_from_json("n", "[]").is_err());
    }

    #[test]
    fn a_project_file_with_other_servers_keeps_them() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(&tmp);
        std::fs::write(
            tmp.path().join("proj/.mcp.json"),
            r#"{"mcpServers":{"keep":{"command":"x"}},"other":1}"#,
        )
        .unwrap();
        store.insert(Scope::Project, &stdio("new")).unwrap();
        let doc: Value = serde_json::from_str(
            &std::fs::read_to_string(tmp.path().join("proj/.mcp.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(doc["mcpServers"]["keep"]["command"], "x");
        assert_eq!(doc["other"], 1);
    }
}
