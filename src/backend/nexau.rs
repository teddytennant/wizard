//! NexAU bridge client.
//!
//! Spawns `python backend/nexau_bridge.py` once and keeps it alive for the
//! whole session (so the NexAU agent retains multi-turn history). Each user
//! turn writes one `{"type":"prompt",...}` NDJSON line to the child's stdin;
//! the child streams NexAU events back as NDJSON on stdout, which we map onto
//! [`AgentEvent`]s for the TUI.
//!
//! stdout carries *only* the NDJSON protocol (the bridge hardens its fd 1);
//! the child's stderr is redirected to a log file so it can never corrupt the
//! alternate-screen terminal.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;

use crate::agent::{AgentEvent, DoneReason, ToolOutput, emit};

/// Everything needed to launch the bridge subprocess.
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    /// Python interpreter (the project's `.venv/bin/python`).
    pub python: PathBuf,
    /// Path to `backend/nexau_bridge.py`.
    pub script: PathBuf,
    /// Working directory the agent's shell tool operates in (`SANDBOX_WORK_DIR`).
    pub workdir: PathBuf,
    /// `LLM_MODEL` for the NexAU agent.
    pub model: String,
    /// `LLM_BASE_URL`.
    pub base_url: String,
    /// `LLM_API_KEY` (already resolved from its env var or config).
    pub api_key: String,
    /// `LLM_API_TYPE`: `openai` (chat/completions) or `openai_responses`.
    pub api_type: String,
    /// Where the child's stderr (NexAU logging) is appended.
    pub log_path: PathBuf,
}

/// A live bridge subprocess plus its stdio handles.
pub struct Bridge {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
}

impl Bridge {
    /// Launch the bridge and block until it reports `BRIDGE_READY`.
    pub async fn spawn(cfg: &BridgeConfig) -> Result<Self> {
        if let Some(parent) = cfg.log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::create_dir_all(&cfg.workdir)
            .with_context(|| format!("creating workdir {}", cfg.workdir.display()))?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&cfg.log_path)
            .with_context(|| format!("opening bridge log {}", cfg.log_path.display()))?;

        let mut cmd = Command::new(&cfg.python);
        cmd.arg(&cfg.script)
            .env("SANDBOX_WORK_DIR", &cfg.workdir)
            .env("LLM_MODEL", &cfg.model)
            .env("LLM_BASE_URL", &cfg.base_url)
            .env("LLM_API_KEY", &cfg.api_key)
            .env("LLM_API_TYPE", &cfg.api_type)
            .env("PYTHONUNBUFFERED", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(log))
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning bridge: {}", cfg.python.display()))?;
        let stdin = child.stdin.take().context("bridge stdin missing")?;
        let stdout = child.stdout.take().context("bridge stdout missing")?;
        let mut stdout = BufReader::new(stdout).lines();

        // Wait for the readiness handshake.
        loop {
            let line = stdout
                .next_line()
                .await
                .context("reading bridge handshake")?
                .context("bridge closed before it was ready (see bridge log)")?;
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            match msg.get("type").and_then(Value::as_str) {
                Some("BRIDGE_READY") => break,
                Some("BRIDGE_ERROR") => {
                    let m = msg.get("message").and_then(Value::as_str).unwrap_or("?");
                    anyhow::bail!("bridge failed to start: {m}");
                }
                _ => continue,
            }
        }

        Ok(Self {
            child,
            stdin,
            stdout,
        })
    }

    /// Send a prompt and stream the resulting NexAU events onto `events`
    /// until the turn completes. Returns how the turn ended.
    pub async fn run_turn(
        &mut self,
        input: &str,
        events: mpsc::Sender<AgentEvent>,
    ) -> Result<DoneReason> {
        let line = serde_json::to_string(&json!({"type": "prompt", "text": input}))?;
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;

        let mut step: u32 = 0;
        // Per-turn accumulation of streamed tool-call args, keyed by id.
        let mut tools: HashMap<String, ToolAcc> = HashMap::new();

        loop {
            let next = self.stdout.next_line().await;
            let line = match next {
                Ok(Some(line)) => line,
                Ok(None) => {
                    // Child closed stdout: treat as a stopped turn.
                    emit(&events, AgentEvent::Error("bridge stream ended".into())).await;
                    return finish(&events, DoneReason::Stopped).await;
                }
                Err(err) => {
                    emit(
                        &events,
                        AgentEvent::Error(format!("bridge read error: {err}")),
                    )
                    .await;
                    return finish(&events, DoneReason::Stopped).await;
                }
            };
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let kind = msg.get("type").and_then(Value::as_str).unwrap_or("");

            match kind {
                "TEXT_MESSAGE_CONTENT" => {
                    if let Some(d) = msg.get("delta").and_then(Value::as_str) {
                        emit(&events, AgentEvent::TextDelta(d.to_string())).await;
                    }
                }
                "THINKING_TEXT_MESSAGE_CONTENT" => {
                    if let Some(d) = msg.get("delta").and_then(Value::as_str) {
                        emit(&events, AgentEvent::ThinkingDelta(d.to_string())).await;
                    }
                }
                "TOOL_CALL_START" => {
                    if let Some(id) = msg.get("tool_call_id").and_then(Value::as_str) {
                        let name = msg
                            .get("tool_call_name")
                            .and_then(Value::as_str)
                            .unwrap_or("tool")
                            .to_string();
                        tools.insert(id.to_string(), ToolAcc::new(name));
                    }
                }
                "TOOL_CALL_ARGS" => {
                    if let (Some(id), Some(d)) = (
                        msg.get("tool_call_id").and_then(Value::as_str),
                        msg.get("delta").and_then(Value::as_str),
                    ) && let Some(acc) = tools.get_mut(id)
                    {
                        acc.args.push_str(d);
                    }
                }
                "TOOL_CALL_END" => {
                    if let Some(id) = msg.get("tool_call_id").and_then(Value::as_str)
                        && let Some(acc) = tools.get(id)
                    {
                        step += 1;
                        emit(&events, AgentEvent::StepCompleted { step }).await;
                        emit(
                            &events,
                            AgentEvent::ToolStarted {
                                name: acc.name.clone(),
                                args: acc.parsed_args(),
                            },
                        )
                        .await;
                    }
                }
                "TOOL_CALL_RESULT" => {
                    let id = msg
                        .get("tool_call_id")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let name = tools
                        .get(id)
                        .map(|a| a.name.clone())
                        .unwrap_or_else(|| "tool".to_string());
                    let raw = match msg.get("content") {
                        Some(Value::String(s)) => s.clone(),
                        Some(other) => other.to_string(),
                        None => String::new(),
                    };
                    let output = tool_output_from_result(&raw);
                    emit(&events, AgentEvent::ToolFinished { name, output }).await;
                }
                "RUN_ERROR" => {
                    let m = msg
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("agent error");
                    emit(&events, AgentEvent::Error(m.to_string())).await;
                }
                "BRIDGE_ERROR" => {
                    let m = msg.get("message").and_then(Value::as_str).unwrap_or("?");
                    emit(&events, AgentEvent::Error(format!("bridge: {m}"))).await;
                }
                "TURN_COMPLETE" => {
                    let interrupted = msg
                        .get("interrupted")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let errored = msg.get("error").and_then(Value::as_bool).unwrap_or(false);
                    let reason = if interrupted || errored {
                        DoneReason::Stopped
                    } else {
                        DoneReason::Completed
                    };
                    return finish(&events, reason).await;
                }
                // RUN_STARTED / RUN_FINISHED / TEXT_MESSAGE_START / *_END /
                // THINKING_*_START/END are structural; the synthetic
                // TURN_COMPLETE is the authoritative end-of-turn marker.
                _ => {}
            }
        }
    }

    /// Push a new API key to the running agent (OAuth token refresh). The
    /// bridge rebuilds its LLM client with the new key, keeping conversation
    /// history. Sent between turns only.
    pub async fn set_api_key(&mut self, key: &str) -> Result<()> {
        let line = serde_json::to_string(&json!({"type": "set_api_key", "key": key}))?;
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;
        Ok(())
    }

    /// Ask the in-flight turn to cancel (best effort).
    pub async fn interrupt(&mut self) {
        let _ = self.stdin.write_all(b"{\"type\":\"interrupt\"}\n").await;
        let _ = self.stdin.flush().await;
    }

    /// Tell the bridge to exit and reap it.
    pub async fn shutdown(mut self) {
        let _ = self.stdin.write_all(b"{\"type\":\"shutdown\"}\n").await;
        let _ = self.stdin.flush().await;
        let _ = self.child.wait().await;
    }
}

/// Emit the terminal `Done` event and return the reason. Every successful
/// `run_turn` exit goes through here so the UI always unblocks.
async fn finish(events: &mpsc::Sender<AgentEvent>, reason: DoneReason) -> Result<DoneReason> {
    emit(events, AgentEvent::Done { reason }).await;
    Ok(reason)
}

/// Accumulates the streamed JSON-args of one tool call until `TOOL_CALL_END`.
struct ToolAcc {
    name: String,
    args: String,
}

impl ToolAcc {
    fn new(name: String) -> Self {
        Self {
            name,
            args: String::new(),
        }
    }

    /// Parse the accumulated args buffer; fall back to a string value when it
    /// is not valid JSON (so the tool card still shows something).
    fn parsed_args(&self) -> Value {
        let trimmed = self.args.trim();
        if trimmed.is_empty() {
            return json!({});
        }
        serde_json::from_str(trimmed).unwrap_or_else(|_| Value::String(self.args.clone()))
    }
}

/// Turn a NexAU tool result into a clean [`ToolOutput`].
///
/// The shell tool returns a structured JSON envelope — e.g.
/// `{"content":"Output: ...","returnDisplay":"...","exit_code":0,...}` — so we
/// surface the human-facing field and key off `exit_code` for the error flag.
/// Tools that just return a string fall back to a text heuristic.
fn tool_output_from_result(raw: &str) -> ToolOutput {
    if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(raw) {
        let display = map
            .get("returnDisplay")
            .or_else(|| map.get("stdout"))
            .or_else(|| map.get("content"))
            .and_then(Value::as_str)
            .unwrap_or(raw)
            .to_string();
        let is_error = match map.get("exit_code").and_then(Value::as_i64) {
            Some(code) => code != 0,
            None => looks_like_error(&display),
        };
        return ToolOutput {
            content: display,
            is_error,
        };
    }
    ToolOutput {
        content: raw.to_string(),
        is_error: looks_like_error(raw),
    }
}

/// Heuristic for plain-string tool results that carry no exit code.
fn looks_like_error(content: &str) -> bool {
    let lower = content.trim_start().to_ascii_lowercase();
    lower.starts_with("error")
        || lower.starts_with("traceback")
        || lower.contains("command failed")
        || lower.contains("non-zero exit")
}
