//! The Model Context Protocol, in both directions, behind `--features mcp`.
//!
//! **Client** (this file): connects to the external servers declared in
//! `~/.wizard/mcp.toml` over stdio or streamable HTTP, lists their tools, and
//! exposes each one as a [`Tool`] for the unified registry. This is the
//! supported path for computer use, browser control, database access, and any
//! capability shipped as an MCP server — no rebuild needed.
//!
//! **Server** ([`serve`]): `wizard mcp-serve` points the same protocol the
//! other way, so Claude Code, Cursor or another Wizard can call *Wizard's*
//! tools.
//!
//! # One feature over both halves
//!
//! They are opposite ends of one wire and they share its vocabulary:
//! [`PROTOCOL_VERSION`] is the revision both announce, and [`McpToolInfo`] is
//! the `tools/list` entry the client parses and the server emits. Splitting
//! them would leave a second feature whose entire content is a struct and a
//! string — the objection `docs/plugins.md` already makes to a cargo flag
//! carrying nothing but one credential variant — or would push both into
//! core, where a protocol revision this build cannot speak has no business
//! being. So `mcp` is one feature, and the two surfaces are two
//! registrations, which is the shape `plugins::gateway` proved.
//!
//! # What core kept
//!
//! [`crate::mcp`] — the `mcp.toml` format, the [`McpClient`] and
//! [`McpConnector`] traits, and the [`McpManager`](crate::mcp::McpManager)
//! four surfaces hold. Nothing in this file is named outside it.

pub mod plugin;
pub mod serve;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::timeout;
use tracing::{debug, warn};

use crate::llm::Image;
use crate::mcp::{McpClient, McpConfig, McpConnector, McpServerConfig, McpTransport};
use crate::tools::{Tool, ToolContext, ToolError, ToolKind, ToolOutput};

/// MCP protocol revision this plugin speaks, as a client and as a server.
///
/// One constant for both halves: a `wizard mcp-serve` that announced a
/// different revision from the one this binary's client understands would be
/// two Wizards unable to talk to each other.
pub(crate) const PROTOCOL_VERSION: &str = "2025-03-26";

/// Budget for spawning/dialing a server and completing `initialize`.
///
/// `wizard doctor` probes at this same budget without knowing the number:
/// it asks [`Connector::probe`], which applies it, so a slow-starting
/// `npx`/`uvx` server cannot pass doctor and fail the runtime. Core holding
/// the constant was the old arrangement and would have meant core holding one
/// protocol's timing.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Budget for one `tools/list` page.
const LIST_TIMEOUT: Duration = Duration::from_secs(30);
/// Budget for one `tools/call`.
const CALL_TIMEOUT_SECS: u64 = 120;
/// How long to wait for a stdio child to exit before giving up on it.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
/// Hard cap on `tools/list` pagination so a misbehaving server that keeps
/// returning cursors cannot spin us forever.
const MAX_LIST_PAGES: usize = 64;
/// Budget for one stdio JSON-RPC round trip, so a server that answers with
/// the wrong ids (or nothing at all) cannot wedge a request forever. Matches
/// the `tools/call` budget — the longest legitimate operation.
const STDIO_REQUEST_TIMEOUT: Duration = Duration::from_secs(CALL_TIMEOUT_SECS);
/// Stale/mismatched-id responses tolerated per request before we give up on
/// the server instead of reading its stdout to EOF.
const MAX_STALE_RESPONSES: usize = 50;

/// Parent environment variables forwarded to a spawned stdio server. The
/// child's environment is otherwise cleared so servers don't inherit API
/// keys and other secrets from the wizard process.
const STDIO_ENV_ALLOWLIST: &[&str] = &[
    "PATH", "HOME", "LANG", "LC_ALL", "TERM", "USER", "SHELL", "TMPDIR",
];

/// Dynamic-linker variables never forwarded from `mcp.toml` to a stdio
/// child: each one is a code-injection vector into the spawned process.
const STDIO_ENV_DENYLIST: &[&str] = &[
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "LD_AUDIT",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
];

/// Native tool names an MCP tool must never shadow in the registry; a tool
/// advertised under one of these is namespaced `server__tool`. Must cover
/// everything `ToolRegistry::with_native_tools` registers plus the names the
/// agent registers at runtime over the top of an MCP tool that took them —
/// `spawn_subagent` and `run_code` — because that registration *replaces* the
/// server's tool rather than colliding with it, so the server's tool goes
/// unreachable with no warning to anyone. A unit test enforces the list.
///
/// The web tools, the two git tools, `publish` and `json_query` are on it
/// unconditionally although each is a plugin, because this list is about
/// *names*: a name Wizard itself can register must not be claimable by an MCP
/// server on a build that happens to have left the plugin out, or the server's
/// `web_fetch` would work until somebody rebuilt with the feature on and then
/// collide. Holding the string is what core is allowed to do with a plugin —
/// see the `ProviderKind::ANTHROPIC` argument in `docs/plugins.md`; what it may
/// not do is name the type. It is also why the list says nothing about which
/// *language* a plugin is written in: `git_status` is Lua and `json_query` is
/// JavaScript and the reservation is identical.
const RESERVED_TOOL_NAMES: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "list_files",
    "search_files",
    "execute",
    "git_status",
    "git_diff",
    "memory",
    "todo",
    "manual",
    "web_fetch",
    "web_search",
    "x_search",
    "generate_image",
    "task_output",
    "task_kill",
    "subagent_status",
    "subagent_kill",
    "run_command",
    "compact",
    "computer",
    "publish",
    "json_query",
    "spawn_subagent",
    crate::tools::code::RUN_CODE_TOOL_NAME,
];

/// A tool as advertised by an MCP server's `tools/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolInfo {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// JSON Schema for the tool's arguments (`inputSchema` on the wire).
    #[serde(rename = "inputSchema", default)]
    pub input_schema: Value,
}

/// Pipes of a spawned stdio server. Held under one lock so each JSON-RPC
/// request writes its line and reads its response without interleaving.
struct StdioIo {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

/// Stdio transport state. The child handle lives behind its own lock so
/// shutdown never contends with an in-flight request holding the I/O lock.
struct StdioTransport {
    child: Mutex<Child>,
    io: Mutex<StdioIo>,
    /// Whether the one automatic respawn after a mid-session crash has been
    /// spent (see [`McpConnection::stdio_request`]).
    respawned: std::sync::atomic::AtomicBool,
}

/// Streamable-HTTP transport state.
struct HttpTransport {
    client: reqwest::Client,
    url: String,
    /// `Mcp-Session-Id` issued by the server on `initialize`, echoed on
    /// subsequent requests.
    session_id: Mutex<Option<String>>,
}

enum Transport {
    Stdio(Box<StdioTransport>),
    Http(HttpTransport),
}

/// Live connection to one MCP server. Internally serializes JSON-RPC
/// requests; safe to share via `Arc`.
pub struct McpConnection {
    config: McpServerConfig,
    transport: Transport,
    next_id: AtomicU64,
}

impl McpConnection {
    /// Spawn/dial the server and run the MCP `initialize` handshake.
    pub async fn connect(config: McpServerConfig) -> Result<Self> {
        let transport = match config.transport {
            McpTransport::Stdio => Transport::Stdio(Box::new(spawn_stdio(&config)?)),
            McpTransport::Http => Transport::Http(open_http(&config)?),
        };
        let connection = Self {
            config,
            transport,
            next_id: AtomicU64::new(1),
        };
        timeout(CONNECT_TIMEOUT, connection.initialize())
            .await
            .map_err(|_| {
                anyhow!(
                    "MCP server '{}' did not complete initialize within {}s",
                    connection.config.name,
                    CONNECT_TIMEOUT.as_secs()
                )
            })??;
        Ok(connection)
    }

    /// Server name from its config.
    pub fn name(&self) -> &str {
        &self.config.name
    }

    /// MCP `initialize` request followed by the `notifications/initialized`
    /// notification.
    async fn initialize(&self) -> Result<()> {
        let result = self.request("initialize", initialize_params()).await?;
        let server_info = result
            .get("serverInfo")
            .and_then(|info| info.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let proto = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        debug!(
            server = %self.config.name,
            server_info,
            protocol = proto,
            "MCP initialize complete"
        );
        // A server that accepted initialize but rejects the initialized
        // notification will surface a clear error on the first real request,
        // so this is warn-and-continue.
        if let Err(err) = self.notify("notifications/initialized", json!({})).await {
            warn!(
                server = %self.config.name,
                "failed to send initialized notification: {err:#}"
            );
        }
        Ok(())
    }

    /// `tools/list` — enumerate the server's tools (follows pagination).
    pub async fn list_tools(&self) -> Result<Vec<McpToolInfo>> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let result = timeout(LIST_TIMEOUT, self.request("tools/list", params))
                .await
                .map_err(|_| {
                    anyhow!(
                        "tools/list on MCP server '{}' timed out after {}s",
                        self.config.name,
                        LIST_TIMEOUT.as_secs()
                    )
                })??;
            let page: Vec<McpToolInfo> =
                serde_json::from_value(result.get("tools").cloned().unwrap_or_else(|| json!([])))
                    .with_context(|| {
                    format!(
                        "MCP server '{}' returned a malformed tools/list result",
                        self.config.name
                    )
                })?;
            tools.extend(page);
            match result.get("nextCursor").and_then(Value::as_str) {
                Some(next) if !next.is_empty() => cursor = Some(next.to_string()),
                _ => return Ok(tools),
            }
        }
        warn!(
            server = %self.config.name,
            "tools/list pagination exceeded {MAX_LIST_PAGES} pages; truncating"
        );
        Ok(tools)
    }

    /// `tools/call` — invoke a tool; returns the decoded content (text, plus
    /// any images the server returned) and whether the server flagged the
    /// result as an error.
    pub async fn call_tool(&self, name: &str, args: Value) -> Result<(McpContent, bool)> {
        let arguments = match args {
            Value::Null => json!({}),
            Value::Object(map) => Value::Object(map),
            other => bail!(
                "tool arguments must be a JSON object, got {}",
                json_type_name(&other)
            ),
        };
        let result = self
            .request(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
            )
            .await?;
        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok((decode_content(&result), is_error))
    }

    /// Shut down the connection (terminate the child process if stdio).
    pub async fn close(self) -> Result<()> {
        self.shutdown().await;
        Ok(())
    }

    /// Best-effort teardown usable through a shared reference (the manager
    /// calls this on `/reload` while tools may still hold `Arc`s).
    async fn shutdown(&self) {
        if let Transport::Stdio(transport) = &self.transport {
            let mut child = transport.child.lock().await;
            if let Err(err) = child.start_kill() {
                debug!(
                    server = %self.config.name,
                    "failed to signal MCP server child: {err}"
                );
            }
            if timeout(SHUTDOWN_GRACE, child.wait()).await.is_err() {
                warn!(
                    server = %self.config.name,
                    "MCP server child did not exit within {}s",
                    SHUTDOWN_GRACE.as_secs()
                );
            }
        }
    }

    /// Send one JSON-RPC request and wait for its matching response.
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        match &self.transport {
            Transport::Stdio(transport) => self.stdio_request(transport, id, &message).await,
            Transport::Http(transport) => self.http_request(transport, id, &message).await,
        }
    }

    /// Send a JSON-RPC notification (no response expected).
    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let message = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        match &self.transport {
            Transport::Stdio(transport) => {
                let mut io = transport.io.lock().await;
                write_line(&mut io.stdin, &message, &self.config.name).await
            }
            Transport::Http(transport) => {
                let response = self
                    .http_post(transport, &message)
                    .await?
                    .error_for_status()
                    .with_context(|| {
                        format!(
                            "MCP server '{}' rejected notification '{}'",
                            self.config.name, method
                        )
                    })?;
                // Notifications get 202 Accepted with no body; drain anyway.
                let _ = response.bytes().await;
                Ok(())
            }
        }
    }

    /// One write-then-read round trip over the child's pipes. When the child
    /// turns out to have crashed mid-session, respawns it once per session
    /// (fresh process + handshake) and retries before giving up.
    async fn stdio_request(
        &self,
        transport: &StdioTransport,
        id: u64,
        message: &Value,
    ) -> Result<Value> {
        match self.stdio_request_once(transport, id, message).await {
            Err(err)
                if is_stdio_crash(&err) && !transport.respawned.swap(true, Ordering::SeqCst) =>
            {
                warn!(
                    server = %self.config.name,
                    "MCP server crashed mid-session; respawning once: {err:#}"
                );
                self.respawn_stdio(transport).await?;
                self.stdio_request_once(transport, id, message).await
            }
            other => other,
        }
    }

    /// One write-then-read round trip over the child's pipes. Skips
    /// notifications, junk lines, and (a bounded number of) stale responses;
    /// politely refuses server-to-client requests. The whole round trip is
    /// capped at [`STDIO_REQUEST_TIMEOUT`].
    async fn stdio_request_once(
        &self,
        transport: &StdioTransport,
        id: u64,
        message: &Value,
    ) -> Result<Value> {
        let name = &self.config.name;
        let mut io = transport.io.lock().await;
        write_line(&mut io.stdin, message, name).await?;
        timeout(
            STDIO_REQUEST_TIMEOUT,
            read_stdio_response(&mut io, id, name),
        )
        .await
        .map_err(|_| {
            anyhow!(
                "MCP server '{name}' did not answer within {}s",
                STDIO_REQUEST_TIMEOUT.as_secs()
            )
        })?
    }

    /// Replace a crashed stdio child with a fresh process and redo the MCP
    /// handshake. The handshake runs directly over the fresh pipes (going
    /// through [`Self::initialize`] would recurse back into `stdio_request`),
    /// holding the I/O lock so no other request interleaves with it.
    async fn respawn_stdio(&self, transport: &StdioTransport) -> Result<()> {
        let name = &self.config.name;
        let fresh =
            spawn_stdio(&self.config).with_context(|| format!("respawning MCP server '{name}'"))?;
        {
            let mut child = transport.child.lock().await;
            let _ = child.start_kill();
            *child = fresh.child.into_inner();
        }
        let mut io = transport.io.lock().await;
        *io = fresh.io.into_inner();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": initialize_params(),
        });
        write_line(&mut io.stdin, &message, name).await?;
        timeout(CONNECT_TIMEOUT, read_stdio_response(&mut io, id, name))
            .await
            .map_err(|_| {
                anyhow!(
                    "respawned MCP server '{name}' did not complete initialize within {}s",
                    CONNECT_TIMEOUT.as_secs()
                )
            })??;
        let initialized = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {},
        });
        write_line(&mut io.stdin, &initialized, name).await
    }

    /// One streamable-HTTP round trip: POST the request, then read the
    /// response from either a plain JSON body or an SSE stream.
    async fn http_request(
        &self,
        transport: &HttpTransport,
        id: u64,
        message: &Value,
    ) -> Result<Value> {
        let name = &self.config.name;
        let response = self.http_post(transport, message).await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!(
                "MCP server '{name}' returned HTTP {status}: {}",
                truncate(&body, 500)
            );
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();

        if content_type.starts_with("text/event-stream") {
            return self.read_sse_response(response, id).await;
        }

        let msg: Value = response
            .json()
            .await
            .with_context(|| format!("MCP server '{name}' returned a malformed JSON body"))?;
        if !id_matches(msg.get("id"), id) {
            bail!("MCP server '{name}' responded with a mismatched request id");
        }
        extract_result(msg, name)
    }

    /// POST one JSON-RPC message with MCP headers, recording any session id
    /// the server issues.
    async fn http_post(
        &self,
        transport: &HttpTransport,
        message: &Value,
    ) -> Result<reqwest::Response> {
        let name = &self.config.name;
        let mut request = transport
            .client
            .post(&transport.url)
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .header("MCP-Protocol-Version", PROTOCOL_VERSION)
            .json(message);
        if let Some(session) = transport.session_id.lock().await.clone() {
            request = request.header("Mcp-Session-Id", session);
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("failed to reach MCP server '{name}' at {}", transport.url))?;
        if let Some(session) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
        {
            *transport.session_id.lock().await = Some(session.to_string());
        }
        Ok(response)
    }

    /// Read SSE events from a streamable-HTTP response until the JSON-RPC
    /// response matching `id` arrives.
    async fn read_sse_response(&self, response: reqwest::Response, id: u64) -> Result<Value> {
        let name = &self.config.name;
        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        let mut data = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.with_context(|| format!("SSE stream from MCP server '{name}' failed"))?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(newline) = buffer.find('\n') {
                let raw: String = buffer.drain(..=newline).collect();
                let line = raw.trim_end_matches(['\r', '\n']);
                if line.is_empty() {
                    // Blank line terminates one SSE event.
                    if !data.is_empty() {
                        if let Ok(msg) = serde_json::from_str::<Value>(&data) {
                            if msg.get("method").is_none() && id_matches(msg.get("id"), id) {
                                return extract_result(msg, name);
                            }
                        } else {
                            warn!(
                                server = %name,
                                "ignoring malformed SSE data from MCP server"
                            );
                        }
                        data.clear();
                    }
                } else if let Some(payload) = line.strip_prefix("data:") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(payload.trim_start());
                }
                // `event:`, `id:`, `retry:` and comment lines are irrelevant
                // here — JSON-RPC messages ride exclusively in `data:`.
            }
        }
        bail!("SSE stream from MCP server '{name}' ended without a response")
    }
}

/// Params for the MCP `initialize` request (shared by the connect handshake
/// and the post-crash respawn).
fn initialize_params() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": {
            "name": "wizard",
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

/// Marker text identifying the "child crashed mid-session" failure; used by
/// [`McpConnection::stdio_request`] to decide the one automatic respawn.
const STDIO_CRASH_MARKER: &str = "closed its stdout (crashed or exited)";

/// True when `err`'s chain reports the child closing its stdout — the
/// specific failure worth one automatic respawn.
fn is_stdio_crash(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|cause| cause.to_string().contains(STDIO_CRASH_MARKER))
}

/// Read lines from the child's stdout until the response matching `id`
/// arrives. Skips notifications and junk lines, politely refuses
/// server-to-client requests, and gives up after [`MAX_STALE_RESPONSES`]
/// mismatched-id responses so a misbehaving server cannot spin us to EOF.
async fn read_stdio_response(io: &mut StdioIo, id: u64, name: &str) -> Result<Value> {
    let mut stale = 0usize;
    let mut line = String::new();
    loop {
        line.clear();
        let read = io
            .stdout
            .read_line(&mut line)
            .await
            .with_context(|| format!("failed reading from MCP server '{name}'"))?;
        if read == 0 {
            bail!("MCP server '{name}' {STDIO_CRASH_MARKER}; run /reload to restart it");
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(trimmed) {
            Ok(msg) => msg,
            Err(_) => {
                warn!(
                    server = %name,
                    "ignoring non-JSON line on MCP server stdout: {}",
                    truncate(trimmed, 200)
                );
                continue;
            }
        };
        if msg.get("method").is_some() {
            if let Some(req_id) = msg.get("id") {
                // Server-to-client request (sampling, roots, ...): not
                // supported by this minimal client; answer so the server
                // doesn't hang waiting on us.
                let refusal = json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": {
                        "code": -32601,
                        "message": "method not supported by wizard",
                    },
                });
                write_line(&mut io.stdin, &refusal, name).await?;
            }
            // Plain notification: ignore.
            continue;
        }
        if !id_matches(msg.get("id"), id) {
            stale += 1;
            if stale > MAX_STALE_RESPONSES {
                bail!(
                    "MCP server '{name}' sent more than {MAX_STALE_RESPONSES} responses \
                     with mismatched ids; giving up on request {id}"
                );
            }
            debug!(server = %name, "ignoring stale MCP response");
            continue;
        }
        return extract_result(msg, name);
    }
}

/// Split config-supplied env vars into the set passed to the child and the
/// (sorted) names of dynamic-linker variables dropped for safety.
fn filter_config_env(env: &HashMap<String, String>) -> (HashMap<String, String>, Vec<String>) {
    let mut allowed = HashMap::with_capacity(env.len());
    let mut denied = Vec::new();
    for (key, value) in env {
        if STDIO_ENV_DENYLIST.contains(&key.as_str()) {
            denied.push(key.clone());
        } else {
            allowed.insert(key.clone(), value.clone());
        }
    }
    denied.sort_unstable();
    (allowed, denied)
}

/// Spawn the stdio child process for `config` (does not handshake). The
/// child's environment is cleared down to [`STDIO_ENV_ALLOWLIST`] plus the
/// config-supplied vars (minus [`STDIO_ENV_DENYLIST`]).
fn spawn_stdio(config: &McpServerConfig) -> Result<StdioTransport> {
    let command = config.command.as_deref().ok_or_else(|| {
        anyhow!(
            "MCP server '{}' uses stdio transport but has no `command`",
            config.name
        )
    })?;
    let program = shellexpand::tilde(command).into_owned();
    let (env, denied) = filter_config_env(&config.env);
    for variable in &denied {
        warn!(
            server = %config.name,
            variable = %variable,
            "dropping dynamic-linker environment variable from MCP server config"
        );
    }
    let mut command = Command::new(&program);
    command
        .args(&config.args)
        .env_clear()
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    for key in STDIO_ENV_ALLOWLIST {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command.envs(&env);
    let mut child = command.spawn().with_context(|| {
        format!(
            "failed to spawn MCP server '{}' (command: {program})",
            config.name
        )
    })?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("MCP server '{}' child has no stdin pipe", config.name))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("MCP server '{}' child has no stdout pipe", config.name))?;
    if let Some(stderr) = child.stderr.take() {
        let server = config.name.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                debug!(server = %server, "mcp stderr: {line}");
            }
        });
    }

    Ok(StdioTransport {
        child: Mutex::new(child),
        io: Mutex::new(StdioIo {
            stdin,
            stdout: BufReader::new(stdout),
        }),
        respawned: std::sync::atomic::AtomicBool::new(false),
    })
}

/// Resolve one configured header value: `env:VAR` reads `$VAR` at connect
/// time (so a token never sits in `mcp.toml`); anything else is literal.
fn resolve_header_value(raw: &str) -> Result<String> {
    match raw.strip_prefix("env:") {
        Some(var) => {
            let var = var.trim();
            std::env::var(var)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| anyhow!("header value references ${var}, which is not set"))
        }
        None => Ok(raw.to_string()),
    }
}

/// Build the HTTP transport for `config` (does not handshake). Configured
/// `headers` (with `env:` values resolved) are sent on every request.
fn open_http(config: &McpServerConfig) -> Result<HttpTransport> {
    let url = config.url.clone().ok_or_else(|| {
        anyhow!(
            "MCP server '{}' uses http transport but has no `url`",
            config.name
        )
    })?;
    let mut headers = reqwest::header::HeaderMap::with_capacity(config.headers.len());
    for (key, raw) in &config.headers {
        let resolved = resolve_header_value(raw)
            .with_context(|| format!("header '{key}' on MCP server '{}'", config.name))?;
        let name = reqwest::header::HeaderName::from_bytes(key.as_bytes()).with_context(|| {
            format!(
                "invalid header name '{key}' on MCP server '{}'",
                config.name
            )
        })?;
        let mut value = reqwest::header::HeaderValue::from_str(&resolved).with_context(|| {
            format!(
                "invalid value for header '{key}' on MCP server '{}'",
                config.name
            )
        })?;
        // Headers here are typically credentials; keep them out of debug logs.
        value.set_sensitive(true);
        headers.insert(name, value);
    }
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(CALL_TIMEOUT_SECS))
        .build()
        .context("failed to build HTTP client for MCP")?;
    Ok(HttpTransport {
        client,
        url,
        session_id: Mutex::new(None),
    })
}

/// Write one newline-delimited JSON-RPC message to the child's stdin.
async fn write_line(stdin: &mut ChildStdin, message: &Value, server: &str) -> Result<()> {
    let mut payload =
        serde_json::to_vec(message).context("failed to serialize JSON-RPC message")?;
    payload.push(b'\n');
    stdin
        .write_all(&payload)
        .await
        .with_context(|| format!("failed writing to MCP server '{server}' (it may have exited)"))?;
    stdin
        .flush()
        .await
        .with_context(|| format!("failed flushing write to MCP server '{server}'"))
}

/// True when a JSON-RPC `id` field matches the id we issued. Servers must
/// echo the same type, but a stringified number is tolerated.
fn id_matches(id: Option<&Value>, expected: u64) -> bool {
    match id {
        Some(Value::Number(n)) => n.as_u64() == Some(expected),
        Some(Value::String(s)) => s.parse::<u64>().is_ok_and(|v| v == expected),
        _ => false,
    }
}

/// Pull `result` out of a JSON-RPC response, converting `error` into a
/// readable failure.
fn extract_result(mut msg: Value, server: &str) -> Result<Value> {
    if let Some(error) = msg.get("error") {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        bail!("MCP server '{server}' returned JSON-RPC error {code}: {message}");
    }
    Ok(msg
        .get_mut("result")
        .map(Value::take)
        .unwrap_or(Value::Null))
}

/// A `tools/call` result decoded for the model: the content blocks flattened
/// into one text payload, and the images carried out of it whole.
#[derive(Debug)]
pub struct McpContent {
    /// What the model reads — one block per line, non-text blocks as readable
    /// placeholders. An image keeps its `[image content: <mime>]` marker here
    /// even though the image itself now rides along: a text-only model never
    /// sees the attachment, and must still be told one came back.
    pub text: String,
    /// What the model sees. Handed to the agent loop through
    /// [`ToolOutput::images`], which persists them, announces them to the
    /// surfaces and feeds them back on a following user message.
    pub images: Vec<Image>,
}

/// Decode a `tools/call` result's content blocks: text and placeholders into
/// [`McpContent::text`], images into [`McpContent::images`].
fn decode_content(result: &Value) -> McpContent {
    let blocks = result
        .get("content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let mut parts: Vec<String> = Vec::with_capacity(blocks.len());
    let mut images: Vec<Image> = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                parts.push(
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                );
            }
            Some("image") => match block_image(block.get("data")) {
                Ok(image) => {
                    parts.push(format!("[image content: {}]", image.mime));
                    images.push(image);
                }
                Err(err) => {
                    // The server promised an image and delivered something
                    // else. Say so plainly rather than claiming an image the
                    // model is never shown.
                    warn!("unusable image in MCP tool result: {err:#}");
                    parts.push(format!("[unusable image content: {err:#}]"));
                }
            },
            Some("audio") => {
                let mime = block
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown type");
                parts.push(format!("[audio content: {mime}]"));
            }
            Some("resource") => {
                let resource = block.get("resource");
                let uri = resource
                    .and_then(|r| r.get("uri"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                if let Some(text) = resource.and_then(|r| r.get("text")).and_then(Value::as_str) {
                    parts.push(text.to_string());
                } else if resource.is_some_and(claims_image) {
                    // An embedded binary resource that says it is an image: its
                    // `blob` is base64 like an `image` block's `data`, so take
                    // it the same way — the sniff decides whether the claim was
                    // true. Anything else stays a placeholder: Wizard has no
                    // path for a binary that is not an image.
                    match block_image(resource.and_then(|r| r.get("blob"))) {
                        Ok(image) => {
                            parts.push(format!("[image content: {}: {uri}]", image.mime));
                            images.push(image);
                        }
                        Err(err) => {
                            warn!("unusable image resource '{uri}' in MCP tool result: {err:#}");
                            parts.push(format!("[binary resource: {uri}]"));
                        }
                    }
                } else {
                    parts.push(format!("[binary resource: {uri}]"));
                }
            }
            Some("resource_link") => {
                let uri = block
                    .get("uri")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                parts.push(format!("[resource link: {uri}]"));
            }
            _ => {
                // Unknown block type: pass it through verbatim rather than
                // silently dropping information.
                parts.push(serde_json::to_string(block).unwrap_or_default());
            }
        }
    }
    let mut text = parts.join("\n");
    if text.is_empty() {
        // Some servers return only structured output.
        if let Some(structured) = result.get("structuredContent") {
            text = serde_json::to_string_pretty(structured).unwrap_or_default();
        }
    }
    McpContent { text, images }
}

/// True when an embedded resource declares an image media type. Only a gate on
/// whether to *try* decoding a `blob` — what the bytes actually are is decided
/// by [`block_image`], not by this claim.
fn claims_image(resource: &Value) -> bool {
    resource
        .get("mimeType")
        .and_then(Value::as_str)
        .is_some_and(|mime| mime.starts_with("image/"))
}

/// Take the image out of a content block's base64 payload (`data` on an
/// `image` block, `blob` on an embedded resource).
///
/// The media type is sniffed from the decoded bytes by [`Image::from_bytes`],
/// never read from the block's `mimeType`: a server's claim is not evidence,
/// and a provider handed a mislabelled image rejects the whole request. The
/// same call applies the size cap, so a server cannot push an absurd payload
/// into history through this seam.
fn block_image(data: Option<&Value>) -> Result<Image> {
    use base64::Engine as _;
    let data = data
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("no base64 payload"))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .context("payload is not valid base64")?;
    Ok(Image::from_bytes(&bytes)?)
}

/// Cap `s` at `max` characters for error messages and logs.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Every MCP connection this process holds, and the plugin's half of
/// [`McpClient`].
///
/// Core's [`McpManager`](crate::mcp::McpManager) holds one of these as a
/// `Box<dyn McpClient>`, which is the whole of the seam: a surface keeps the
/// manager for the life of a session and never learns what is inside it.
pub struct Connections {
    connections: Vec<Arc<McpConnection>>,
}

impl Connections {
    /// Connect to every configured server. Servers that fail to connect are
    /// skipped with a warning so one bad server doesn't take down startup.
    ///
    /// The handshakes run **concurrently**. Each one spawns a process and waits
    /// on `initialize` — `npx -y @playwright/mcp@latest` is a couple of seconds
    /// of npm resolution before it says a word — and doing them one after
    /// another made startup the *sum* of every server's cold start, with a
    /// single unreachable one contributing the whole [`CONNECT_TIMEOUT`] to
    /// that sum. Concurrently it is the slowest one instead. Nothing about a
    /// connect depends on another connect having finished, so there is no
    /// ordering to preserve here beyond the one the results come back in, and
    /// `join_all` preserves that: `connections` stays in `mcp.toml` order, which
    /// is what decides which server wins an un-namespaced tool name in
    /// [`Self::tools`].
    pub async fn connect_all(config: &McpConfig) -> Result<Self> {
        // The duplicate check stays sequential and stays first: it is a pass
        // over names, and a second entry for a name is dropped without being
        // dialed at all.
        let mut seen_names: HashSet<&str> = HashSet::new();
        let mut wanted = Vec::with_capacity(config.servers.len());
        for server in &config.servers {
            if !seen_names.insert(server.name.as_str()) {
                warn!(
                    server = %server.name,
                    "duplicate MCP server name in mcp.toml; skipping later entry"
                );
                continue;
            }
            wanted.push(server.clone());
        }

        let dialed = futures_util::future::join_all(wanted.into_iter().map(|server| async move {
            let name = server.name.clone();
            (name, McpConnection::connect(server).await)
        }))
        .await;

        let mut connections = Vec::with_capacity(dialed.len());
        for (name, outcome) in dialed {
            match outcome {
                Ok(connection) => {
                    debug!(server = %name, "connected to MCP server");
                    connections.push(Arc::new(connection));
                }
                Err(err) => {
                    warn!(server = %name, "skipping MCP server (failed to connect): {err:#}");
                }
            }
        }
        Ok(Self { connections })
    }

    /// An empty manager (no servers configured).
    pub fn empty() -> Self {
        Self {
            connections: Vec::new(),
        }
    }

    /// Number of connected servers.
    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    /// List the tools of every connected server as registry-ready [`Tool`]
    /// objects. Tool names colliding across servers (or with native tools)
    /// are namespaced `server__tool`.
    pub async fn tools(&self) -> Result<Vec<Arc<dyn Tool>>> {
        // Concurrent for the same reason `connect_all` is: `tools/list` is a
        // round trip per server (paginated, up to [`MAX_LIST_PAGES`] of them,
        // each with its own [`LIST_TIMEOUT`]), and asking one server costs
        // nothing that asking another needs. `join_all` keeps the results in
        // connection order, which the namespacing below depends on being
        // stable.
        let listed = futures_util::future::join_all(self.connections.iter().map(|connection| {
            let connection = Arc::clone(connection);
            async move {
                let listing = connection.list_tools().await;
                (connection, listing)
            }
        }))
        .await;

        let mut listings: Vec<(Arc<McpConnection>, Vec<McpToolInfo>)> =
            Vec::with_capacity(listed.len());
        for (connection, listing) in listed {
            match listing {
                Ok(infos) => listings.push((connection, infos)),
                Err(err) => {
                    warn!(
                        server = %connection.name(),
                        "skipping MCP server tools (tools/list failed): {err:#}"
                    );
                }
            }
        }

        // Count plain names across all servers so collisions get namespaced.
        let mut name_counts: HashMap<String, usize> = HashMap::new();
        for (_, infos) in &listings {
            for info in infos {
                *name_counts.entry(info.name.clone()).or_default() += 1;
            }
        }

        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        let mut registered: HashSet<String> = HashSet::new();
        for (connection, infos) in listings {
            for info in infos {
                let collides = name_counts.get(info.name.as_str()).copied().unwrap_or(0) > 1
                    || RESERVED_TOOL_NAMES.contains(&info.name.as_str());
                let registered_name = if collides {
                    format!("{}__{}", connection.name(), info.name)
                } else {
                    info.name.clone()
                };
                if !registered.insert(registered_name.clone()) {
                    warn!(
                        server = %connection.name(),
                        tool = %info.name,
                        "duplicate MCP tool name even after namespacing; skipping"
                    );
                    continue;
                }
                tools.push(Arc::new(McpTool {
                    connection: Arc::clone(&connection),
                    info,
                    registered_name,
                }));
            }
        }
        Ok(tools)
    }
}

#[async_trait]
impl McpClient for Connections {
    async fn tools(&self) -> Result<Vec<Arc<dyn Tool>>> {
        Connections::tools(self).await
    }

    fn connection_count(&self) -> usize {
        self.connections.len()
    }

    /// Close every connection.
    ///
    /// Through the shared reference rather than by consuming `self`, because
    /// the tools in the registry that is about to be rebuilt still hold
    /// `Arc`s to these connections. The stdio child is `kill_on_drop` as a
    /// backstop, but a backstop that fires whenever the last `Arc` happens to
    /// go is not a shutdown — it is a race with whoever is still holding one.
    async fn disconnect(&self) {
        for connection in &self.connections {
            connection.shutdown().await;
        }
    }
}

/// The plugin's half of [`McpConnector`]: it makes [`Connections`], and it
/// answers `wizard doctor` about one server.
///
/// A unit struct because there is nothing to configure. Everything a connect
/// needs comes from the [`McpConfig`] it is handed, which is what makes the
/// service safe to register in an `apply` that runs synchronously, with no
/// tokio runtime, from a unit test.
pub struct Connector;

#[async_trait]
impl McpConnector for Connector {
    async fn connect(&self, config: McpConfig) -> Result<Box<dyn McpClient>> {
        Ok(Box::new(Connections::connect_all(&config).await?))
    }

    /// One handshake, one `tools/list`, one sentence.
    ///
    /// The budget is [`CONNECT_TIMEOUT`], the same one a session connects
    /// under, which is the reason this is the plugin's method rather than
    /// doctor connecting for itself: a diagnostic that is more patient than
    /// the runtime passes servers that will fail every session.
    async fn probe(&self, server: McpServerConfig) -> Result<String> {
        let name = server.name.clone();
        let connection = match timeout(CONNECT_TIMEOUT, McpConnection::connect(server)).await {
            Ok(result) => result?,
            Err(_) => bail!("no handshake within {}s", CONNECT_TIMEOUT.as_secs()),
        };
        // A server that shook hands and cannot list is still reachable, which
        // is the fact doctor was asking about, so the tool count degrades
        // rather than turning the whole check red.
        let detail = match timeout(CONNECT_TIMEOUT, connection.list_tools()).await {
            Ok(Ok(tools)) => format!("handshake ok, {} tool(s)", tools.len()),
            _ => "handshake ok".to_string(),
        };
        // Doctor probes and leaves; without this the stdio child outlives the
        // check that spawned it and `wizard doctor` on a machine with three
        // servers leaks three processes.
        connection.shutdown().await;
        debug!(server = %name, "MCP probe complete");
        Ok(detail)
    }
}

/// Adapter exposing one remote MCP tool through the [`Tool`] trait.
pub struct McpTool {
    connection: Arc<McpConnection>,
    info: McpToolInfo,
    /// Registry-unique name (possibly namespaced `server__tool`).
    registered_name: String,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.registered_name
    }

    fn description(&self) -> &str {
        self.info.description.as_deref().unwrap_or("MCP tool")
    }

    fn parameters(&self) -> Value {
        if self.info.input_schema.is_object() {
            self.info.input_schema.clone()
        } else {
            // Servers may omit inputSchema; advertise an empty object schema
            // so the model still emits valid calls.
            json!({ "type": "object", "properties": {} })
        }
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Mcp
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        if !(args.is_object() || args.is_null()) {
            return Err(ToolError::InvalidArgs {
                tool: self.registered_name.clone(),
                message: format!("expected a JSON object, got {}", json_type_name(&args)),
            });
        }
        let call = self.connection.call_tool(&self.info.name, args);
        match timeout(Duration::from_secs(CALL_TIMEOUT_SECS), call).await {
            // Images ride back even on a failed call: a browser tool that
            // reports a broken page still returns the screenshot of it.
            Ok(Ok((content, is_error))) => Ok(ToolOutput {
                content: content.text,
                is_error,
                images: content.images,
            }),
            Ok(Err(err)) => Err(ToolError::Execution {
                tool: self.registered_name.clone(),
                source: err,
            }),
            Err(_) => Err(ToolError::Timeout {
                tool: self.registered_name.clone(),
                seconds: CALL_TIMEOUT_SECS,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reserved_tool_names_match_the_native_registry() {
        use std::collections::BTreeSet;

        let mut compiled: HashSet<&str> = vec![
            crate::agent::subagent::SPAWN_SUBAGENT_TOOL_NAME,
            crate::tools::code::RUN_CODE_TOOL_NAME,
        ]
        .into_iter()
        .collect();
        // Native *and* plugin, because both are compiled in and both are ones
        // an MCP server would be shadowing. The Lua plugins have to be loaded
        // before they are in the kernel to be copied out of; nothing calls
        // `plugins::boot` in a test binary.
        crate::plugins::bundled::ensure().await;
        let mut registry = crate::tools::registry::ToolRegistry::with_native_tools();
        crate::plugins::install_tools_into(&mut registry);
        let specs = registry.specs();
        for spec in &specs {
            compiled.insert(spec.function.name.as_str());
        }
        let reserved: HashSet<&str> = RESERVED_TOOL_NAMES.iter().copied().collect();

        let unreserved: BTreeSet<&str> = compiled.difference(&reserved).copied().collect();
        assert!(
            unreserved.is_empty(),
            "RESERVED_TOOL_NAMES must track every tool this build registers; missing {unreserved:?}"
        );

        // The other direction is not equality, because a plugin tool stays
        // reserved on a build that left the plugin out — see the constant. So
        // the extras have to be exactly the tools of the plugins this build
        // does not have, and nothing else.
        let extra: BTreeSet<&str> = reserved.difference(&compiled).copied().collect();
        let mut absent: BTreeSet<&str> = BTreeSet::new();
        if !cfg!(feature = "tool-web") {
            absent.extend(["web_fetch", "web_search", "x_search"]);
        }
        if !cfg!(feature = "tool-git") {
            absent.extend(["git_status", "git_diff"]);
        }
        if !cfg!(feature = "tool-publish") {
            absent.extend(["publish"]);
        }
        if !cfg!(feature = "tool-json") {
            absent.extend(["json_query"]);
        }
        assert_eq!(
            extra, absent,
            "a reserved name that no compiled-in tool claims must be a plugin tool this \
             build left out"
        );
    }

    #[test]
    fn header_values_resolve_env_indirection() {
        // Literal values pass through untouched.
        assert_eq!(
            resolve_header_value("Bearer abc").expect("literal"),
            "Bearer abc"
        );
        // `env:VAR` reads the environment (PATH is always set for tests).
        let path = std::env::var("PATH").expect("PATH set");
        assert_eq!(resolve_header_value("env:PATH").expect("env"), path);
        // An unset variable is a hard error, not a silent empty header.
        let err = resolve_header_value("env:WIZARD_MCP_TEST_HEADER_NEVER_SET")
            .expect_err("unset var should error");
        assert!(err.to_string().contains("not set"), "got: {err}");
    }

    #[test]
    fn id_matching_accepts_number_and_string_forms() {
        assert!(id_matches(Some(&json!(7)), 7));
        assert!(id_matches(Some(&json!("7")), 7));
        assert!(!id_matches(Some(&json!(8)), 7));
        assert!(!id_matches(Some(&json!(null)), 7));
        assert!(!id_matches(None, 7));
    }

    #[test]
    fn extract_result_surfaces_jsonrpc_errors() {
        let ok = json!({"jsonrpc": "2.0", "id": 1, "result": {"tools": []}});
        assert_eq!(
            extract_result(ok, "srv").expect("result should extract"),
            json!({"tools": []})
        );

        let err = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": {"code": -32601, "message": "method not found"}
        });
        let message = extract_result(err, "srv")
            .expect_err("error should propagate")
            .to_string();
        assert!(message.contains("-32601"), "got: {message}");
        assert!(message.contains("method not found"), "got: {message}");
    }

    /// Base64 of a real (if tiny) PNG: the magic number is what `sniff_mime`
    /// reads, and the trailing bytes stand in for pixels.
    fn png_b64() -> String {
        b64(&[
            &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a][..],
            b"pixels",
        ]
        .concat())
    }

    /// Base64 of a real (if tiny) GIF, for telling two images apart.
    fn gif_b64() -> String {
        b64(b"GIF89a-pixels")
    }

    fn b64(bytes: &[u8]) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn decode_content_handles_mixed_blocks() {
        let result = json!({
            "content": [
                {"type": "text", "text": "hello"},
                {"type": "image", "mimeType": "image/png", "data": png_b64()},
                {"type": "resource", "resource": {"uri": "file:///a.txt", "text": "contents"}},
                {"type": "resource_link", "uri": "file:///b.txt"},
            ],
        });
        let content = decode_content(&result);
        assert_eq!(
            content.text,
            "hello\n[image content: image/png]\ncontents\n[resource link: file:///b.txt]",
            "the image keeps its marker in the text a text-only model reads"
        );
        assert_eq!(content.images.len(), 1);
        assert_eq!(content.images[0].mime, "image/png");
        assert_eq!(content.images[0].b64, png_b64(), "the payload rides whole");
        assert!(
            content.images[0].path.is_none(),
            "the agent's image store fills the path in, not us"
        );
    }

    #[test]
    fn decode_content_leaves_text_only_results_alone() {
        let result = json!({
            "content": [
                {"type": "text", "text": "line one"},
                {"type": "text", "text": "line two"},
            ],
        });
        let content = decode_content(&result);
        assert_eq!(content.text, "line one\nline two");
        assert!(content.images.is_empty());
    }

    #[test]
    fn decode_content_takes_every_image_in_order() {
        let result = json!({
            "content": [
                {"type": "image", "mimeType": "image/png", "data": png_b64()},
                {"type": "text", "text": "and another"},
                {"type": "image", "mimeType": "image/gif", "data": gif_b64()},
            ],
        });
        let content = decode_content(&result);
        let mimes: Vec<&str> = content
            .images
            .iter()
            .map(|image| image.mime.as_str())
            .collect();
        assert_eq!(mimes, ["image/png", "image/gif"]);
        assert_eq!(
            content.text,
            "[image content: image/png]\nand another\n[image content: image/gif]"
        );
    }

    #[test]
    fn decode_content_sniffs_the_media_type_instead_of_trusting_the_server() {
        // A PNG mislabelled as text: the bytes decide, so it still reaches the
        // model as an image — a provider handed `text/plain` would reject it.
        let result = json!({
            "content": [{"type": "image", "mimeType": "text/plain", "data": png_b64()}],
        });
        let content = decode_content(&result);
        assert_eq!(content.images.len(), 1);
        assert_eq!(content.images[0].mime, "image/png");
        assert_eq!(content.text, "[image content: image/png]");

        // And the other way round: text dressed up as an image is refused, not
        // handed on as a broken attachment.
        let result = json!({
            "content": [{"type": "image", "mimeType": "image/png", "data": b64(b"hello")}],
        });
        let content = decode_content(&result);
        assert!(content.images.is_empty());
        assert!(
            content
                .text
                .starts_with("[unusable image content: unrecognized image data"),
            "got: {}",
            content.text
        );
    }

    #[test]
    fn decode_content_degrades_honestly_on_a_broken_image_block() {
        let result = json!({
            "content": [
                {"type": "text", "text": "before"},
                {"type": "image", "mimeType": "image/png", "data": "!!! not base64 !!!"},
                {"type": "image", "mimeType": "image/png"},
                {"type": "image", "mimeType": "image/png", "data": 7},
                {"type": "text", "text": "after"},
            ],
        });
        let content = decode_content(&result);
        assert!(
            content.images.is_empty(),
            "nothing unusable reaches the model"
        );
        let lines: Vec<&str> = content.text.lines().collect();
        assert_eq!(lines.len(), 5, "every block still says something");
        assert_eq!(lines[0], "before");
        assert!(lines[1].contains("not valid base64"), "got: {}", lines[1]);
        assert!(lines[2].contains("no base64 payload"), "got: {}", lines[2]);
        assert!(lines[3].contains("no base64 payload"), "got: {}", lines[3]);
        assert_eq!(lines[4], "after", "a bad block never poisons the good ones");
    }

    #[test]
    fn decode_content_takes_images_out_of_embedded_resources() {
        let result = json!({
            "content": [
                {"type": "resource", "resource": {
                    "uri": "file:///shot.png", "mimeType": "image/png", "blob": png_b64(),
                }},
                // Claims an image, is not one: back to a plain binary resource.
                {"type": "resource", "resource": {
                    "uri": "file:///lies.png", "mimeType": "image/png", "blob": b64(b"hello"),
                }},
                // Never claimed to be an image: untouched, as before.
                {"type": "resource", "resource": {
                    "uri": "file:///a.bin", "mimeType": "application/octet-stream",
                    "blob": b64(b"binary"),
                }},
            ],
        });
        let content = decode_content(&result);
        assert_eq!(content.images.len(), 1);
        assert_eq!(content.images[0].mime, "image/png");
        assert_eq!(
            content.text,
            "[image content: image/png: file:///shot.png]\n[binary resource: file:///lies.png]\n\
             [binary resource: file:///a.bin]"
        );
    }

    #[test]
    fn decode_content_falls_back_to_structured_content() {
        let result = json!({
            "content": [],
            "structuredContent": {"answer": 42},
        });
        let content = decode_content(&result);
        assert!(content.text.contains("42"), "got: {}", content.text);
        assert!(content.images.is_empty());
    }

    /// A fake stdio MCP server implemented as a `sh` line loop: answers
    /// `initialize` (id 1), `tools/list` (id 2, one tool named `tool`), and
    /// `tools/call` (id 3) with `content` — a JSON array of content blocks.
    fn fake_server_returning(name: &str, tool: &str, content: &str) -> McpServerConfig {
        let script = format!(
            r#"while read -r line; do
  case "$line" in
    *'"initialize"'*) printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"2025-03-26","capabilities":{{}},"serverInfo":{{"name":"fake","version":"0"}}}}}}' ;;
    *'"tools/list"'*) printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"tools":[{{"name":"{tool}","description":"a fake tool","inputSchema":{{"type":"object","properties":{{}}}}}}]}}}}' ;;
    *'"tools/call"'*) printf '%s\n' '{{"jsonrpc":"2.0","id":3,"result":{{"content":{content},"isError":false}}}}' ;;
  esac
done"#
        );
        McpServerConfig {
            name: name.into(),
            transport: McpTransport::Stdio,
            command: Some("sh".into()),
            args: vec!["-c".into(), script],
            url: None,
            env: HashMap::new(),
            headers: HashMap::new(),
        }
    }

    /// The same fake server, answering `tools/call` with a line of text.
    fn fake_server_config(name: &str, tool: &str) -> McpServerConfig {
        let content = format!(r#"[{{"type":"text","text":"called {tool}"}}]"#);
        fake_server_returning(name, tool, &content)
    }

    #[tokio::test]
    async fn stdio_roundtrip_namespaces_collisions_and_calls_tools() {
        let config = McpConfig {
            servers: vec![
                fake_server_config("alpha", "weather"),
                fake_server_config("beta", "weather"),
                fake_server_config("gamma", "read_file"),
            ],
        };
        let manager = Connections::connect_all(&config)
            .await
            .expect("connect_all never hard-fails");
        let tools = manager.tools().await.expect("tools/list should succeed");
        let mut names: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();
        names.sort_unstable();
        // `weather` collides across servers; `read_file` shadows a native.
        assert_eq!(
            names,
            vec!["alpha__weather", "beta__weather", "gamma__read_file"]
        );

        let ctx = ToolContext::new(std::env::temp_dir());
        let tool = tools
            .iter()
            .find(|tool| tool.name() == "alpha__weather")
            .expect("alpha__weather should be registered");
        assert_eq!(tool.access(), crate::tools::ToolAccess::Execute);
        assert_eq!(tool.kind(), ToolKind::Mcp);
        let output = tool
            .execute(json!({}), &ctx)
            .await
            .expect("tools/call should succeed");
        assert!(!output.is_error);
        assert_eq!(output.content, "called weather");
        assert!(output.images.is_empty(), "a text result carries no images");
    }

    /// The whole seam, over a real stdio transport: a server returns a
    /// screenshot, and it lands on `ToolOutput::images` for the agent loop to
    /// persist and announce.
    #[tokio::test]
    async fn a_screenshot_from_a_server_lands_on_tool_output_images() {
        let content = format!(
            r#"[{{"type":"text","text":"the page"}},{{"type":"image","mimeType":"image/png","data":"{}"}}]"#,
            png_b64()
        );
        let config = McpConfig {
            servers: vec![fake_server_returning("browser", "screenshot", &content)],
        };
        let manager = Connections::connect_all(&config)
            .await
            .expect("connect_all never hard-fails");
        let tools = manager.tools().await.expect("tools/list should succeed");
        let output = tools[0]
            .execute(json!({}), &ToolContext::new(std::env::temp_dir()))
            .await
            .expect("tools/call should succeed");

        assert!(!output.is_error);
        assert_eq!(output.content, "the page\n[image content: image/png]");
        assert_eq!(output.images.len(), 1);
        assert_eq!(output.images[0].mime, "image/png");
        assert_eq!(output.images[0].b64, png_b64());
    }

    #[tokio::test]
    async fn one_bad_server_does_not_take_down_startup() {
        let config = McpConfig {
            servers: vec![
                McpServerConfig {
                    name: "broken".into(),
                    transport: McpTransport::Stdio,
                    command: Some("/nonexistent/wizard-mcp-server".into()),
                    args: vec![],
                    url: None,
                    env: HashMap::new(),
                    headers: HashMap::new(),
                },
                McpServerConfig {
                    name: "misconfigured".into(),
                    transport: McpTransport::Http,
                    command: None,
                    args: vec![],
                    url: None, // http transport without a url
                    env: HashMap::new(),
                    headers: HashMap::new(),
                },
                fake_server_config("good", "weather"),
            ],
        };
        let manager = Connections::connect_all(&config)
            .await
            .expect("bad servers are skipped, not fatal");
        let tools = manager.tools().await.expect("tools should list");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "weather");
    }

    #[test]
    fn filter_config_env_drops_dynamic_linker_vars() {
        let mut env: HashMap<String, String> = HashMap::from([
            ("PATH".to_string(), "/custom/bin".to_string()),
            ("API_KEY".to_string(), "secret".to_string()),
        ]);
        for key in STDIO_ENV_DENYLIST {
            env.insert((*key).to_string(), "/evil.so".to_string());
        }
        let (allowed, denied) = filter_config_env(&env);

        let mut expected_denied: Vec<String> =
            STDIO_ENV_DENYLIST.iter().map(|s| s.to_string()).collect();
        expected_denied.sort_unstable();
        assert_eq!(denied, expected_denied);
        assert_eq!(allowed.len(), 2);
        assert_eq!(allowed["PATH"], "/custom/bin");
        assert_eq!(allowed["API_KEY"], "secret");
    }

    /// End to end: the child must see config-supplied vars, but neither the
    /// denylisted linker vars nor the parent's environment (cargo always
    /// sets `CARGO_MANIFEST_DIR` for test binaries).
    #[tokio::test]
    async fn stdio_child_env_is_cleared_and_filtered() {
        let script = r#"while read -r line; do
  case "$line" in
    *'"initialize"'*) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{},"serverInfo":{"name":"env","version":"0"}}}' ;;
    *'"env/echo"'*) printf '{"jsonrpc":"2.0","id":2,"result":{"ok":"%s","ld":"%s","manifest":"%s"}}\n' "${WIZARD_MCP_TEST_OK:-unset}" "${LD_PRELOAD:-unset}" "${CARGO_MANIFEST_DIR:-unset}" ;;
  esac
done"#;
        let connection = McpConnection::connect(McpServerConfig {
            name: "envprobe".into(),
            transport: McpTransport::Stdio,
            command: Some("sh".into()),
            args: vec!["-c".into(), script.into()],
            url: None,
            env: HashMap::from([
                ("WIZARD_MCP_TEST_OK".to_string(), "yes".to_string()),
                ("LD_PRELOAD".to_string(), "/tmp/evil.so".to_string()),
            ]),
            headers: HashMap::new(),
        })
        .await
        .expect("env probe server should connect");
        let result = connection
            .request("env/echo", json!({}))
            .await
            .expect("env/echo should answer");
        assert_eq!(result["ok"], "yes");
        assert_eq!(result["ld"], "unset");
        assert_eq!(result["manifest"], "unset");
        connection.close().await.ok();
    }

    /// A server that floods mismatched-id responses and then stalls must
    /// produce a bounded error, not read stdout until EOF or timeout.
    #[tokio::test]
    async fn stale_response_flood_fails_with_bounded_error() {
        let script = r#"read -r line
i=0
while [ "$i" -lt 60 ]; do
  printf '%s\n' '{"jsonrpc":"2.0","id":999,"result":{}}'
  i=$((i+1))
done
exec sleep 60"#;
        let result = McpConnection::connect(McpServerConfig {
            name: "flood".into(),
            transport: McpTransport::Stdio,
            command: Some("sh".into()),
            args: vec!["-c".into(), script.into()],
            url: None,
            env: HashMap::new(),
            headers: HashMap::new(),
        })
        .await;
        let Err(err) = result else {
            panic!("a flood of stale responses should fail the request");
        };
        let message = format!("{err:#}");
        assert!(message.contains("flood"), "got: {message}");
        assert!(message.contains("mismatched ids"), "got: {message}");
    }

    /// A server that crashes mid-session is respawned once (fresh process +
    /// handshake, the in-flight request retried); a second crash surfaces the
    /// error with the `/reload` hint instead of respawning again.
    #[tokio::test]
    async fn stdio_crash_respawns_once_then_hints_at_reload() {
        let marker =
            std::env::temp_dir().join(format!("wizard-mcp-respawn-marker-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        // Echoes each request id; on `ping`, crashes (exit before answering)
        // unless the marker file exists, dropping the marker so the respawned
        // incarnation answers.
        let script = r#"while read -r line; do
  id="${line#*\"id\":}"; id="${id%%,*}"
  case "$line" in
    *'"method":"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-03-26","capabilities":{},"serverInfo":{"name":"crashy","version":"0"}}}\n' "$id" ;;
    *'"method":"ping"'*)
      if [ -e "$MARKER" ]; then
        printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
      else
        : > "$MARKER"
        exit 0
      fi ;;
  esac
done"#;
        let connection = McpConnection::connect(McpServerConfig {
            name: "crashy".into(),
            transport: McpTransport::Stdio,
            command: Some("sh".into()),
            args: vec!["-c".into(), script.into()],
            url: None,
            env: HashMap::from([("MARKER".to_string(), marker.to_string_lossy().into_owned())]),
            headers: HashMap::new(),
        })
        .await
        .expect("crashy server should connect");

        // First ping crashes the child; the automatic respawn answers it.
        let result = connection
            .request("ping", json!({}))
            .await
            .expect("respawn should recover the request");
        assert_eq!(result["ok"], true);

        // Second crash: the respawn is spent, so the error surfaces with the
        // /reload hint.
        std::fs::remove_file(&marker).expect("clear marker");
        let err = connection
            .request("ping", json!({}))
            .await
            .expect_err("second crash should not respawn again");
        let message = format!("{err:#}");
        assert!(message.contains("run /reload"), "got: {message}");

        let _ = std::fs::remove_file(&marker);
        connection.close().await.ok();
    }

    /// A request the server never answers must hit the per-request timeout
    /// instead of hanging. Time is paused after the handshake so the 120s
    /// budget elapses instantly.
    #[tokio::test]
    async fn stdio_request_times_out_when_server_goes_silent() {
        let connection = McpConnection::connect(fake_server_config("quiet", "tool"))
            .await
            .expect("fake server should connect");
        // The fake server's `case` matches no pattern for this method and
        // prints nothing, so the client would otherwise wait forever.
        tokio::time::pause();
        let err = connection
            .request("wizard/unhandled", json!({}))
            .await
            .expect_err("an unanswered request should time out");
        tokio::time::resume();
        let message = format!("{err:#}");
        assert!(message.contains("quiet"), "got: {message}");
        assert!(message.contains("did not answer"), "got: {message}");
        connection.close().await.ok();
    }

    #[test]
    fn truncate_caps_long_strings() {
        assert_eq!(truncate("short", 10), "short");
        let long = "x".repeat(20);
        let cut = truncate(&long, 10);
        assert!(cut.starts_with("xxxxxxxxxx") && cut.ends_with('…'));
    }
}
