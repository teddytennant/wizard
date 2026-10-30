//! One method per lifecycle event that is not a tool call, each a thin
//! wrapper over [`HookEngine::emit`] that names the payload fields Claude
//! Code documents for it.
//!
//! Where the call sits matters more than the wrapper. The wiring lives at the
//! place each thing happens: the agent loop for `Stop`, the subagent spawner
//! for `SubagentStart`/`SubagentStop`, the compactor for `PreCompact`, and so
//! on. `docs/hooks.md` lists them.

use std::path::Path;

use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::engine::{HookEngine, Outcome};
use super::event::HookEvent;
use crate::agent::AgentEvent;
use crate::config::Mode;

type Events<'a> = Option<&'a mpsc::Sender<AgentEvent>>;

/// One finished call in a `PostToolBatch` payload.
pub struct BatchCall<'a> {
    pub tool: &'a str,
    pub args: &'a Value,
    pub tool_use_id: &'a str,
    pub output: &'a str,
}

/// What a `PostToolBatch` hook asked for.
#[derive(Debug, Default)]
pub struct BatchVerdict {
    pub context: Option<String>,
    /// Stop the turn with this note.
    pub halt: Option<String>,
}

/// Consecutive `Stop` continuations before the loop lets the model stop
/// anyway. Claude Code uses the same number.
pub const STOP_CONTINUATION_CAP: u32 = 8;

impl HookEngine {
    /// `Stop`: the model finished. `Some(feedback)` means a hook wants the
    /// turn to go on, with that text as the next instruction.
    pub async fn stop(
        &self,
        last_message: &str,
        stop_hook_active: bool,
        mode: Mode,
        events: Events<'_>,
    ) -> Option<String> {
        let extra = json!({
            "stop_hook_active": stop_hook_active,
            "last_assistant_message": last_message,
            "background_tasks": [],
            "session_crons": [],
        });
        let out = self.emit(HookEvent::Stop, None, extra, mode, events).await;
        let context = out.context_text();
        out.block.or(context)
    }

    /// `StopFailure`: the turn ended on a provider error. Observational.
    pub async fn stop_failure(&self, kind: &str, details: &str, mode: Mode, events: Events<'_>) {
        let extra =
            json!({ "error": kind, "error_details": details, "last_assistant_message": details });
        self.emit(HookEvent::StopFailure, Some(kind), extra, mode, events)
            .await;
    }

    /// `SubagentStart`: context to hand the subagent, if a hook has some.
    pub async fn subagent_start(
        &self,
        agent_type: &str,
        agent_id: &str,
        mode: Mode,
        events: Events<'_>,
    ) -> Option<String> {
        let extra = json!({ "agent_id": agent_id, "agent_type": agent_type });
        self.emit(
            HookEvent::SubagentStart,
            Some(agent_type),
            extra,
            mode,
            events,
        )
        .await
        .context_text()
    }

    /// `SubagentStop`: `Some(feedback)` sends the subagent back to work.
    pub async fn subagent_stop(
        &self,
        agent_type: &str,
        agent_id: &str,
        last_message: &str,
        stop_hook_active: bool,
        mode: Mode,
        events: Events<'_>,
    ) -> Option<String> {
        let extra = json!({
            "agent_id": agent_id,
            "agent_type": agent_type,
            "stop_hook_active": stop_hook_active,
            "last_assistant_message": last_message,
        });
        let out = self
            .emit(
                HookEvent::SubagentStop,
                Some(agent_type),
                extra,
                mode,
                events,
            )
            .await;
        let context = out.context_text();
        out.block.or(context)
    }

    /// `PreCompact`: `Some(reason)` cancels the compaction.
    pub async fn pre_compact(
        &self,
        trigger: &str,
        instructions: Option<&str>,
        mode: Mode,
        events: Events<'_>,
    ) -> Option<String> {
        let extra = json!({ "trigger": trigger, "custom_instructions": instructions });
        let out = self
            .emit(HookEvent::PreCompact, Some(trigger), extra, mode, events)
            .await;
        out.block
    }

    /// `PostCompact`: observational.
    pub async fn post_compact(&self, trigger: &str, summary: &str, mode: Mode, events: Events<'_>) {
        let extra = json!({ "trigger": trigger, "compact_summary": summary });
        self.emit(HookEvent::PostCompact, Some(trigger), extra, mode, events)
            .await;
    }

    /// `PostToolBatch`: every call of one model response has resolved.
    pub async fn post_tool_batch(
        &self,
        calls: &[BatchCall<'_>],
        mode: Mode,
        events: Events<'_>,
    ) -> BatchVerdict {
        if !self.listens(HookEvent::PostToolBatch) {
            return BatchVerdict::default();
        }
        let calls: Vec<Value> = calls
            .iter()
            .map(|call| {
                json!({
                    "tool_name": super::toolmap::claude_name(call.tool),
                    "tool_input": super::toolmap::to_claude_input(call.tool, call.args),
                    "tool_use_id": call.tool_use_id,
                    "tool_response": call.output,
                })
            })
            .collect();
        let out = self
            .emit(
                HookEvent::PostToolBatch,
                None,
                json!({ "tool_calls": calls }),
                mode,
                events,
            )
            .await;
        BatchVerdict {
            context: out.context_text(),
            halt: out.block.or(out.halt),
        }
    }

    /// `UserPromptExpansion`: a slash command is about to expand. `Err` is a
    /// block; `Ok` may carry context for the expanded prompt.
    pub async fn user_prompt_expansion(
        &self,
        command_name: &str,
        command_args: &str,
        prompt: &str,
        mode: Mode,
        events: Events<'_>,
    ) -> Result<Option<String>, String> {
        let extra = json!({
            "expansion_type": "slash_command",
            "command_name": command_name,
            "command_args": command_args,
            "prompt": prompt,
        });
        let out = self
            .emit(
                HookEvent::UserPromptExpansion,
                Some(command_name),
                extra,
                mode,
                events,
            )
            .await;
        let context = out.context_text();
        match out.block.or(out.halt) {
            Some(reason) => Err(reason),
            None => Ok(context),
        }
    }

    /// `Notification`: Wizard has something to tell the user.
    pub async fn notification(
        &self,
        kind: &str,
        title: Option<&str>,
        message: &str,
        mode: Mode,
        events: Events<'_>,
    ) {
        let extra = json!({ "notification_type": kind, "title": title, "message": message });
        self.emit(HookEvent::Notification, Some(kind), extra, mode, events)
            .await;
    }

    /// `Elicitation`: the model asked the user a question. An `action` of
    /// `decline` or `cancel` from a hook answers it without asking.
    pub async fn elicitation(
        &self,
        server: &str,
        message: &str,
        schema: &Value,
        mode: Mode,
        events: Events<'_>,
    ) -> Outcome {
        let extra = json!({
            "mcp_server_name": server,
            "message": message,
            "requested_schema": schema,
        });
        self.emit(HookEvent::Elicitation, Some(server), extra, mode, events)
            .await
    }

    /// `ElicitationResult`: the user answered.
    pub async fn elicitation_result(
        &self,
        server: &str,
        action: &str,
        content: &Value,
        mode: Mode,
        events: Events<'_>,
    ) {
        let extra = json!({ "mcp_server_name": server, "action": action, "content": content });
        self.emit(
            HookEvent::ElicitationResult,
            Some(server),
            extra,
            mode,
            events,
        )
        .await;
    }

    /// `ConfigChange`: `Some(reason)` asks that the change not be applied.
    pub async fn config_change(
        &self,
        source: &str,
        file: Option<&Path>,
        mode: Mode,
        events: Events<'_>,
    ) -> Option<String> {
        let extra = json!({ "source": source, "file_path": file });
        self.emit(HookEvent::ConfigChange, Some(source), extra, mode, events)
            .await
            .block
    }

    /// `CwdChanged`.
    pub async fn cwd_changed(&self, old: &Path, new: &Path, mode: Mode, events: Events<'_>) {
        let extra = json!({ "old_cwd": old, "new_cwd": new });
        self.emit(HookEvent::CwdChanged, None, extra, mode, events)
            .await;
    }

    /// `InstructionsLoaded`: an instructions file entered the context.
    pub async fn instructions_loaded(
        &self,
        file: &Path,
        memory_type: &str,
        load_reason: &str,
        mode: Mode,
        events: Events<'_>,
    ) {
        let extra = json!({
            "file_path": file,
            "memory_type": memory_type,
            "load_reason": load_reason,
        });
        self.emit(
            HookEvent::InstructionsLoaded,
            Some(load_reason),
            extra,
            mode,
            events,
        )
        .await;
    }

    /// `TaskCreated`: `Err` vetoes the new to-do item.
    pub async fn task_created(
        &self,
        task_id: &str,
        subject: &str,
        mode: Mode,
        events: Events<'_>,
    ) -> Result<(), String> {
        let extra = json!({ "task_id": task_id, "task_subject": subject });
        match self
            .emit(HookEvent::TaskCreated, None, extra, mode, events)
            .await
            .block
        {
            Some(reason) => Err(reason),
            None => Ok(()),
        }
    }

    /// `TaskCompleted`: `Err` keeps the item open.
    pub async fn task_completed(
        &self,
        task_id: &str,
        subject: &str,
        mode: Mode,
        events: Events<'_>,
    ) -> Result<(), String> {
        let extra = json!({ "task_id": task_id, "task_subject": subject });
        match self
            .emit(HookEvent::TaskCompleted, None, extra, mode, events)
            .await
            .block
        {
            Some(reason) => Err(reason),
            None => Ok(()),
        }
    }

    /// `TeammateIdle`: `Some(feedback)` keeps the worker going.
    pub async fn teammate_idle(
        &self,
        name: &str,
        mode: Mode,
        events: Events<'_>,
    ) -> Option<String> {
        let extra = json!({ "teammate_name": name });
        self.emit(HookEvent::TeammateIdle, None, extra, mode, events)
            .await
            .block
    }

    /// `WorktreeCreate`.
    pub async fn worktree_create(&self, name: &str, mode: Mode, events: Events<'_>) {
        self.emit(
            HookEvent::WorktreeCreate,
            None,
            json!({ "name": name }),
            mode,
            events,
        )
        .await;
    }

    /// `WorktreeRemove`.
    pub async fn worktree_remove(&self, path: &Path, mode: Mode, events: Events<'_>) {
        self.emit(
            HookEvent::WorktreeRemove,
            None,
            json!({ "worktree_path": path }),
            mode,
            events,
        )
        .await;
    }

    /// `DirectoryAdded`.
    pub async fn directory_added(&self, dir: &Path, source: &str, mode: Mode, events: Events<'_>) {
        let extra = json!({ "directory": dir, "source": source });
        self.emit(HookEvent::DirectoryAdded, Some(source), extra, mode, events)
            .await;
    }

    /// `MessageDisplay`: assistant text reached the user. Observational.
    pub async fn message_display(&self, text: &str, mode: Mode, events: Events<'_>) {
        if !self.listens(HookEvent::MessageDisplay) {
            return;
        }
        let extra = json!({ "index": 0, "final": true, "delta": text });
        self.emit(HookEvent::MessageDisplay, None, extra, mode, events)
            .await;
    }

    /// `PreModelSwitch`: `Some(reason)` refuses the switch.
    #[allow(clippy::too_many_arguments)]
    pub async fn pre_model_switch(
        &self,
        from: &str,
        to: &str,
        source: &str,
        mode: Mode,
        events: Events<'_>,
    ) -> Option<String> {
        let extra = json!({
            "from_model": from,
            "to_model": to,
            "requested_model": to,
            "source": source,
        });
        let out = self
            .emit(HookEvent::PreModelSwitch, Some(to), extra, mode, events)
            .await;
        out.block.or_else(|| {
            (out.verdict == Some(super::protocol::Verdict::Deny)).then(|| {
                out.verdict_reason
                    .clone()
                    .unwrap_or_else(|| "a hook refused the model switch".to_string())
            })
        })
    }

    /// `PostModelSwitch`: context for the next request, if any.
    pub async fn post_model_switch(
        &self,
        from: &str,
        to: &str,
        source: &str,
        mode: Mode,
        events: Events<'_>,
    ) -> Option<String> {
        let extra = json!({
            "from_model": from,
            "to_model": to,
            "requested_model": to,
            "source": source,
        });
        self.emit(HookEvent::PostModelSwitch, Some(to), extra, mode, events)
            .await
            .context_text()
    }

    /// `Setup`: one-time project setup.
    pub async fn setup(&self, trigger: &str, mode: Mode, events: Events<'_>) {
        let extra = json!({ "trigger": trigger });
        self.emit(HookEvent::Setup, Some(trigger), extra, mode, events)
            .await;
    }
}
