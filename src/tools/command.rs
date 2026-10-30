//! The `run_command` tool: lets the agent invoke Wizard's own slash commands.
//!
//! The agent calls this with a command line exactly as a user would type it
//! (`/effort high`, `/model claude-sonnet-5`, `/status`, …). The tool parses
//! and validates it against [`SlashCommand`](crate::commands::SlashCommand) — the
//! same parser the prompt uses — and checks
//! [`SlashCommand::agent_runnable`](crate::commands::SlashCommand::agent_runnable),
//! which gates out interactive-only, session-ending, and external-setup
//! commands. A valid, allowed command is handed to the interactive surface via
//! [`AgentEvent::CommandRequested`]; the surface dispatches it once the turn
//! finishes and the agent is back in its slot (a turn already in flight cannot
//! be reconfigured), so effort/model/mode changes take effect on the next turn.
//!
//! Only a surface with somewhere to put a command drains that queue, so the tool
//! gates on [`ToolContext::command_dispatch`](crate::tools::CommandDispatch) —
//! every command on the TUI, the subset its executor implements on the GUI, none
//! at all headless. Headless and gateway runs have a live event channel too (it
//! streams to a printer), so a presence check on `ctx.events` alone would wrongly
//! report "queued"; the capability refuses honestly there, as it does for
//! subagents and direct registry execution.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent::AgentEvent;
use crate::commands::SlashCommand;

use super::{Tool, ToolAccess, ToolContext, ToolError, ToolOutput, parse_args};

/// Advertised name of the tool.
pub const RUN_COMMAND_TOOL_NAME: &str = "run_command";

/// Lets the agent run Wizard's built-in slash commands.
pub struct RunCommandTool;

#[async_trait]
impl Tool for RunCommandTool {
    fn name(&self) -> &str {
        RUN_COMMAND_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Run one of Wizard's own slash commands — the same ones the user can \
         type at the prompt. Pass the command exactly as typed, including any \
         arguments: e.g. `/effort high`, `/model claude-sonnet-5`, `/mode \
         sovereign`, `/goal ship the release`, `/reload`, `/status`, \
         `/diff`, `/settings`. For mid-turn history summarization prefer the \
         native `compact` tool (works headless/gateway too); `run_command` \
         `/compact` only queues until this turn ends and is interactive-only. \
         Configuration changes (effort, model, mode) apply once the current \
         turn finishes — a request already in flight cannot be reconfigured. \
         Interactive pickers require the choice as an argument (`/effort` \
         alone is refused; `/effort high` is fine). Session-ending and \
         external-setup commands (`/quit`, `/clear`, `/rewind`, `/resume`, \
         `/provider`, `/login`, `/publish`, `/evolve`) are refused, and so \
         are `/goal clear` and `/goal resume`, and setting a new goal while \
         a goal loop is running. Run \
         `/help` to list every command."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The slash command to run, exactly as typed at the prompt \
                                    (with or without the leading '/'), e.g. '/effort high'."
                }
            },
            "required": ["command"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::Execute
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        #[derive(Deserialize)]
        struct Args {
            command: String,
        }
        let Args { command } = parse_args(RUN_COMMAND_TOOL_NAME, args)?;

        // Normalize to a leading '/' so the model may pass either form.
        let trimmed = command.trim();
        if trimmed.is_empty() {
            return Ok(ToolOutput::error("no command given (e.g. '/effort high')"));
        }
        let line = if trimmed.starts_with('/') {
            trimmed.to_string()
        } else {
            format!("/{trimmed}")
        };

        // Validate against the one shared parser, then the agent allowlist.
        let parsed = match SlashCommand::parse(&line) {
            Some(Ok(parsed)) => parsed,
            Some(Err(message)) => return Ok(ToolOutput::error(message)),
            None => {
                return Ok(ToolOutput::error(format!(
                    "'{line}' is not a slash command"
                )));
            }
        };
        if let Err(reason) = parsed.agent_runnable() {
            return Ok(ToolOutput::error(reason));
        }

        // Only a surface that drains and applies queued commands may accept one.
        // A live `events` channel alone isn't enough — headless and gateway runs
        // also have one, but stream to a printer that can't apply a command —
        // and a surface that implements only some of them must refuse the rest
        // here, in the result the model reads, rather than swallow them after
        // the turn.
        let name = line.trim_start_matches('/');
        let name = name.split_whitespace().next().unwrap_or(name);
        if let Err(reason) = ctx.command_dispatch.accepts(name) {
            return Ok(ToolOutput::error(reason));
        }
        let Some(events) = ctx.events.clone() else {
            return Ok(ToolOutput::error(
                "no interactive surface is attached to dispatch the command",
            ));
        };
        if events
            .send(AgentEvent::CommandRequested(line.clone()))
            .await
            .is_err()
        {
            return Ok(ToolOutput::error(
                "the session is no longer accepting commands",
            ));
        }

        Ok(ToolOutput::ok(format!(
            "queued `{line}` — it runs once this turn finishes"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::CommandDispatch;
    use tokio::sync::mpsc;

    /// A TUI-like context: a live event channel and every command dispatchable.
    fn ctx_with_events() -> (ToolContext, mpsc::Receiver<AgentEvent>) {
        let (tx, rx) = mpsc::channel(8);
        let ctx = ToolContext::new(std::env::temp_dir())
            .with_command_dispatch(CommandDispatch::All)
            .with_events(tx);
        (ctx, rx)
    }

    #[tokio::test]
    async fn queues_a_valid_command_and_normalizes_the_slash() {
        let (ctx, mut rx) = ctx_with_events();
        let out = RunCommandTool
            .execute(json!({ "command": "effort high" }), &ctx)
            .await
            .expect("executes");
        assert!(!out.is_error, "{}", out.content);
        match rx.recv().await {
            Some(AgentEvent::CommandRequested(line)) => assert_eq!(line, "/effort high"),
            other => panic!("expected CommandRequested, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_an_interactive_picker_without_an_argument() {
        let (ctx, _rx) = ctx_with_events();
        let out = RunCommandTool
            .execute(json!({ "command": "/effort" }), &ctx)
            .await
            .expect("executes");
        assert!(out.is_error);
        assert!(out.content.contains("name a level"), "{}", out.content);
    }

    #[tokio::test]
    async fn refuses_session_ending_commands() {
        let (ctx, _rx) = ctx_with_events();
        for cmd in ["/quit", "/clear", "/rewind 2", "/login xai"] {
            let out = RunCommandTool
                .execute(json!({ "command": cmd }), &ctx)
                .await
                .expect("executes");
            assert!(out.is_error, "{cmd} should be refused");
        }
    }

    #[tokio::test]
    async fn reports_a_parse_error_from_the_shared_parser() {
        let (ctx, _rx) = ctx_with_events();
        let out = RunCommandTool
            .execute(json!({ "command": "/mode nonsense" }), &ctx)
            .await
            .expect("executes");
        assert!(out.is_error);
        assert!(out.content.contains("unknown mode"), "{}", out.content);
    }

    #[tokio::test]
    async fn refuses_on_a_non_dispatching_surface_even_with_a_live_channel() {
        // Headless / gateway: a channel exists, but nothing drains commands, so
        // the tool must refuse rather than report a false "queued".
        let (tx, mut rx) = mpsc::channel(8);
        let ctx = ToolContext::new(std::env::temp_dir()).with_events(tx);
        let out = RunCommandTool
            .execute(json!({ "command": "/status" }), &ctx)
            .await
            .expect("executes");
        assert!(out.is_error);
        assert!(out.content.contains("interactive"), "{}", out.content);
        // Nothing was emitted onto the channel.
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn errors_without_any_surface() {
        let ctx = ToolContext::new(std::env::temp_dir());
        let out = RunCommandTool
            .execute(json!({ "command": "/status" }), &ctx)
            .await
            .expect("executes");
        assert!(out.is_error);
        assert!(out.content.contains("interactive"), "{}", out.content);
    }

    /// A surface whose executor implements a subset (the GUI) queues what it can
    /// run and refuses the rest *here* — the tool result is the last thing the
    /// model reads about the command, so a silent drop after the turn would leave
    /// it believing `/effort high` took.
    #[tokio::test]
    async fn a_subset_surface_queues_what_it_runs_and_refuses_the_rest() {
        let (tx, mut rx) = mpsc::channel(8);
        let ctx = ToolContext::new(std::env::temp_dir())
            .with_command_dispatch(CommandDispatch::Only(&["compact", "model"]))
            .with_events(tx);

        let out = RunCommandTool
            .execute(json!({ "command": "/compact" }), &ctx)
            .await
            .expect("executes");
        assert!(!out.is_error, "{}", out.content);
        match rx.recv().await {
            Some(AgentEvent::CommandRequested(line)) => assert_eq!(line, "/compact"),
            other => panic!("expected CommandRequested, got {other:?}"),
        }

        // `/effort high` is agent-runnable, but this surface has nowhere to put
        // it: refused, and nothing is queued.
        let out = RunCommandTool
            .execute(json!({ "command": "/effort high" }), &ctx)
            .await
            .expect("executes");
        assert!(out.is_error);
        assert!(out.content.contains("/compact, /model"), "{}", out.content);
        assert!(rx.try_recv().is_err(), "nothing was queued");
    }

    #[tokio::test]
    async fn unknown_command_is_reported() {
        let (ctx, _rx) = ctx_with_events();
        let out = RunCommandTool
            .execute(json!({ "command": "/frobnicate" }), &ctx)
            .await
            .expect("executes");
        assert!(out.is_error);
    }
}
