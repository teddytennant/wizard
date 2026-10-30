//! `Monitor`: a background command whose stdout lines reach the model as they
//! arrive.
//!
//! It is a background task in the same registry `execute` uses
//! ([`TaskRegistry::spawn_monitor`]), so `task_output`, `task_kill` and
//! `TaskStop` already manage it, and a finished one is reported the usual way.
//! What it adds is the line events: the agent drains them into history at the
//! top of every step, as it does a finished task, so they arrive between model
//! calls. Like a finished task they do not wake an idle session.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{invalid, str_arg};
use crate::platform::process::ProcessGroupExt;
use crate::platform::shell;
use crate::tools::{Tool, ToolAccess, ToolContext, ToolError, ToolOutput};

/// Default watch length, as in Claude Code.
const DEFAULT_TIMEOUT_MS: u64 = 300_000;

/// Longest watch. The schema allows more; this is where it is capped.
const MAX_TIMEOUT_MS: u64 = 1_800_000;

pub fn tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(MonitorTool)]
}

/// `Monitor`.
pub struct MonitorTool;

#[async_trait]
impl Tool for MonitorTool {
    fn name(&self) -> &str {
        "Monitor"
    }

    fn description(&self) -> &str {
        "Start a background command and get each stdout line as an event, between your steps, \
         until it exits or timeout_ms passes (default 5 minutes, at most 30). For one \
         notification when something finishes, run it with execute in the background instead. \
         Filter to the lines you would act on and include failure signatures: silence is not \
         success. Pipe stages need line buffering (grep --line-buffered). Only stdout is the \
         event stream; stderr goes to task_output. Stop it with TaskStop."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "description": { "type": "string", "description": "Short description of what you are monitoring, shown with each event." },
                "timeout_ms": { "type": "number", "minimum": 1000, "maximum": 3600000, "default": 300000, "description": "Kill the monitor after this long. Default 300000; above 1800000 is capped." },
                "command": { "type": "string", "description": "Shell command or script. Each stdout line is an event; exit ends the watch." },
                "ws": { "type": "object", "description": "Not supported; use a command such as websocat.", "properties": { "url": { "type": "string" }, "protocols": { "type": "array", "items": { "type": "string" } } } }
            },
            "required": ["description", "timeout_ms"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::Execute
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        if args.get("ws").is_some_and(|ws| !ws.is_null()) {
            return Ok(invalid(
                self.name(),
                "the ws source is not supported in Wizard; stream frames with a command such as `websocat wss://...`",
            ));
        }
        let (Some(description), Some(command)) =
            (str_arg(&args, "description"), str_arg(&args, "command"))
        else {
            return Ok(invalid(self.name(), "description and command are required"));
        };
        if command.trim().is_empty() {
            return Ok(invalid(self.name(), "command must not be empty"));
        }
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(Value::as_f64)
            .map_or(DEFAULT_TIMEOUT_MS, |ms| ms.max(1000.0) as u64)
            .min(MAX_TIMEOUT_MS);

        let mut cmd = shell::tokio_command_pipefail(command);
        cmd.current_dir(&ctx.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .own_process_group();
        let child = cmd.spawn().map_err(|err| ToolError::Execution {
            tool: self.name().to_string(),
            source: anyhow::Error::new(err).context("failed to spawn the monitor"),
        })?;
        let id = ctx.tasks.spawn_monitor(
            description,
            command,
            child,
            Duration::from_millis(timeout_ms),
        );
        if let Some(events) = &ctx.events {
            let _ = events
                .send(crate::agent::AgentEvent::TaskStarted {
                    id,
                    command: command.to_string(),
                })
                .await;
        }
        Ok(ToolOutput::ok(format!(
            "Monitor #{id} started: {description}. Each stdout line arrives as a note between \
             your steps. It ends when the command exits or after {}s; TaskStop #{id} cancels it.",
            timeout_ms / 1000
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::tasks::TaskStatus;

    async fn wait_finished(ctx: &ToolContext, id: u32) {
        for _ in 0..200 {
            if ctx.tasks.status(id).is_some_and(TaskStatus::is_finished) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("monitor #{id} did not finish");
    }

    #[tokio::test]
    async fn stdout_lines_become_events_and_stderr_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        let out = MonitorTool
            .execute(
                json!({ "description": "demo", "timeout_ms": 5000,
                        "command": "echo one; echo oops >&2; echo two" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        wait_finished(&ctx, 1).await;

        let batches = ctx.tasks.drain_monitor_events();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].description, "demo");
        assert_eq!(batches[0].lines, vec!["one", "two"]);
        assert!(
            ctx.tasks.drain_monitor_events().is_empty(),
            "each line is reported once"
        );
        let (_, output) = ctx.tasks.output(1, 1000).unwrap();
        assert!(output.contains("oops"), "stderr is still in the buffer");
    }

    #[tokio::test]
    async fn a_line_without_a_newline_is_flushed_when_the_command_exits() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        MonitorTool
            .execute(
                json!({ "description": "d", "timeout_ms": 5000, "command": "printf tail" }),
                &ctx,
            )
            .await
            .unwrap();
        wait_finished(&ctx, 1).await;
        let batches = ctx.tasks.drain_monitor_events();
        assert_eq!(batches[0].lines, vec!["tail"]);
    }

    #[tokio::test]
    async fn the_deadline_ends_the_watch() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        MonitorTool
            .execute(
                json!({ "description": "d", "timeout_ms": 1000, "command": "sleep 30" }),
                &ctx,
            )
            .await
            .unwrap();
        wait_finished(&ctx, 1).await;
        assert_eq!(ctx.tasks.status(1), Some(TaskStatus::TimedOut));
    }

    #[tokio::test]
    async fn ws_is_refused_and_a_command_is_required() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        let ws = MonitorTool
            .execute(
                json!({ "description": "d", "timeout_ms": 1000, "ws": { "url": "wss://x" } }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(ws.is_error);
        let none = MonitorTool
            .execute(json!({ "description": "d", "timeout_ms": 1000 }), &ctx)
            .await
            .unwrap();
        assert!(none.is_error);
    }
}
