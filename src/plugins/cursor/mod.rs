//! Cursor models on a Cursor subscription, as a plugin: `kind = "cursor"`
//! behind `--features provider-cursor`.
//!
//! Cursor allows subscription sign-in only through its own tools, so this
//! provider never holds a credential and never opens a connection. Each model
//! call spawns the user's own `cursor-agent -p` and reads its `stream-json`
//! events; signing in is `cursor-agent login`, and whether that happened is
//! whatever `cursor-agent status` says. Nothing here reads Cursor's files.
//!
//! Wizard keeps its own prompt and tools. `cursor-agent` has no flag for
//! either, so the child runs in a throwaway workspace that carries them: the
//! system prompt as `AGENTS.md`, Cursor's own shell, file and web tools denied
//! in `.cursor/cli.json`, and Wizard's tool specs as an MCP server in
//! `.cursor/mcp.json`. A tool call reaches the agent loop through that server,
//! which holds it open while the loop runs the tool itself. See [`bridge`].
//!
//! A child's turn ends when it stops calling tools, so only a child that is
//! waiting on tool results outlives a call. The next call after a finished turn
//! starts a new child, and since the prompt goes in as text, a history is
//! replayed as one labelled transcript: see [`prompt`].

mod bridge;
pub mod setup;

use std::collections::HashMap;
use std::ffi::OsStr;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use futures_util::stream;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdout};
use tokio::task::JoinHandle;

use self::bridge::{Bridge, Call};
use crate::config::ProviderConfig;
use crate::kernel::{Capability, Ctx, Plugin, PluginManifest};
use crate::llm::provider::LlmProvider;
use crate::llm::registry::{Credentials, ProviderDescriptor, ProviderKind};
use crate::llm::{
    CacheTokens, ChatChunk, ChatMessage, ChatRequest, ChatStream, ContentBlock, FunctionCall, Role,
    ToolCall,
};

/// Cursor's installer, as https://cursor.com/cli gives it.
pub const INSTALL_COMMAND: &str = "curl https://cursor.com/install -fsS | bash";

pub const LOGIN_COMMAND: &str = "cursor-agent login";

/// Cursor's own router, which picks a model per request.
const DEFAULT_MODEL: &str = "auto";

/// Idle children kept for the conversations most likely to come back: the
/// main loop and a few subagents.
const KEPT: usize = 4;
const IDLE_LIMIT: Duration = Duration::from_secs(15 * 60);

/// Calls the model makes together reach the bridge within a few milliseconds
/// of each other.
const CALL_BATCH: Duration = Duration::from_millis(150);

/// Put ahead of Wizard's system prompt, since the denied tools are still
/// listed to the model and it spends turns on them otherwise.
const WORKSPACE_NOTE: &str = "Your own shell, file and web tools are disabled in this workspace \
     and always fail. Use the tools of the `wizard` MCP server for everything, and never retry a \
     disabled one.";

const TRANSCRIPT_PREAMBLE: &str = "This is the conversation so far, oldest turn first. Each turn \
     starts with a line naming who spoke, like [user] or [assistant]. Write the next assistant \
     turn only, without the label.";

/// What Cursor's own tools would otherwise do on the host. Wizard runs every
/// tool itself, so these are denied and only the bridge's are allowed.
const PERMISSIONS: &str = r#"{"permissions":{"allow":["Mcp(wizard:*)"],"deny":["Shell(*)","Read(**)","Write(**)","WebFetch(*)"]}}"#;

/// A result for each tool call a session is waiting on, by call id.
type Results = Vec<(String, Vec<Value>)>;

static IDLE: Mutex<Vec<Session>> = Mutex::new(Vec::new());

/// The entry `/provider` writes. The zero rates are what keep `/cost` from
/// pricing a subscription by the token.
pub fn provider_config() -> ProviderConfig {
    ProviderConfig {
        name: "cursor".to_string(),
        kind: ProviderKind::CURSOR,
        model: DEFAULT_MODEL.to_string(),
        usd_per_mtok_in: Some(0.0),
        usd_per_mtok_out: Some(0.0),
        ..ProviderConfig::default()
    }
}

/// An installed `cursor-agent`.
#[derive(Debug, Clone)]
pub struct Cli {
    program: PathBuf,
}

impl Cli {
    pub fn find() -> Option<Self> {
        Self::find_in(std::env::var_os("PATH").as_deref(), dirs::home_dir())
    }

    /// `cursor-agent` in one of `path`'s directories, or in `~/.local/bin`,
    /// which is where the installer puts it and which a running shell may not
    /// have on its `PATH` yet.
    fn find_in(path: Option<&OsStr>, home: Option<PathBuf>) -> Option<Self> {
        let name = if cfg!(windows) {
            "cursor-agent.exe"
        } else {
            "cursor-agent"
        };
        path.into_iter()
            .flat_map(std::env::split_paths)
            .chain(home.map(|home| home.join(".local").join("bin")))
            .map(|dir| dir.join(name))
            .find(|candidate| crate::platform::host::is_executable(candidate))
            .map(|program| Self { program })
    }

    fn command(&self) -> std::process::Command {
        std::process::Command::new(&self.program)
    }

    pub fn signed_in(&self) -> Result<bool> {
        let output = self
            .command()
            .args(["status", "--format", "json"])
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("running {}", self.program.display()))?;
        let status: Value = serde_json::from_slice(&output.stdout)
            .context("`cursor-agent status --format json` did not print JSON")?;
        Ok(status["isAuthenticated"].as_bool().unwrap_or(false))
    }

    /// Cursor's own sign-in, on the terminal Wizard was given.
    pub fn login(&self) -> Result<()> {
        self.command()
            .arg("login")
            .status()
            .with_context(|| format!("running {}", self.program.display()))?;
        Ok(())
    }

    /// The model ids this account can use, from the `id - Name` rows of
    /// `cursor-agent models`.
    fn models(&self) -> Result<Vec<String>> {
        let output = self
            .command()
            .arg("models")
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("running {}", self.program.display()))?;
        if !output.status.success() {
            bail!("`cursor-agent models` failed: {}", output.status);
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.split_once(" - "))
            .map(|(id, _)| id.trim().to_string())
            .collect())
    }
}

/// What a run that cannot ask is told: one line naming the command to run.
fn ready(cli: Option<&Cli>) -> Result<()> {
    let cli = cli.ok_or_else(not_installed)?;
    if !cli.signed_in()? {
        bail!("Cursor is not signed in. Run: {LOGIN_COMMAND}");
    }
    Ok(())
}

fn not_installed() -> anyhow::Error {
    anyhow!("Cursor is not installed. Install it with: {INSTALL_COMMAND}")
}

struct CursorProvider {
    cli: Option<Cli>,
    model: String,
}

#[async_trait]
impl LlmProvider for CursorProvider {
    async fn health(&self) -> Result<()> {
        let cli = self.cli.clone();
        tokio::task::spawn_blocking(move || ready(cli.as_ref()))
            .await
            .context("the Cursor check panicked")?
    }

    async fn supports_native_tools(&self, _model: &str) -> Result<bool> {
        Ok(true)
    }

    /// Cursor does not say, and without a window the loop falls back to
    /// compacting on a few dozen kilobytes.
    async fn context_window(&self, _model: &str) -> Option<u32> {
        Some(200_000)
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        let cli = self.cli.clone().ok_or_else(not_installed)?;
        tokio::task::spawn_blocking(move || cli.models())
            .await
            .context("the Cursor model list panicked")?
    }

    async fn chat_stream(&self, request: ChatRequest) -> Result<ChatStream> {
        let cli = self.cli.as_ref().ok_or_else(not_installed)?;
        let (system, _) = split_system(&request.messages);
        let key = key(cli, &request, &system);

        let mut session = match take(key, &request.messages) {
            Some((mut session, results)) => {
                for (id, content) in results {
                    session.answer(&id, content);
                }
                session
            }
            None => {
                tracing::debug!(messages = request.messages.len(), "starting a cursor child");
                Session::open(cli, &request, &system, key).await?
            }
        };
        session.seen = request.messages.len();
        session.said.clear();

        Ok(Box::pin(stream::try_unfold(
            Some(session),
            |session| async move {
                let Some(mut session) = session else {
                    return Ok(None);
                };
                let chunk = session.next().await?;
                if chunk.done {
                    if !session.pending.is_empty() {
                        keep(session);
                    }
                    return Ok(Some((chunk, None)));
                }
                Ok(Some((chunk, Some(session))))
            },
        )))
    }

    fn label(&self) -> String {
        format!("cursor:{}", self.model)
    }
}

/// An exported API key would outrank the subscription sign-in inside Cursor
/// and bill the turn to it instead.
fn without_api_keys(command: &mut tokio::process::Command) -> &mut tokio::process::Command {
    command.env_remove("CURSOR_API_KEY")
}

/// A `cursor-agent` child, alive from the call that started it until its turn
/// ends or the loop stops coming back for its tool results.
struct Session {
    /// The child's fixed half: binary, model, system prompt, tools.
    key: u64,
    bridge: Bridge,
    lines: Lines<BufReader<ChildStdout>>,
    child: Child,
    stderr: JoinHandle<String>,
    _workspace: tempfile::TempDir,
    /// How many messages the request that produced the last reply held.
    seen: usize,
    /// Tool calls the child is waiting on. Empty once its turn has ended.
    pending: HashMap<String, Call>,
    /// The text of the reply in progress, then of the last one given.
    said: String,
    used: Instant,
}

impl Session {
    async fn open(cli: &Cli, request: &ChatRequest, system: &str, key: u64) -> Result<Self> {
        let bridge = Bridge::start(&request.tools).await?;
        let workspace = tempfile::tempdir().context("making the Cursor workspace")?;
        let write = |path: &str, text: &str| -> Result<()> {
            let path = workspace.path().join(path);
            std::fs::create_dir_all(path.parent().expect("a file path"))?;
            std::fs::write(path, text)?;
            Ok(())
        };
        write("AGENTS.md", &format!("{WORKSPACE_NOTE}\n\n{system}"))
            .context("writing the system prompt for Cursor")?;
        write(".cursor/cli.json", PERMISSIONS)?;
        write(".cursor/mcp.json", &bridge.config().to_string())
            .context("writing the tool bridge config for Cursor")?;

        let mut command = tokio::process::Command::new(&cli.program);
        command
            .arg("-p")
            .args(["--output-format", "stream-json", "--stream-partial-output"])
            .args(["--trust", "--approve-mcps"])
            .arg("--workspace")
            .arg(workspace.path())
            .args(["--model", &request.model])
            .current_dir(workspace.path());
        let mut child = without_api_keys(&mut command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting {}", cli.program.display()))?;

        let mut stdin = child.stdin.take().context("cursor-agent has no stdin")?;
        let input = prompt(&request.messages);
        tokio::spawn(async move {
            let _ = stdin.write_all(input.as_bytes()).await;
        });
        let stdout = child.stdout.take().context("cursor-agent has no stdout")?;
        let mut stderr = child.stderr.take().context("cursor-agent has no stderr")?;
        let stderr = tokio::spawn(async move {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text).await;
            text
        });
        Ok(Self {
            key,
            bridge,
            lines: BufReader::new(stdout).lines(),
            child,
            stderr,
            _workspace: workspace,
            seen: 0,
            pending: HashMap::new(),
            said: String::new(),
            used: Instant::now(),
        })
    }

    fn answer(&mut self, id: &str, content: Vec<Value>) {
        if let Some(call) = self.pending.remove(id) {
            call.answer(content);
        }
    }

    /// The results of every call this session is waiting on, if `messages` is
    /// its conversation one step on: the reply this session gave, then a
    /// result for each call. Anything else the loop added rides along with
    /// the last result, which is where the model reads it from.
    fn continues(&self, messages: &[ChatMessage]) -> Option<Results> {
        if self.bridge.expired() {
            return None;
        }
        let (reply, added) = messages.get(self.seen..)?.split_first()?;
        let said: String = reply
            .content
            .iter()
            .filter_map(ContentBlock::as_text)
            .collect();
        if self.pending.is_empty()
            || reply.role != Role::Assistant
            || said.trim() != self.said.trim()
            || added.iter().any(|message| message.role == Role::Assistant)
        {
            return None;
        }

        let mut results = Results::new();
        let mut loose = Vec::new();
        for block in added.iter().flat_map(|message| &message.content) {
            match block {
                ContentBlock::ToolResult(result)
                    if self.pending.contains_key(&result.tool_use_id) =>
                {
                    let mut content = Vec::new();
                    push_text(&mut content, result.content.clone());
                    results.push((result.tool_use_id.clone(), content));
                }
                ContentBlock::Text(text) => push_text(&mut loose, text.text.clone()),
                ContentBlock::Image(image) => loose.push(json!({
                    "type": "image", "data": image.b64, "mimeType": image.mime,
                })),
                _ => {}
            }
        }
        let answered = |id: &String| results.iter().any(|(answered, _)| answered == id);
        if !self.pending.keys().all(answered) {
            return None;
        }
        results.last_mut()?.1.extend(loose);
        Some(results)
    }

    async fn next(&mut self) -> Result<ChatChunk> {
        loop {
            tokio::select! {
                Some(call) = self.bridge.calls.recv() => return self.tool_use(call).await,
                line = self.lines.next_line() => {
                    let Some(line) = line? else { break };
                    let Ok(event) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    if let Some(chunk) = self.decode(&event)? {
                        return Ok(chunk);
                    }
                }
            }
        }
        let status = self.child.wait().await?;
        let stderr = (&mut self.stderr).await.unwrap_or_default();
        bail!(
            "cursor-agent exited before finishing the reply ({status}): {}",
            stderr.trim()
        );
    }

    /// The chunk that ends a model step on `first` and whatever calls arrive
    /// with it.
    async fn tool_use(&mut self, first: Call) -> Result<ChatChunk> {
        let mut batch = vec![first];
        while let Ok(Some(call)) = tokio::time::timeout(CALL_BATCH, self.bridge.calls.recv()).await
        {
            batch.push(call);
        }
        let calls = batch
            .iter()
            .map(|call| ToolCall {
                id: call.id.clone(),
                function: FunctionCall {
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                },
            })
            .collect();
        self.pending
            .extend(batch.into_iter().map(|call| (call.id.clone(), call)));
        let message = ChatMessage::assistant_turn("", Vec::new(), calls);
        Ok(chunk(
            true,
            false,
            Some(message),
            &Value::Null,
            Some("tool_use"),
        ))
    }

    /// One `stream-json` event as a chunk. Thinking and text deltas stream
    /// live; the `result` event closes the turn and carries the usage Cursor
    /// counted. Everything else on the stream, Cursor's own tool calls
    /// included, is bookkeeping.
    fn decode(&mut self, event: &Value) -> Result<Option<ChatChunk>> {
        match event["type"].as_str() {
            Some("thinking") if event["subtype"] == "delta" => {
                let text = event["text"].as_str().unwrap_or_default();
                Ok(Some(chunk(
                    false,
                    true,
                    Some(ChatMessage::assistant(text)),
                    &Value::Null,
                    None,
                )))
            }
            // The text deltas carry a timestamp; the message that repeats them
            // whole does not.
            Some("assistant") if event.get("timestamp_ms").is_some() => {
                let text = event["message"]["content"][0]["text"]
                    .as_str()
                    .unwrap_or_default();
                self.said.push_str(text);
                Ok(Some(chunk(
                    false,
                    false,
                    Some(ChatMessage::assistant(text)),
                    &Value::Null,
                    None,
                )))
            }
            Some("result") => {
                if event["is_error"].as_bool().unwrap_or(false) {
                    bail!(
                        "Cursor: {}",
                        event["result"].as_str().unwrap_or("the turn failed")
                    );
                }
                Ok(Some(chunk(
                    true,
                    false,
                    None,
                    &event["usage"],
                    Some("end_turn"),
                )))
            }
            _ => Ok(None),
        }
    }
}

fn chunk(
    done: bool,
    thinking: bool,
    message: Option<ChatMessage>,
    usage: &Value,
    stop: Option<&str>,
) -> ChatChunk {
    let count = |field: &str| usage[field].as_u64().unwrap_or(0);
    let cache = CacheTokens {
        read: count("cacheReadTokens"),
        write: count("cacheWriteTokens"),
    };
    ChatChunk {
        message,
        images: Vec::new(),
        thinking,
        done,
        done_reason: stop.map(str::to_string),
        eval_count: done.then(|| count("outputTokens")),
        prompt_eval_count: done.then(|| count("inputTokens") + cache.read + cache.write),
        cache,
        reasoning_eval_count: None,
        billed_ticks: None,
    }
}

fn key(cli: &Cli, request: &ChatRequest, system: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    cli.program.hash(&mut hasher);
    request.model.hash(&mut hasher);
    system.hash(&mut hasher);
    serde_json::to_string(&request.tools)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

fn take(key: u64, messages: &[ChatMessage]) -> Option<(Session, Results)> {
    let mut idle = IDLE.lock().expect("session lock");
    idle.retain(|session| session.used.elapsed() < IDLE_LIMIT);
    let (index, results) = idle.iter().enumerate().find_map(|(index, session)| {
        (session.key == key)
            .then(|| session.continues(messages))
            .flatten()
            .map(|results| (index, results))
    })?;
    Some((idle.remove(index), results))
}

fn keep(mut session: Session) {
    session.used = Instant::now();
    let mut idle = IDLE.lock().expect("session lock");
    if idle.len() == KEPT {
        idle.remove(0);
    }
    idle.push(session);
}

fn push_text(content: &mut Vec<Value>, text: String) {
    if !text.is_empty() {
        content.push(json!({"type": "text", "text": text}));
    }
}

/// The leading system messages as the system prompt, and everything after.
fn split_system(messages: &[ChatMessage]) -> (String, &[ChatMessage]) {
    let leading = messages
        .iter()
        .take_while(|message| message.role == Role::System)
        .count();
    let (system, rest) = messages.split_at(leading);
    let system = system
        .iter()
        .map(ChatMessage::text)
        .collect::<Vec<_>>()
        .join("\n\n");
    (system, rest)
}

/// Wizard's history after the system prompt as `cursor-agent`'s prompt.
///
/// A lone user message goes in as it is. A longer history becomes a labelled
/// transcript. Images do not go in: `cursor-agent -p` takes text.
fn prompt(messages: &[ChatMessage]) -> String {
    let (_, rest) = split_system(messages);
    if let [only] = rest {
        return only.text();
    }
    let mut transcript = format!("{TRANSCRIPT_PREAMBLE}\n\n");
    for message in rest {
        let who = match message.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        transcript.push_str(&format!("[{who}]\n"));
        for block in &message.content {
            match block {
                ContentBlock::Text(block) => transcript.push_str(&block.text),
                ContentBlock::ToolUse(call) => transcript.push_str(
                    &json!({"tool": call.function.name, "arguments": call.function.arguments})
                        .to_string(),
                ),
                ContentBlock::ToolResult(result) => transcript.push_str(&format!(
                    "Tool result for `{}`:\n{}",
                    result.name, result.content
                )),
                ContentBlock::Image(_) => transcript.push_str("[image]"),
                ContentBlock::Thinking(_) => {}
            }
        }
        transcript.push_str("\n\n");
    }
    transcript
}

pub fn descriptor() -> ProviderDescriptor {
    ProviderDescriptor::new(
        ProviderKind::CURSOR,
        "Cursor (subscription)",
        Credentials::Account {
            login: "cursor".to_string(),
        },
        |config| {
            Ok(Arc::new(CursorProvider {
                cli: Cli::find(),
                model: config.model.clone(),
            }))
        },
    )
}

pub struct CursorPlugin {
    manifest: PluginManifest,
}

impl CursorPlugin {
    pub fn new() -> Self {
        Self {
            manifest: PluginManifest {
                name: "cursor".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                description: "Cursor models on a subscription, through cursor-agent".to_string(),
                capabilities: vec![Capability::Process],
                optional_deps: Vec::new(),
                profiles: vec!["server".to_string(), "default".to_string()],
            },
        }
    }
}

impl Default for CursorPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for CursorPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn apply(&self, ctx: &mut Ctx) -> anyhow::Result<()> {
        ctx.provider(descriptor())?;
        Ok(())
    }
}

/// A directory to search for `cursor-agent`, for tests: `tests/fixtures/fake-cursor`
/// symlinked in as `cursor-agent`, answering from the files beside the link, so
/// no test needs a real sign-in.
#[cfg(all(test, unix))]
pub(crate) mod fake {
    use std::ffi::OsString;
    use std::path::PathBuf;

    const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake-cursor");

    pub(crate) struct FakeCursor {
        dir: tempfile::TempDir,
    }

    impl FakeCursor {
        pub(crate) fn new(installed: bool) -> Self {
            let fake = Self {
                dir: tempfile::tempdir().expect("temp dir"),
            };
            if installed {
                std::os::unix::fs::symlink(SCRIPT, fake.file("cursor-agent"))
                    .expect("link the fake");
            }
            fake
        }

        pub(crate) fn signed_in() -> Self {
            let fake = Self::new(true);
            fake.write("signed-in", "");
            fake
        }

        pub(crate) fn path(&self) -> OsString {
            self.dir.path().as_os_str().to_owned()
        }

        pub(crate) fn file(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }

        pub(crate) fn write(&self, name: &str, contents: &str) {
            std::fs::write(self.file(name), contents).expect("write fixture");
        }

        pub(crate) fn read(&self, name: &str) -> String {
            std::fs::read_to_string(self.file(name)).expect("read fixture")
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use futures_util::StreamExt;

    use super::fake::FakeCursor;
    use super::*;
    use crate::llm::ToolSpec;

    /// Recorded from `cursor-agent -p --output-format stream-json
    /// --stream-partial-output` 2026.10.01, trimmed to the fields read here
    /// plus one of each event that is skipped.
    const EVENTS: &str = r#"{"type":"system","subtype":"init","apiKeySource":"login","model":"Composer 2.5"}
{"type":"user","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}
{"type":"thinking","subtype":"delta","text":"Recall it.","timestamp_ms":1}
{"type":"thinking","subtype":"completed","timestamp_ms":2}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Pli"}]},"timestamp_ms":3}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"nth."}]},"timestamp_ms":4}
{"type":"tool_call","subtype":"started","call_id":"t1","tool_call":{"readToolCall":{"args":{"path":"a"}}}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Plinth."}]}}
{"type":"result","subtype":"success","is_error":false,"result":"Plinth.","usage":{"inputTokens":2,"outputTokens":7,"cacheReadTokens":40,"cacheWriteTokens":717}}
"#;
    const CALL_EVENTS: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Reading."}]},"timestamp_ms":1}
"#;
    const CALL: &str = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"read_file","arguments":{"path":"a"}}}"#;

    fn provider(fake: &FakeCursor) -> CursorProvider {
        CursorProvider {
            cli: Cli::find_in(Some(&fake.path()), None),
            model: "auto".to_string(),
        }
    }

    fn request(messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest {
            model: "composer-2.5".to_string(),
            messages,
            tools: Vec::new(),
            stream: true,
            options: None,
        }
    }

    fn with_tool(mut request: ChatRequest) -> ChatRequest {
        request.tools = vec![ToolSpec::function(
            "read_file",
            "Read a file.",
            json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        )];
        request
    }

    async fn collect(provider: &CursorProvider, request: ChatRequest) -> Vec<ChatChunk> {
        let stream = provider.chat_stream(request).await.expect("stream starts");
        stream
            .map(|chunk| chunk.expect("chunk"))
            .collect::<Vec<_>>()
            .await
    }

    #[tokio::test]
    async fn stream_json_events_become_chunks_with_cursors_usage() {
        let fake = FakeCursor::signed_in();
        fake.write("events", EVENTS);

        let chunks = collect(
            &provider(&fake),
            request(vec![
                ChatMessage::system("You are Wizard."),
                ChatMessage::user("What is my favorite word?"),
            ]),
        )
        .await;

        let said: Vec<(String, bool)> = chunks
            .iter()
            .filter_map(|chunk| Some((chunk.message.as_ref()?.text(), chunk.thinking)))
            .collect();
        assert_eq!(
            said,
            [
                ("Recall it.".to_string(), true),
                ("Pli".to_string(), false),
                ("nth.".to_string(), false),
            ]
        );
        let last = chunks.last().expect("a final chunk");
        assert!(last.done);
        assert_eq!(last.done_reason.as_deref(), Some("end_turn"));
        assert_eq!(last.prompt_eval_count, Some(2 + 40 + 717));
        assert_eq!(
            last.cache,
            CacheTokens {
                read: 40,
                write: 717
            }
        );
        assert_eq!(last.eval_count, Some(7));
    }

    #[tokio::test]
    async fn the_child_gets_wizards_prompt_and_tools_in_a_workspace_with_cursors_own_denied() {
        let fake = FakeCursor::signed_in();
        fake.write("events", EVENTS);

        collect(
            &provider(&fake),
            request(vec![
                ChatMessage::system("You are Wizard."),
                ChatMessage::user("hi"),
            ]),
        )
        .await;

        let system = fake.read("system");
        assert!(system.ends_with("\n\nYou are Wizard."), "{system}");
        assert!(system.contains("`wizard` MCP server"), "{system}");
        let permissions: Value = serde_json::from_str(&fake.read("permissions")).expect("JSON");
        assert_eq!(
            permissions["permissions"]["allow"],
            json!(["Mcp(wizard:*)"])
        );
        assert!(
            permissions["permissions"]["deny"]
                .as_array()
                .expect("deny list")
                .contains(&json!("Shell(*)"))
        );
        let args = fake.read("args");
        let args: Vec<&str> = args.lines().collect();
        let after = |flag: &str| args[args.iter().position(|arg| *arg == flag).expect(flag) + 1];
        assert_eq!(args[0], "-p");
        assert_eq!(after("--output-format"), "stream-json");
        assert_eq!(after("--model"), "composer-2.5");
        assert!(args.contains(&"--approve-mcps"));
        assert_eq!(fake.read("stdin"), "hi");
    }

    #[tokio::test]
    async fn an_exported_api_key_is_not_passed_on() {
        let fake = FakeCursor::signed_in();
        fake.write("events", EVENTS);
        // SAFETY: no other test reads CURSOR_API_KEY, and the fake only looks
        // at the environment it is spawned with.
        unsafe { std::env::set_var("CURSOR_API_KEY", "key") };

        collect(&provider(&fake), request(vec![ChatMessage::user("hi")])).await;

        unsafe { std::env::remove_var("CURSOR_API_KEY") };
        assert!(!fake.file("api-key").exists());
    }

    #[test]
    fn a_longer_history_goes_in_as_a_labelled_transcript() {
        let text = prompt(&[
            ChatMessage::system("You are Wizard."),
            ChatMessage::user("read it"),
            ChatMessage::assistant(r#"{"tool": "read_file", "arguments": {"path": "a"}}"#),
            ChatMessage::tool_result("c1", "read_file", "plinth"),
        ]);

        assert!(text.starts_with(TRANSCRIPT_PREAMBLE), "{text}");
        assert!(text.contains("[user]\nread it\n"), "{text}");
        assert!(
            text.contains("[assistant]\n{\"tool\": \"read_file\""),
            "{text}"
        );
        assert!(
            text.contains("Tool result for `read_file`:\nplinth"),
            "{text}"
        );
        assert!(!text.contains("You are Wizard."), "{text}");
    }

    #[tokio::test]
    async fn an_error_result_fails_the_turn_with_cursors_own_words() {
        let fake = FakeCursor::signed_in();
        fake.write(
            "events",
            r#"{"type":"result","is_error":true,"result":"Usage limit reached"}"#,
        );

        let mut stream = provider(&fake)
            .chat_stream(request(vec![ChatMessage::user("hi")]))
            .await
            .expect("stream starts");
        let err = stream.next().await.expect("an item").expect_err("fails");
        assert!(err.to_string().contains("Usage limit reached"), "{err}");
    }

    #[tokio::test]
    async fn a_child_that_dies_without_a_result_is_an_error() {
        let fake = FakeCursor::signed_in();
        fake.write("events", "");

        let mut stream = provider(&fake)
            .chat_stream(request(vec![ChatMessage::user("hi")]))
            .await
            .expect("stream starts");
        let err = stream.next().await.expect("an item").expect_err("fails");
        assert!(err.to_string().contains("exited before"), "{err}");
    }

    #[tokio::test]
    async fn a_headless_run_is_told_what_to_run_instead_of_being_asked() {
        let missing = FakeCursor::new(false);
        let err = provider(&missing)
            .health()
            .await
            .expect_err("not installed");
        assert_eq!(
            err.to_string(),
            format!("Cursor is not installed. Install it with: {INSTALL_COMMAND}")
        );

        let signed_out = FakeCursor::new(true);
        let err = provider(&signed_out)
            .health()
            .await
            .expect_err("not signed in");
        assert_eq!(
            err.to_string(),
            "Cursor is not signed in. Run: cursor-agent login"
        );

        let signed_in = FakeCursor::signed_in();
        provider(&signed_in).health().await.expect("signed in");
    }

    #[tokio::test]
    async fn the_models_are_the_ids_cursor_lists() {
        let fake = FakeCursor::signed_in();
        assert_eq!(
            provider(&fake).list_models().await.expect("models"),
            ["auto", "composer-2.5"]
        );
    }

    async fn reply(provider: &CursorProvider, messages: &[ChatMessage]) -> (String, ChatChunk) {
        let chunks = collect(provider, with_tool(request(messages.to_vec()))).await;
        let text = chunks
            .iter()
            .filter(|chunk| !chunk.done && !chunk.thinking)
            .filter_map(|chunk| Some(chunk.message.as_ref()?.text()))
            .collect();
        (text, chunks.into_iter().last().expect("a final chunk"))
    }

    #[tokio::test]
    async fn one_child_serves_the_tool_call_and_its_result() {
        if std::process::Command::new("curl")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipped: the fake cursor-agent needs curl to reach the bridge");
            return;
        }
        let fake = FakeCursor::signed_in();
        fake.write("call-events", CALL_EVENTS);
        fake.write("call", CALL);
        fake.write("events", EVENTS);
        let provider = provider(&fake);

        let mut messages = vec![
            ChatMessage::system("You are Wizard."),
            ChatMessage::user("What is in a?"),
        ];
        let (text, last) = reply(&provider, &messages).await;
        assert_eq!(text, "Reading.");
        assert_eq!(last.done_reason.as_deref(), Some("tool_use"));
        let mut turn = last.message.expect("the tool call");
        let calls = turn.take_tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "read_file");
        assert_eq!(calls[0].function.arguments, json!({"path": "a"}));

        messages.push(ChatMessage::assistant_turn(
            "Reading.",
            Vec::new(),
            calls.clone(),
        ));
        messages.push(ChatMessage::tool_result(
            &calls[0].id,
            "read_file",
            "plinth",
        ));
        let (text, last) = reply(&provider, &messages).await;
        assert_eq!(text, "Plinth.");
        assert_eq!(last.done_reason.as_deref(), Some("end_turn"));
        let result: Value = serde_json::from_str(&fake.read("tool-result")).expect("JSON-RPC");
        assert_eq!(result["id"], 7);
        assert_eq!(
            result["result"]["content"],
            json!([{"type": "text", "text": "plinth"}])
        );
        assert_eq!(fake.read("starts").lines().count(), 1);
    }

    #[tokio::test]
    async fn a_history_with_no_live_child_starts_a_new_one_from_the_transcript() {
        let fake = FakeCursor::signed_in();
        fake.write("events", EVENTS);
        let provider = provider(&fake);

        reply(&provider, &[ChatMessage::user("one")]).await;
        reply(
            &provider,
            &[
                ChatMessage::user("one"),
                ChatMessage::assistant("Plinth."),
                ChatMessage::user("two"),
            ],
        )
        .await;

        assert_eq!(fake.read("starts").lines().count(), 2);
        assert!(fake.read("stdin").contains("[assistant]\nPlinth."));
    }

    #[test]
    fn the_stored_entry_selects_this_kind_and_is_not_priced_by_the_token() {
        let config = provider_config();
        assert_eq!(config.kind.as_str(), "cursor");
        assert_eq!(config.usd_per_mtok_in, Some(0.0));
        assert_eq!(config.usd_per_mtok_out, Some(0.0));
    }
}
