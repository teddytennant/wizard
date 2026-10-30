//! `SubagentHandback`, `WaitForMcpServers`, `EndConversation`, `SendFeedback`
//! and `PowerShell`: the small session-level tools.
//!
//! None of them has much to do in Wizard, and each says exactly what it did,
//! so a prompt written for Claude Code gets a true answer instead of a missing
//! tool.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{invalid, str_arg};
use crate::tools::registry::ToolRegistry;
use crate::tools::{Tool, ToolAccess, ToolContext, ToolError, ToolKind, ToolOutput};

pub fn tools(registry: &ToolRegistry) -> Vec<Arc<dyn Tool>> {
    let connected: BTreeSet<String> = registry
        .specs()
        .iter()
        .filter(|spec| {
            registry
                .get(&spec.function.name)
                .is_some_and(|tool| tool.kind() == ToolKind::Mcp)
        })
        .filter_map(|spec| {
            spec.function
                .name
                .split_once("__")
                .map(|(server, _)| server.to_string())
        })
        .collect();
    let mut out: Vec<Arc<dyn Tool>> = vec![
        Arc::new(SubagentHandbackTool),
        Arc::new(WaitForMcpServersTool { connected }),
        Arc::new(EndConversationTool),
        Arc::new(SendFeedbackTool),
    ];
    if let Some(powershell) = PowerShellTool::detect(registry) {
        out.push(Arc::new(powershell));
    }
    out
}

/// `SubagentHandback`: a subagent's report, handed back by name.
pub struct SubagentHandbackTool;

#[async_trait]
impl Tool for SubagentHandbackTool {
    fn name(&self) -> &str {
        "SubagentHandback"
    }

    fn description(&self) -> &str {
        "Hand your final report back to the agent that started you. Call it once, when the \
         task is done, with everything the caller needs; then stop. Only a subagent can call \
         it."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "report": { "type": "string", "description": "The report for the agent that started you. It is returned to them as your result." }
            },
            "required": ["report"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        if ctx.parent_inbox.is_none() {
            return Ok(ToolOutput::error(
                "SubagentHandback is only for a subagent; there is no caller to hand a report to",
            ));
        }
        let Some(report) = str_arg(&args, "report").filter(|r| !r.trim().is_empty()) else {
            return Ok(invalid(self.name(), "report is required"));
        };
        *ctx.handback.lock().unwrap_or_else(|e| e.into_inner()) = Some(report.to_string());
        Ok(ToolOutput::ok(
            "Report recorded. It is your result. End your turn now without calling more tools.",
        ))
    }
}

/// `WaitForMcpServers`.
pub struct WaitForMcpServersTool {
    connected: BTreeSet<String>,
}

#[async_trait]
impl Tool for WaitForMcpServersTool {
    fn name(&self) -> &str {
        "WaitForMcpServers"
    }

    fn description(&self) -> &str {
        "Wait for MCP servers that are still connecting and whose tools are not yet in your \
         tool list. Wizard connects every configured MCP server before the first turn, so \
         nothing is ever pending: this reports which of the servers you name are connected and \
         which are not."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "servers": { "type": "array", "items": { "type": "string" }, "description": "Servers to wait for; omit to check all of them" }
            }
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let named: Vec<String> = args
            .get("servers")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let ready: Vec<&String> = if named.is_empty() {
            self.connected.iter().collect()
        } else {
            named
                .iter()
                .filter(|n| self.connected.contains(*n))
                .collect()
        };
        let missing: Vec<&String> = named
            .iter()
            .filter(|n| !self.connected.contains(*n))
            .collect();
        let list = |names: &[&String]| {
            names
                .iter()
                .map(|n| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut text = if missing.is_empty() {
            format!("ready=true. Connected: {}.", {
                let joined = list(&ready);
                if joined.is_empty() {
                    "none".to_string()
                } else {
                    joined
                }
            })
        } else {
            format!(
                "ready=false. Not connected: {} (failed to connect, need authentication, are \
                 disabled, or are not configured). Check `wizard mcp list`.",
                list(&missing)
            )
        };
        if !missing.is_empty() && !ready.is_empty() {
            text.push_str(&format!(" Connected: {}.", list(&ready)));
        }
        Ok(ToolOutput::ok(text))
    }
}

/// `EndConversation`.
pub struct EndConversationTool;

#[async_trait]
impl Tool for EndConversationTool {
    fn name(&self) -> &str {
        "EndConversation"
    }

    fn description(&self) -> &str {
        "End this conversation. Use it only as a last resort, when the user keeps asking for \
         something you will not do after you have said so clearly. The session stays open for \
         the user; you stop replying."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, _args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        if let Some(events) = &ctx.events {
            let _ = events
                .send(crate::agent::AgentEvent::Notice(
                    "the assistant ended the conversation".to_string(),
                ))
                .await;
        }
        Ok(ToolOutput::ok(
            "The conversation is ended. Do not reply further and do not call any more tools.",
        ))
    }
}

/// `SendFeedback`.
pub struct SendFeedbackTool;

impl SendFeedbackTool {
    fn dir() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join(".wizard")
            .join("feedback")
    }
}

#[async_trait]
impl Tool for SendFeedbackTool {
    fn name(&self) -> &str {
        "SendFeedback"
    }

    fn description(&self) -> &str {
        "Write feedback about this session to a local draft file. Nothing is sent anywhere; the \
         user decides whether to share it."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "description": { "type": "string", "description": "What went wrong, or what should be different" }
            },
            "required": ["description"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::Execute
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let Some(text) = str_arg(&args, "description")
            .or_else(|| str_arg(&args, "message"))
            .filter(|t| !t.trim().is_empty())
        else {
            return Ok(invalid(self.name(), "description is required"));
        };
        let dir = Self::dir();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let path = dir.join(format!("feedback-{stamp}-{}.md", std::process::id()));
        let body = format!(
            "# Feedback draft\n\nProject: {}\nWritten: unix {stamp}\n\n{text}\n",
            ctx.cwd.display()
        );
        if let Err(err) = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, body)) {
            return Ok(ToolOutput::error(format!(
                "Could not write the feedback draft to {}: {err}",
                path.display()
            )));
        }
        Ok(ToolOutput::ok(format!(
            "Feedback saved to {}. Nothing was sent.",
            path.display()
        )))
    }
}

/// `PowerShell`: Claude's schema, run through `execute` as an encoded command
/// so no quoting survives to be wrong.
pub struct PowerShellTool {
    inner: Arc<dyn Tool>,
    program: &'static str,
}

impl PowerShellTool {
    /// Registered where a PowerShell exists: `powershell.exe` on Windows,
    /// `pwsh` anywhere it is installed. Everywhere else the tool is absent.
    fn detect(registry: &ToolRegistry) -> Option<Self> {
        let inner = registry.get("execute")?;
        let program = ["pwsh", "powershell", "powershell.exe"]
            .into_iter()
            .find(|name| super::lsp::on_path(name))?;
        Some(Self {
            inner: Arc::clone(inner),
            program,
        })
    }
}

/// Standard base64, for `-EncodedCommand`.
fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | ((*b as u32) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The `execute` arguments that run `script` in PowerShell.
fn encoded_command(program: &str, script: &str) -> String {
    let utf16: Vec<u8> = script
        .encode_utf16()
        .flat_map(|unit| unit.to_le_bytes())
        .collect();
    format!(
        "{program} -NoProfile -NonInteractive -EncodedCommand {}",
        base64(&utf16)
    )
}

#[async_trait]
impl Tool for PowerShellTool {
    fn name(&self) -> &str {
        "PowerShell"
    }

    fn description(&self) -> &str {
        "Run a PowerShell command and return its output and exit code."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The PowerShell command to execute" },
                "timeout": { "type": "number", "description": "Optional timeout in milliseconds (max 600000)" },
                "description": { "type": "string", "description": "What the command does, in a few plain words" },
                "run_in_background": { "type": "boolean", "description": "Set to true to run this command in the background." }
            },
            "required": ["command"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::Execute
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let Some(script) = str_arg(&args, "command").filter(|c| !c.trim().is_empty()) else {
            return Ok(invalid(self.name(), "command is required"));
        };
        let mut mapped = json!({ "command": encoded_command(self.program, script) });
        if let Some(ms) = args
            .get("timeout")
            .and_then(Value::as_f64)
            .filter(|ms| *ms > 0.0)
        {
            mapped["timeout_secs"] = json!(((ms / 1000.0).ceil() as u64).clamp(1, 600));
            mapped["kill_on_timeout"] = json!(true);
        }
        if let Some(background) = args.get("run_in_background").and_then(Value::as_bool) {
            mapped["run_in_background"] = json!(background);
        }
        self.inner.execute(mapped, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn a_script_becomes_an_encoded_utf16_command() {
        // `Get-Date` as UTF-16LE is 47 00 65 00 ..., so it starts "RwBlAHQ"
        let line = encoded_command("pwsh", "Get-Date");
        assert!(line.starts_with("pwsh -NoProfile -NonInteractive -EncodedCommand RwBlAHQA"));
    }

    #[tokio::test]
    async fn handback_needs_a_caller_and_records_the_report() {
        let mut ctx = ToolContext::new(std::env::temp_dir());
        let alone = SubagentHandbackTool
            .execute(json!({ "report": "done" }), &ctx)
            .await
            .unwrap();
        assert!(alone.is_error);
        ctx.parent_inbox = Some(Arc::default());
        let out = SubagentHandbackTool
            .execute(json!({ "report": "found 3 files" }), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            ctx.handback.lock().unwrap().as_deref(),
            Some("found 3 files")
        );
        let empty = SubagentHandbackTool
            .execute(json!({ "report": " " }), &ctx)
            .await
            .unwrap();
        assert!(empty.is_error);
    }

    #[tokio::test]
    async fn waiting_reports_what_is_connected() {
        let tool = WaitForMcpServersTool {
            connected: ["github".to_string()].into_iter().collect(),
        };
        let ctx = ToolContext::new(std::env::temp_dir());
        let all = tool.execute(json!({}), &ctx).await.unwrap();
        assert_eq!(all.content, "ready=true. Connected: github.");
        let missing = tool
            .execute(json!({ "servers": ["github", "linear"] }), &ctx)
            .await
            .unwrap();
        assert!(
            missing
                .content
                .starts_with("ready=false. Not connected: linear")
        );
        assert!(missing.content.ends_with("Connected: github."));
    }

    #[tokio::test]
    async fn end_conversation_tells_the_model_to_stop() {
        let ctx = ToolContext::new(std::env::temp_dir());
        let out = EndConversationTool.execute(json!({}), &ctx).await.unwrap();
        assert!(out.content.contains("Do not reply further"));
    }
}
