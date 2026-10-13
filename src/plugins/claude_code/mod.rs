//! Claude on a Pro or Max subscription, as a plugin: `kind = "claudecode"`
//! behind `--features provider-claude-code`.
//!
//! Anthropic allows subscription sign-in only through Claude Code itself, so
//! this provider never holds a credential and never opens a connection. Each
//! model call spawns the user's own `claude -p` and reads its `stream-json`
//! events; signing in is `claude auth login`, and whether that happened is
//! whatever `claude auth status` says. Nothing here reads Claude Code's
//! files.
//!
//! Wizard keeps its own prompt and tools. The child runs with `--tools ""`
//! and Wizard's system prompt. A call that offers tools gets them as native
//! tool calls: the child is given Wizard's tool specs through a loopback MCP
//! server and kept alive for the conversation, and the agent loop still runs
//! every tool itself. See [`session`] and [`bridge`].
//!
//! `claude -p` answers every user line on stdin with a turn of its own and
//! takes no assistant lines, so a history that is not already inside a live
//! child cannot be replayed as turns. It goes in as one user message
//! instead: see [`encode`].

mod bridge;
mod session;
pub mod setup;

use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use futures_util::stream;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdout};
use tokio::task::JoinHandle;

use crate::config::ProviderConfig;
use crate::kernel::{Capability, Ctx, Plugin, PluginManifest};
use crate::llm::provider::LlmProvider;
use crate::llm::registry::{Credentials, ProviderDescriptor, ProviderKind};
use crate::llm::{
    CacheTokens, ChatChunk, ChatMessage, ChatRequest, ChatStream, ContentBlock, Role,
};

/// Anthropic's installer, as https://code.claude.com/docs/en/setup gives it.
#[cfg(not(windows))]
pub const INSTALL_COMMAND: &str = "curl -fsSL https://claude.ai/install.sh | bash";
#[cfg(windows)]
pub const INSTALL_COMMAND: &str = "irm https://claude.ai/install.ps1 | iex";

pub const LOGIN_COMMAND: &str = "claude auth login";

/// Claude Code's own model aliases, each following the newest model of its
/// family. A full model id works too.
const MODELS: &[&str] = &["sonnet", "opus", "haiku"];

const TRANSCRIPT_PREAMBLE: &str = "This is the conversation so far, oldest turn first. Each turn \
     starts with a line naming who spoke, like [user] or [assistant]. Write the next assistant \
     turn only, without the label.";

/// The entry `/provider` writes. The zero rates are what keep `/cost` from
/// pricing a subscription by the token.
pub fn provider_config() -> ProviderConfig {
    ProviderConfig {
        name: "claude-code".to_string(),
        kind: ProviderKind::CLAUDE_CODE,
        model: MODELS[0].to_string(),
        usd_per_mtok_in: Some(0.0),
        usd_per_mtok_out: Some(0.0),
        ..ProviderConfig::default()
    }
}

/// An installed `claude`.
#[derive(Debug, Clone)]
pub struct Cli {
    program: PathBuf,
}

impl Cli {
    pub fn find() -> Option<Self> {
        Self::find_in(std::env::var_os("PATH").as_deref(), dirs::home_dir())
    }

    /// `claude` in one of `path`'s directories, or in `~/.local/bin`, which is
    /// where the installer puts it and which a running shell may not have on
    /// its `PATH` yet.
    fn find_in(path: Option<&OsStr>, home: Option<PathBuf>) -> Option<Self> {
        let name = if cfg!(windows) {
            "claude.exe"
        } else {
            "claude"
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

    pub fn version(&self) -> Result<String> {
        let output = self
            .command()
            .arg("--version")
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("running {}", self.program.display()))?;
        if !output.status.success() {
            bail!("`claude --version` failed: {}", output.status);
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    pub fn signed_in(&self) -> Result<bool> {
        let output = self
            .command()
            .args(["auth", "status", "--json"])
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("running {}", self.program.display()))?;
        let status: Value = serde_json::from_slice(&output.stdout)
            .context("`claude auth status --json` did not print JSON")?;
        Ok(status["loggedIn"].as_bool().unwrap_or(false))
    }

    /// Claude Code's own sign-in, on the terminal Wizard was given.
    pub fn login(&self) -> Result<()> {
        self.command()
            .args(["auth", "login"])
            .status()
            .with_context(|| format!("running {}", self.program.display()))?;
        Ok(())
    }
}

/// What a run that cannot ask is told: one line naming the command to run.
fn ready(cli: Option<&Cli>) -> Result<()> {
    let cli = cli.ok_or_else(not_installed)?;
    if !cli.signed_in()? {
        bail!("Claude Code is not signed in. Run: {LOGIN_COMMAND}");
    }
    Ok(())
}

fn not_installed() -> anyhow::Error {
    anyhow!("Claude Code is not installed. Install it with: {INSTALL_COMMAND}")
}

struct ClaudeCodeProvider {
    cli: Option<Cli>,
    model: String,
}

#[async_trait]
impl LlmProvider for ClaudeCodeProvider {
    async fn health(&self) -> Result<()> {
        let cli = self.cli.clone();
        tokio::task::spawn_blocking(move || ready(cli.as_ref()))
            .await
            .context("the Claude Code check panicked")?
    }

    async fn supports_native_tools(&self, _model: &str) -> Result<bool> {
        Ok(true)
    }

    /// Every model Claude Code offers has at least this much, and without a
    /// window the loop falls back to compacting on a few dozen kilobytes.
    async fn context_window(&self, _model: &str) -> Option<u32> {
        Some(200_000)
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        Ok(MODELS.iter().map(|model| model.to_string()).collect())
    }

    async fn chat_stream(&self, request: ChatRequest) -> Result<ChatStream> {
        let cli = self.cli.as_ref().ok_or_else(not_installed)?;
        if !request.tools.is_empty() {
            return session::chat(cli, request).await;
        }
        let (system, input) = encode(&request.messages);

        let mut system_file =
            tempfile::NamedTempFile::new().context("writing the system prompt for Claude Code")?;
        std::io::Write::write_all(&mut system_file, system.as_bytes())
            .context("writing the system prompt for Claude Code")?;

        let mut command = tokio::process::Command::new(&cli.program);
        command
            .args(["-p", "--input-format", "stream-json"])
            .args(["--output-format", "stream-json", "--verbose"])
            .arg("--include-partial-messages")
            .args(["--tools", "", "--strict-mcp-config", "--safe-mode"])
            .args(["--disable-slash-commands", "--no-session-persistence"])
            .args(["--model", &request.model])
            .arg("--system-prompt-file")
            .arg(system_file.path());
        if let Some(effort) = request.options.and_then(|options| options.reasoning_effort) {
            command.args(["--effort", &effort]);
        }
        let mut child = without_api_keys(&mut command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting {}", cli.program.display()))?;

        let mut stdin = child.stdin.take().context("claude has no stdin")?;
        tokio::spawn(async move {
            let _ = stdin.write_all(format!("{input}\n").as_bytes()).await;
        });
        let mut stderr = child.stderr.take().context("claude has no stderr")?;
        let stderr = tokio::spawn(async move {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text).await;
            text
        });
        let stdout = child.stdout.take().context("claude has no stdout")?;

        let turn = Turn {
            lines: BufReader::new(stdout).lines(),
            child,
            stderr,
            _system_file: system_file,
            done: false,
        };
        Ok(Box::pin(stream::try_unfold(turn, |mut turn| async move {
            Ok(turn.next().await?.map(|chunk| (chunk, turn)))
        })))
    }

    fn label(&self) -> String {
        format!("claude-code:{}", self.model)
    }
}

/// An exported API key would outrank the subscription sign-in inside Claude
/// Code and bill the turn to it instead.
fn without_api_keys(command: &mut tokio::process::Command) -> &mut tokio::process::Command {
    command
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
}

/// One `claude -p` run, alive until its stream is dropped.
struct Turn {
    lines: Lines<BufReader<ChildStdout>>,
    child: Child,
    stderr: JoinHandle<String>,
    _system_file: tempfile::NamedTempFile,
    done: bool,
}

impl Turn {
    async fn next(&mut self) -> Result<Option<ChatChunk>> {
        if self.done {
            return Ok(None);
        }
        while let Some(line) = self.lines.next_line().await? {
            let Ok(event) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let Some(chunk) = decode(&event)? {
                self.done = chunk.done;
                return Ok(Some(chunk));
            }
        }
        let status = self.child.wait().await?;
        let stderr = (&mut self.stderr).await.unwrap_or_default();
        bail!(
            "claude exited before finishing the reply ({status}): {}",
            stderr.trim()
        );
    }
}

/// Wizard's history as `claude -p` input: the leading system messages as the
/// system prompt, and everything after them as one `stream-json` user line.
///
/// A lone user message goes in as it is. A longer history becomes a labelled
/// transcript, one text block per message, so a later call repeats the earlier
/// blocks byte for byte and the prompt cache still matches them.
fn encode(messages: &[ChatMessage]) -> (String, Value) {
    let (system, rest) = split_system(messages);
    (system, user_line(rest, rest.len() > 1))
}

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

/// `messages` as one `stream-json` user line, labelled by speaker when it is
/// a `transcript` of several turns.
fn user_line(messages: &[ChatMessage], transcript: bool) -> Value {
    let mut content = Vec::new();
    if transcript {
        content.push(json!({"type": "text", "text": TRANSCRIPT_PREAMBLE}));
    }
    for message in messages {
        let mut text = String::new();
        if transcript {
            let who = match message.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
            };
            text = format!("[{who}]\n");
        }
        for block in &message.content {
            match block {
                ContentBlock::Text(block) => text.push_str(&block.text),
                ContentBlock::ToolUse(call) => text.push_str(
                    &json!({"tool": call.function.name, "arguments": call.function.arguments})
                        .to_string(),
                ),
                ContentBlock::ToolResult(result) => text.push_str(&format!(
                    "Tool result for `{}`:\n{}",
                    result.name, result.content
                )),
                ContentBlock::Image(image) => {
                    push_text(&mut content, std::mem::take(&mut text));
                    content.push(json!({
                        "type": "image",
                        "source": {"type": "base64", "media_type": image.mime, "data": image.b64},
                    }));
                }
                ContentBlock::Thinking(_) => {}
            }
        }
        push_text(&mut content, text);
    }
    json!({"type": "user", "message": {"role": "user", "content": content}})
}

fn push_text(content: &mut Vec<Value>, text: String) {
    if !text.is_empty() {
        content.push(json!({"type": "text", "text": text}));
    }
}

/// One `stream-json` event as a chunk. Text and thinking deltas stream live;
/// the `result` event closes the turn and carries the usage Claude Code
/// counted. Everything else on the stream is Claude Code's own bookkeeping.
fn decode(event: &Value) -> Result<Option<ChatChunk>> {
    match event["type"].as_str() {
        Some("stream_event") => Ok(delta_chunk(&event["event"])),
        Some("result") => {
            if event["is_error"].as_bool().unwrap_or(false) {
                bail!(
                    "Claude Code: {}",
                    event["result"].as_str().unwrap_or("the turn failed")
                );
            }
            Ok(Some(final_chunk(
                &event["usage"],
                event["stop_reason"].as_str(),
                None,
            )))
        }
        _ => Ok(None),
    }
}

/// The text or thinking an API stream event adds, if it adds any.
fn delta_chunk(event: &Value) -> Option<ChatChunk> {
    let delta = &event["delta"];
    let (text, thinking) = match delta["type"].as_str()? {
        "text_delta" => (delta["text"].as_str()?, false),
        "thinking_delta" => (delta["thinking"].as_str()?, true),
        _ => return None,
    };
    Some(ChatChunk {
        message: Some(ChatMessage::assistant(text)),
        images: Vec::new(),
        thinking,
        done: false,
        done_reason: None,
        eval_count: None,
        prompt_eval_count: None,
        cache: CacheTokens::NONE,
        reasoning_eval_count: None,
    })
}

/// The chunk that closes a reply, carrying the `usage` Claude Code counted.
fn final_chunk(usage: &Value, stop: Option<&str>, message: Option<ChatMessage>) -> ChatChunk {
    let count = |field: &str| usage[field].as_u64().unwrap_or(0);
    let cache = CacheTokens {
        read: count("cache_read_input_tokens"),
        write: count("cache_creation_input_tokens"),
    };
    ChatChunk {
        message,
        images: Vec::new(),
        thinking: false,
        done: true,
        done_reason: stop.map(str::to_string),
        eval_count: Some(count("output_tokens")),
        prompt_eval_count: Some(count("input_tokens") + cache.read + cache.write),
        cache,
        reasoning_eval_count: usage["output_tokens_details"]["thinking_tokens"].as_u64(),
    }
}

pub fn descriptor() -> ProviderDescriptor {
    ProviderDescriptor::new(
        ProviderKind::CLAUDE_CODE,
        "Claude (subscription)",
        Credentials::Account {
            login: "claude".to_string(),
        },
        |config| {
            Ok(Arc::new(ClaudeCodeProvider {
                cli: Cli::find(),
                model: config.model.clone(),
            }))
        },
    )
}

pub struct ClaudeCodePlugin {
    manifest: PluginManifest,
}

impl ClaudeCodePlugin {
    pub fn new() -> Self {
        Self {
            manifest: PluginManifest {
                name: "claude-code".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                description: "Claude on a subscription, through Claude Code".to_string(),
                capabilities: vec![Capability::Process],
                optional_deps: Vec::new(),
                profiles: vec!["server".to_string(), "default".to_string()],
            },
        }
    }
}

impl Default for ClaudeCodePlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for ClaudeCodePlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn apply(&self, ctx: &mut Ctx) -> anyhow::Result<()> {
        ctx.provider(descriptor())?;
        Ok(())
    }
}

/// A directory to search for `claude`, for tests: `tests/fixtures/fake-claude`
/// symlinked in as `claude`, answering from the files beside the link, so no
/// test needs a real sign-in.
#[cfg(all(test, unix))]
pub(crate) mod fake {
    use std::ffi::OsString;
    use std::path::PathBuf;

    const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake-claude");

    pub(crate) struct FakeClaude {
        dir: tempfile::TempDir,
    }

    impl FakeClaude {
        pub(crate) fn new(installed: bool) -> Self {
            let fake = Self {
                dir: tempfile::tempdir().expect("temp dir"),
            };
            if installed {
                std::os::unix::fs::symlink(SCRIPT, fake.file("claude")).expect("link the fake");
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

        /// A shell command that installs the fake, standing in for the
        /// installer.
        pub(crate) fn installer(&self) -> String {
            format!("ln -s '{SCRIPT}' '{}'", self.file("claude").display())
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use futures_util::StreamExt;

    use super::fake::FakeClaude;
    use super::*;
    use crate::llm::{ChatOptions, Image};

    /// Recorded from `claude -p --output-format stream-json --verbose
    /// --include-partial-messages` 2.1.296, trimmed to the fields read here
    /// plus one of each event that is skipped.
    const EVENTS: &str = r#"{"type":"system","subtype":"init","tools":[],"model":"claude-haiku-5-5"}
{"type":"stream_event","event":{"type":"message_start","message":{"usage":{"input_tokens":2}}}}
{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Recall it."}}}
{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Pli"}}}
{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"nth."}}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Plinth."}]}}
{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}
{"duration_api_ms":2178,"stop_reason":"end_turn","total_cost_usd":0.0003028,"usage":{"input_tokens":2,"cache_creation_input_tokens":717,"cache_read_input_tokens":40,"output_tokens":7,"output_tokens_details":{"thinking_tokens":3}},"is_error":false,"type":"result","subtype":"success","result":"Plinth."}
"#;

    fn provider(fake: &FakeClaude) -> ClaudeCodeProvider {
        ClaudeCodeProvider {
            cli: Cli::find_in(Some(&fake.path()), None),
            model: "sonnet".to_string(),
        }
    }

    fn request(messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest {
            model: "haiku".to_string(),
            messages,
            tools: Vec::new(),
            stream: true,
            options: Some(ChatOptions {
                reasoning_effort: Some("high".to_string()),
                ..ChatOptions::default()
            }),
        }
    }

    async fn collect(provider: &ClaudeCodeProvider, request: ChatRequest) -> Vec<ChatChunk> {
        let stream = provider.chat_stream(request).await.expect("stream starts");
        stream
            .map(|chunk| chunk.expect("chunk"))
            .collect::<Vec<_>>()
            .await
    }

    #[tokio::test]
    async fn stream_json_events_become_chunks_with_claude_codes_usage() {
        let fake = FakeClaude::signed_in();
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
        assert_eq!(last.prompt_eval_count, Some(2 + 717 + 40));
        assert_eq!(
            last.cache,
            CacheTokens {
                read: 40,
                write: 717
            }
        );
        assert_eq!(last.eval_count, Some(7));
        assert_eq!(last.reasoning_eval_count, Some(3));
    }

    #[tokio::test]
    async fn the_child_gets_wizards_prompt_the_model_and_no_tools_of_its_own() {
        let fake = FakeClaude::signed_in();
        fake.write("events", EVENTS);

        collect(
            &provider(&fake),
            request(vec![
                ChatMessage::system("You are Wizard."),
                ChatMessage::user("hi"),
            ]),
        )
        .await;

        assert_eq!(fake.read("system"), "You are Wizard.");
        let args = fake.read("args");
        let args: Vec<&str> = args.lines().collect();
        let after = |flag: &str| args[args.iter().position(|arg| *arg == flag).expect(flag) + 1];
        assert_eq!(args[0], "-p");
        assert_eq!(after("--input-format"), "stream-json");
        assert_eq!(after("--output-format"), "stream-json");
        assert_eq!(after("--tools"), "");
        assert_eq!(after("--model"), "haiku");
        assert_eq!(after("--effort"), "high");
        assert!(args.contains(&"--strict-mcp-config"));

        let sent: Value = serde_json::from_str(fake.read("stdin").trim()).expect("one JSON line");
        assert_eq!(sent["type"], "user");
        assert_eq!(
            sent["message"]["content"],
            json!([{"type": "text", "text": "hi"}])
        );
    }

    #[test]
    fn a_longer_history_goes_in_as_a_labelled_transcript() {
        let (system, line) = encode(&[
            ChatMessage::system("You are Wizard."),
            ChatMessage::user("read it"),
            ChatMessage::assistant(r#"{"tool": "read_file", "arguments": {"path": "a"}}"#),
            ChatMessage::user_with_images(
                "Tool result for `read_file`:\nshot",
                vec![Image::new("AAAA", "image/png")],
            ),
        ]);

        assert_eq!(system, "You are Wizard.");
        let content = line["message"]["content"].as_array().expect("blocks");
        let texts: Vec<&str> = content
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect();
        assert_eq!(
            texts,
            [
                TRANSCRIPT_PREAMBLE,
                "[user]\nread it",
                "[assistant]\n{\"tool\": \"read_file\", \"arguments\": {\"path\": \"a\"}}",
                "[user]\nTool result for `read_file`:\nshot",
            ]
        );
        assert_eq!(
            content.last().expect("the image")["source"],
            json!({"type": "base64", "media_type": "image/png", "data": "AAAA"})
        );
    }

    #[tokio::test]
    async fn an_error_result_fails_the_turn_with_claude_codes_own_words() {
        let fake = FakeClaude::signed_in();
        fake.write(
            "events",
            r#"{"type":"result","is_error":true,"result":"Not logged in · Please run /login"}"#,
        );

        let mut stream = provider(&fake)
            .chat_stream(request(vec![ChatMessage::user("hi")]))
            .await
            .expect("stream starts");
        let err = stream.next().await.expect("an item").expect_err("fails");
        assert!(err.to_string().contains("Not logged in"), "{err}");
    }

    #[tokio::test]
    async fn a_child_that_dies_without_a_result_is_an_error() {
        let fake = FakeClaude::signed_in();
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
        let missing = FakeClaude::new(false);
        let err = provider(&missing)
            .health()
            .await
            .expect_err("not installed");
        assert_eq!(
            err.to_string(),
            format!("Claude Code is not installed. Install it with: {INSTALL_COMMAND}")
        );

        let signed_out = FakeClaude::new(true);
        let err = provider(&signed_out)
            .health()
            .await
            .expect_err("not signed in");
        assert_eq!(
            err.to_string(),
            "Claude Code is not signed in. Run: claude auth login"
        );

        let signed_in = FakeClaude::signed_in();
        provider(&signed_in).health().await.expect("signed in");
    }

    #[tokio::test]
    async fn the_loop_is_told_the_window_so_it_does_not_compact_on_bytes() {
        let fake = FakeClaude::signed_in();
        assert_eq!(
            provider(&fake).context_window("sonnet").await,
            Some(200_000)
        );
    }

    #[tokio::test]
    async fn tools_reach_the_model_as_native_calls() {
        let fake = FakeClaude::signed_in();
        assert!(
            provider(&fake)
                .supports_native_tools("sonnet")
                .await
                .unwrap()
        );
    }

    #[test]
    fn the_stored_entry_selects_this_kind_and_is_not_priced_by_the_token() {
        let config = provider_config();
        assert_eq!(config.kind.as_str(), "claudecode");
        assert_eq!(config.usd_per_mtok_in, Some(0.0));
        assert_eq!(config.usd_per_mtok_out, Some(0.0));
    }
}
