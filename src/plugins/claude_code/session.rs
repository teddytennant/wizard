//! A `claude` child kept alive across the calls of one conversation.
//!
//! Starting `claude -p` costs about a second, and a child that is handed the
//! whole history again on every call cannot give the model real turns or keep
//! its prompt cache. So a call with tools opens a session: the child gets
//! Wizard's tools through the [`Bridge`] and stays up. The next call for the
//! same conversation only adds to it, either the results of the tool calls
//! the child is waiting on or the user's next message.
//!
//! A call that does not continue a live session (the first one, or the one
//! after a compaction rewrote the history) starts a new child from the
//! transcript, exactly as a call without tools does.

use std::hash::{Hash, Hasher};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures_util::stream;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::task::JoinHandle;

use super::bridge::{Bridge, SERVER, TOOL_PREFIX};
use super::{Cli, delta_chunk, encode, final_chunk, push_text, split_system, user_line};
use crate::llm::{
    ChatChunk, ChatMessage, ChatRequest, ChatStream, ContentBlock, FunctionCall, Role, ToolCall,
};

/// Idle children kept for the conversations most likely to come back: the
/// main loop and a few subagents.
const KEPT: usize = 4;
const IDLE_LIMIT: Duration = Duration::from_secs(15 * 60);

static IDLE: Mutex<Vec<Session>> = Mutex::new(Vec::new());

pub struct Session {
    /// The child's fixed half: binary, model, effort, system prompt, tools.
    key: u64,
    bridge: Bridge,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    child: Child,
    stderr: JoinHandle<String>,
    _files: [tempfile::NamedTempFile; 2],
    /// How many messages the request that produced the last reply held.
    seen: usize,
    /// Tool calls the child is waiting on. Empty once its turn has ended.
    pending: Vec<String>,
    calls: Vec<ToolCall>,
    /// The text of the reply in progress, then of the last one given.
    said: String,
    usage: Value,
    used: Instant,
}

/// What a request adds to a live session.
enum Next {
    Results(Vec<(String, Vec<Value>)>),
    User(Value),
}

pub async fn chat(cli: &Cli, request: ChatRequest) -> Result<ChatStream> {
    let (system, _) = split_system(&request.messages);
    let key = key(cli, &request, &system);

    let mut session = match take(key, &request.messages) {
        Some((mut session, next)) => {
            match next {
                Next::Results(results) => {
                    for (id, content) in results {
                        session.bridge.answer(&id, content);
                    }
                }
                Next::User(line) => session.send(&line).await?,
            }
            session
        }
        None => {
            tracing::debug!(messages = request.messages.len(), "starting a claude child");
            let mut session = Session::open(cli, &request, &system, key).await?;
            session.send(&encode(&request.messages).1).await?;
            session
        }
    };
    session.seen = request.messages.len();
    session.pending.clear();
    session.said.clear();

    Ok(Box::pin(stream::try_unfold(
        Some(session),
        |session| async move {
            let Some(mut session) = session else {
                return Ok(None);
            };
            let chunk = session.next().await?;
            if chunk.done {
                keep(session);
                return Ok(Some((chunk, None)));
            }
            Ok(Some((chunk, Some(session))))
        },
    )))
}

fn key(cli: &Cli, request: &ChatRequest, system: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    cli.program.hash(&mut hasher);
    request.model.hash(&mut hasher);
    effort(request).hash(&mut hasher);
    system.hash(&mut hasher);
    serde_json::to_string(&request.tools)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

fn effort(request: &ChatRequest) -> Option<&str> {
    request.options.as_ref()?.reasoning_effort.as_deref()
}

fn take(key: u64, messages: &[ChatMessage]) -> Option<(Session, Next)> {
    let mut idle = IDLE.lock().expect("session lock");
    idle.retain(|session| session.used.elapsed() < IDLE_LIMIT);
    let (index, next) = idle.iter().enumerate().find_map(|(index, session)| {
        (session.key == key)
            .then(|| session.continues(messages))
            .flatten()
            .map(|next| (index, next))
    })?;
    Some((idle.remove(index), next))
}

fn keep(mut session: Session) {
    session.used = Instant::now();
    let mut idle = IDLE.lock().expect("session lock");
    if idle.len() == KEPT {
        idle.remove(0);
    }
    idle.push(session);
}

impl Session {
    async fn open(cli: &Cli, request: &ChatRequest, system: &str, key: u64) -> Result<Self> {
        let bridge = Bridge::start(&request.tools).await?;
        let write = |text: &str| -> Result<tempfile::NamedTempFile> {
            let mut file = tempfile::NamedTempFile::new()?;
            std::io::Write::write_all(&mut file, text.as_bytes())?;
            Ok(file)
        };
        let system_file = write(system).context("writing the system prompt for Claude Code")?;
        let config_file = write(&bridge.config().to_string())
            .context("writing the tool bridge config for Claude Code")?;

        let mut command = tokio::process::Command::new(&cli.program);
        command
            .args(["-p", "--input-format", "stream-json"])
            .args(["--output-format", "stream-json", "--verbose"])
            .arg("--include-partial-messages")
            .args(["--tools", "", "--strict-mcp-config", "--mcp-config"])
            .arg(config_file.path())
            .args(["--allowedTools", &format!("mcp__{SERVER}")])
            .args(["--setting-sources", "", "--disable-slash-commands"])
            .arg("--no-session-persistence")
            .args(["--model", &request.model])
            .arg("--system-prompt-file")
            .arg(system_file.path());
        if let Some(effort) = effort(request) {
            command.args(["--effort", effort]);
        }
        // Wizard already bounds how long a tool runs and how much it returns.
        let mut child = super::without_api_keys(&mut command)
            .env("MCP_TOOL_TIMEOUT", "86400000")
            .env("MAX_MCP_OUTPUT_TOKENS", "1000000")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting {}", cli.program.display()))?;

        let stdin = child.stdin.take().context("claude has no stdin")?;
        let stdout = child.stdout.take().context("claude has no stdout")?;
        let mut stderr = child.stderr.take().context("claude has no stderr")?;
        let stderr = tokio::spawn(async move {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text).await;
            text
        });
        Ok(Self {
            key,
            bridge,
            stdin,
            lines: BufReader::new(stdout).lines(),
            child,
            stderr,
            _files: [system_file, config_file],
            seen: 0,
            pending: Vec::new(),
            calls: Vec::new(),
            said: String::new(),
            usage: Value::Null,
            used: Instant::now(),
        })
    }

    async fn send(&mut self, line: &Value) -> Result<()> {
        self.stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .context("writing to claude")?;
        self.stdin.flush().await.context("writing to claude")
    }

    /// What `messages` adds, if it is this session's conversation one step on:
    /// the reply this session gave, then either the results of every call it
    /// is waiting on or what the user said next.
    fn continues(&self, messages: &[ChatMessage]) -> Option<Next> {
        let (reply, added) = messages.get(self.seen..)?.split_first()?;
        let said: String = reply
            .content
            .iter()
            .filter_map(ContentBlock::as_text)
            .collect();
        if reply.role != Role::Assistant
            || said.trim() != self.said.trim()
            || added.is_empty()
            || added.iter().any(|message| message.role == Role::Assistant)
        {
            return None;
        }
        if self.pending.is_empty() {
            if added.iter().any(|message| message.role == Role::Tool) {
                return None;
            }
            return Some(Next::User(user_line(added, false)));
        }

        let mut results: Vec<(String, Vec<Value>)> = Vec::new();
        let mut loose = Vec::new();
        for block in added.iter().flat_map(|message| &message.content) {
            match block {
                ContentBlock::ToolResult(result) if self.pending.contains(&result.tool_use_id) => {
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
        if !self.pending.iter().all(answered) {
            return None;
        }
        // Anything else the loop added rides along with the last result,
        // which is where the model reads it from.
        results.last_mut()?.1.extend(loose);
        Some(Next::Results(results))
    }

    async fn next(&mut self) -> Result<ChatChunk> {
        while let Some(line) = self.lines.next_line().await? {
            let Ok(event) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            match event["type"].as_str() {
                Some("stream_event") => {
                    let inner = &event["event"];
                    if let Some(chunk) = delta_chunk(inner) {
                        if let (false, Some(message)) = (chunk.thinking, &chunk.message) {
                            self.said.push_str(&message.text());
                        }
                        return Ok(chunk);
                    }
                    if inner["type"] == "message_delta" {
                        self.usage = inner["usage"].clone();
                        if inner["delta"]["stop_reason"] == "tool_use" && !self.calls.is_empty() {
                            let calls = std::mem::take(&mut self.calls);
                            self.pending = calls.iter().map(|call| call.id.clone()).collect();
                            let message = ChatMessage::assistant_turn("", Vec::new(), calls);
                            return Ok(final_chunk(&self.usage, Some("tool_use"), Some(message)));
                        }
                    }
                }
                Some("assistant") => {
                    let blocks = event["message"]["content"].as_array();
                    for block in blocks.into_iter().flatten() {
                        self.note_call(block);
                    }
                }
                Some("result") => {
                    if event["is_error"].as_bool().unwrap_or(false) {
                        bail!(
                            "Claude Code: {}",
                            event["result"].as_str().unwrap_or("the turn failed")
                        );
                    }
                    let usage = if self.usage.is_null() {
                        &event["usage"]
                    } else {
                        &self.usage
                    };
                    return Ok(final_chunk(usage, event["stop_reason"].as_str(), None));
                }
                _ => {}
            }
        }
        let status = self.child.wait().await?;
        let stderr = (&mut self.stderr).await.unwrap_or_default();
        bail!(
            "claude exited before finishing the reply ({status}): {}",
            stderr.trim()
        );
    }

    fn note_call(&mut self, block: &Value) {
        let (Some(id), Some(name)) = (block["id"].as_str(), block["name"].as_str()) else {
            return;
        };
        let Some(name) = name.strip_prefix(TOOL_PREFIX) else {
            return;
        };
        if block["type"] != "tool_use" || self.calls.iter().any(|call| call.id == id) {
            return;
        }
        self.calls.push(ToolCall {
            id: id.to_string(),
            function: FunctionCall {
                name: name.to_string(),
                arguments: block["input"].clone(),
            },
        });
    }
}

#[cfg(all(test, unix))]
mod tests {
    use futures_util::StreamExt;

    use super::super::fake::FakeClaude;
    use super::*;
    use crate::llm::ToolSpec;

    const CALL_EVENTS: &str = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Reading."}}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"mcp__wizard__read_file","input":{"path":"a"}}]}}
{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"input_tokens":2,"cache_creation_input_tokens":100,"cache_read_input_tokens":0,"output_tokens":9}}}
"#;
    const CALL: &str = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"read_file","arguments":{"path":"a"},"_meta":{"claudecode/toolUseId":"toolu_1"}}}"#;
    const EVENTS: &str = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Plinth."}}}
{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":3,"cache_creation_input_tokens":20,"cache_read_input_tokens":100,"output_tokens":4}}}
{"type":"result","is_error":false,"stop_reason":"end_turn","usage":{"input_tokens":5,"cache_creation_input_tokens":120,"cache_read_input_tokens":100,"output_tokens":13}}
"#;

    fn request(messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest {
            model: "haiku".to_string(),
            messages,
            tools: vec![ToolSpec::function(
                "read_file",
                "Read a file.",
                json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            )],
            stream: true,
            options: None,
        }
    }

    async fn reply(cli: &Cli, messages: &[ChatMessage]) -> (String, ChatChunk) {
        let stream = chat(cli, request(messages.to_vec())).await.expect("starts");
        let chunks: Vec<ChatChunk> = stream.map(|chunk| chunk.expect("chunk")).collect().await;
        let text = chunks
            .iter()
            .filter(|chunk| !chunk.done)
            .filter_map(|chunk| Some(chunk.message.as_ref()?.text()))
            .collect();
        (text, chunks.into_iter().last().expect("a final chunk"))
    }

    #[tokio::test]
    async fn one_child_serves_the_tool_call_its_result_and_the_next_turn() {
        if std::process::Command::new("curl")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipped: the fake claude needs curl to reach the bridge");
            return;
        }
        let fake = FakeClaude::signed_in();
        fake.write("call-events", CALL_EVENTS);
        fake.write("call", CALL);
        fake.write("events", EVENTS);
        let cli = Cli::find_in(Some(&fake.path()), None).expect("the fake");

        let mut messages = vec![
            ChatMessage::system("You are Wizard."),
            ChatMessage::user("What is in a?"),
        ];
        let (text, last) = reply(&cli, &messages).await;
        assert_eq!(text, "Reading.");
        assert_eq!(last.done_reason.as_deref(), Some("tool_use"));
        assert_eq!(last.prompt_eval_count, Some(102));
        let mut turn = last.message.expect("the tool call");
        let calls = turn.take_tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "toolu_1");
        assert_eq!(calls[0].function.name, "read_file");
        assert_eq!(calls[0].function.arguments, json!({"path": "a"}));

        messages.push(ChatMessage::assistant_turn("Reading.", Vec::new(), calls));
        messages.push(ChatMessage::tool_result("toolu_1", "read_file", "plinth"));
        let (text, last) = reply(&cli, &messages).await;
        assert_eq!(text, "Plinth.");
        assert_eq!(last.done_reason.as_deref(), Some("end_turn"));
        assert_eq!(last.prompt_eval_count, Some(123));
        let result: Value = serde_json::from_str(&fake.read("tool-result")).expect("JSON-RPC");
        assert_eq!(result["id"], 7);
        assert_eq!(
            result["result"]["content"],
            json!([{"type": "text", "text": "plinth"}])
        );

        messages.push(ChatMessage::assistant("Plinth."));
        messages.push(ChatMessage::user("Again?"));
        let (text, _) = reply(&cli, &messages).await;
        assert_eq!(text, "Plinth.");

        assert_eq!(fake.read("starts").lines().count(), 1);
        let sent = fake.read("stdin");
        let sent: Vec<Value> = sent
            .lines()
            .map(|line| serde_json::from_str(line).expect("a JSON line"))
            .collect();
        assert_eq!(sent.len(), 2);
        assert_eq!(
            sent[1]["message"]["content"],
            json!([{"type": "text", "text": "Again?"}])
        );
        let args = fake.read("args");
        assert!(args.lines().any(|arg| arg == "--mcp-config"), "{args}");
        assert!(args.lines().any(|arg| arg == "mcp__wizard"), "{args}");
    }

    #[tokio::test]
    async fn a_rewritten_history_starts_a_new_child_from_the_transcript() {
        let fake = FakeClaude::signed_in();
        fake.write("call-events", "");
        fake.write("call", "{}");
        fake.write("events", EVENTS);
        let cli = Cli::find_in(Some(&fake.path()), None).expect("the fake");

        let first = [
            ChatMessage::system("You are Wizard."),
            ChatMessage::user("one"),
        ];
        reply(&cli, &first).await;
        let rewritten = [
            ChatMessage::system("You are Wizard."),
            ChatMessage::user("a summary of what came before"),
            ChatMessage::assistant("noted"),
            ChatMessage::user("two"),
        ];
        reply(&cli, &rewritten).await;

        assert_eq!(fake.read("starts").lines().count(), 2);
        let sent = fake.read("stdin");
        assert!(sent.lines().last().expect("a line").contains("[assistant]"));
    }
}
