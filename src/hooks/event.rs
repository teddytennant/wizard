//! The lifecycle points a hook can attach to.
//!
//! Wizard's own `hooks.toml` has always used snake_case names, and Claude
//! Code's settings files use PascalCase ones. One enum carries both spellings
//! so a hook declared either way lands on the same event.

use serde::Deserialize;

/// Lifecycle points a hook can attach to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    /// Before a tool call executes. May rewrite the arguments or block.
    PreToolUse,
    /// After a tool call executes. Stdout is appended to the tool result.
    PostToolUse,
    /// When a user message starts a turn. Stdout is appended to the message;
    /// exit 2 ends the turn before the model sees it.
    UserPromptSubmit,
    /// Once when a session begins. Stdout becomes system context.
    SessionStart,
    /// Once when a session ends. Observational.
    SessionEnd,
    /// After every turn finishes. Observational. Wizard's own event; Claude
    /// Code's closest relative is `Stop`, which can also keep the turn going.
    TurnEnd,
    /// A tool ran and reported failure.
    PostToolUseFailure,
    /// Every tool call of one model response has resolved.
    PostToolBatch,
    /// A user-typed slash command is about to expand into a prompt.
    UserPromptExpansion,
    /// The model finished its turn. May send it back to work.
    Stop,
    /// The turn ended on a provider error instead of finishing.
    StopFailure,
    /// A subagent is about to start.
    SubagentStart,
    /// A subagent finished. May send it back to work.
    SubagentStop,
    /// Before the conversation is compacted.
    PreCompact,
    /// After the conversation was compacted.
    PostCompact,
    /// One-time project setup (`wizard init`).
    Setup,
    /// Wizard has something to tell the user.
    Notification,
    /// A tool call needs approval (an `ask` rule matched).
    PermissionRequest,
    /// A tool call was refused by a permission rule or mode.
    PermissionDenied,
    /// The model asked the user a question through the interview tool.
    Elicitation,
    /// The user answered that question.
    ElicitationResult,
    /// A settings or config file changed.
    ConfigChange,
    /// The working directory changed.
    CwdChanged,
    /// A file changed on disk.
    FileChanged,
    /// An instructions file (`WIZARD.md`, `CLAUDE.md`, ...) was loaded.
    InstructionsLoaded,
    /// A to-do item was created.
    TaskCreated,
    /// A to-do item was completed.
    TaskCompleted,
    /// A fleet worker ran out of work.
    TeammateIdle,
    /// A git worktree was created.
    WorktreeCreate,
    /// A git worktree was removed.
    WorktreeRemove,
    /// A directory was added to the session's working set.
    DirectoryAdded,
    /// Assistant text was shown to the user.
    MessageDisplay,
    /// Before the active model changes.
    PreModelSwitch,
    /// After the active model changed.
    PostModelSwitch,
}

impl HookEvent {
    /// Every event, in the order the docs list them.
    pub const ALL: [HookEvent; 34] = [
        HookEvent::PreToolUse,
        HookEvent::PostToolUse,
        HookEvent::PostToolUseFailure,
        HookEvent::PostToolBatch,
        HookEvent::UserPromptSubmit,
        HookEvent::UserPromptExpansion,
        HookEvent::Stop,
        HookEvent::StopFailure,
        HookEvent::SubagentStart,
        HookEvent::SubagentStop,
        HookEvent::PreCompact,
        HookEvent::PostCompact,
        HookEvent::SessionStart,
        HookEvent::SessionEnd,
        HookEvent::Setup,
        HookEvent::Notification,
        HookEvent::PermissionRequest,
        HookEvent::PermissionDenied,
        HookEvent::Elicitation,
        HookEvent::ElicitationResult,
        HookEvent::ConfigChange,
        HookEvent::CwdChanged,
        HookEvent::FileChanged,
        HookEvent::InstructionsLoaded,
        HookEvent::TaskCreated,
        HookEvent::TaskCompleted,
        HookEvent::TeammateIdle,
        HookEvent::WorktreeCreate,
        HookEvent::WorktreeRemove,
        HookEvent::DirectoryAdded,
        HookEvent::MessageDisplay,
        HookEvent::PreModelSwitch,
        HookEvent::PostModelSwitch,
        HookEvent::TurnEnd,
    ];

    /// The snake_case name used in `hooks.toml` and the stdin payload.
    pub fn name(self) -> &'static str {
        match self {
            HookEvent::PreToolUse => "pre_tool_use",
            HookEvent::PostToolUse => "post_tool_use",
            HookEvent::UserPromptSubmit => "user_prompt_submit",
            HookEvent::SessionStart => "session_start",
            HookEvent::SessionEnd => "session_end",
            HookEvent::TurnEnd => "turn_end",
            HookEvent::PostToolUseFailure => "post_tool_use_failure",
            HookEvent::PostToolBatch => "post_tool_batch",
            HookEvent::UserPromptExpansion => "user_prompt_expansion",
            HookEvent::Stop => "stop",
            HookEvent::StopFailure => "stop_failure",
            HookEvent::SubagentStart => "subagent_start",
            HookEvent::SubagentStop => "subagent_stop",
            HookEvent::PreCompact => "pre_compact",
            HookEvent::PostCompact => "post_compact",
            HookEvent::Setup => "setup",
            HookEvent::Notification => "notification",
            HookEvent::PermissionRequest => "permission_request",
            HookEvent::PermissionDenied => "permission_denied",
            HookEvent::Elicitation => "elicitation",
            HookEvent::ElicitationResult => "elicitation_result",
            HookEvent::ConfigChange => "config_change",
            HookEvent::CwdChanged => "cwd_changed",
            HookEvent::FileChanged => "file_changed",
            HookEvent::InstructionsLoaded => "instructions_loaded",
            HookEvent::TaskCreated => "task_created",
            HookEvent::TaskCompleted => "task_completed",
            HookEvent::TeammateIdle => "teammate_idle",
            HookEvent::WorktreeCreate => "worktree_create",
            HookEvent::WorktreeRemove => "worktree_remove",
            HookEvent::DirectoryAdded => "directory_added",
            HookEvent::MessageDisplay => "message_display",
            HookEvent::PreModelSwitch => "pre_model_switch",
            HookEvent::PostModelSwitch => "post_model_switch",
        }
    }

    /// The PascalCase name Claude Code uses in `settings.json` and in the
    /// `hook_event_name` payload field. `TurnEnd` has none; it reports as
    /// `Stop`'s observational cousin under its own name.
    pub fn claude_name(self) -> &'static str {
        match self {
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
            HookEvent::SessionStart => "SessionStart",
            HookEvent::SessionEnd => "SessionEnd",
            HookEvent::TurnEnd => "TurnEnd",
            HookEvent::PostToolUseFailure => "PostToolUseFailure",
            HookEvent::PostToolBatch => "PostToolBatch",
            HookEvent::UserPromptExpansion => "UserPromptExpansion",
            HookEvent::Stop => "Stop",
            HookEvent::StopFailure => "StopFailure",
            HookEvent::SubagentStart => "SubagentStart",
            HookEvent::SubagentStop => "SubagentStop",
            HookEvent::PreCompact => "PreCompact",
            HookEvent::PostCompact => "PostCompact",
            HookEvent::Setup => "Setup",
            HookEvent::Notification => "Notification",
            HookEvent::PermissionRequest => "PermissionRequest",
            HookEvent::PermissionDenied => "PermissionDenied",
            HookEvent::Elicitation => "Elicitation",
            HookEvent::ElicitationResult => "ElicitationResult",
            HookEvent::ConfigChange => "ConfigChange",
            HookEvent::CwdChanged => "CwdChanged",
            HookEvent::FileChanged => "FileChanged",
            HookEvent::InstructionsLoaded => "InstructionsLoaded",
            HookEvent::TaskCreated => "TaskCreated",
            HookEvent::TaskCompleted => "TaskCompleted",
            HookEvent::TeammateIdle => "TeammateIdle",
            HookEvent::WorktreeCreate => "WorktreeCreate",
            HookEvent::WorktreeRemove => "WorktreeRemove",
            HookEvent::DirectoryAdded => "DirectoryAdded",
            HookEvent::MessageDisplay => "MessageDisplay",
            HookEvent::PreModelSwitch => "PreModelSwitch",
            HookEvent::PostModelSwitch => "PostModelSwitch",
        }
    }

    /// Parse a Claude Code event name. `TurnEnd` is Wizard's own and is not
    /// accepted here; an unknown name is `None` so a settings file that names
    /// a future event still loads.
    pub fn from_claude_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|event| *event != HookEvent::TurnEnd && event.claude_name() == name)
    }

    /// True when a nonzero verdict from a hook on this event changes what
    /// Wizard does next (blocks a tool, a prompt, a stop, ...).
    pub fn blockable(self) -> bool {
        matches!(
            self,
            HookEvent::PreToolUse
                | HookEvent::UserPromptSubmit
                | HookEvent::UserPromptExpansion
                | HookEvent::PostToolBatch
                | HookEvent::Stop
                | HookEvent::SubagentStop
                | HookEvent::PreCompact
                | HookEvent::ConfigChange
                | HookEvent::TaskCreated
                | HookEvent::TaskCompleted
                | HookEvent::TeammateIdle
                | HookEvent::PreModelSwitch
                | HookEvent::Elicitation
                | HookEvent::ElicitationResult
        )
    }

    /// True when plain stdout from an exit-0 hook on this event is handed to
    /// the model as context. Claude Code limits this to four events; the six
    /// events Wizard had first keep the behavior they always had.
    pub fn stdout_is_context(self) -> bool {
        matches!(
            self,
            HookEvent::UserPromptSubmit
                | HookEvent::UserPromptExpansion
                | HookEvent::SessionStart
                | HookEvent::PostModelSwitch
        )
    }

    /// True when a tool name is what a matcher is tested against.
    pub fn is_tool_event(self) -> bool {
        matches!(
            self,
            HookEvent::PreToolUse
                | HookEvent::PostToolUse
                | HookEvent::PostToolUseFailure
                | HookEvent::PermissionRequest
                | HookEvent::PermissionDenied
        )
    }

    /// True when the event has a matcher subject at all. A matcher on any
    /// other event is ignored.
    pub fn has_matcher(self) -> bool {
        self.is_tool_event()
            || matches!(
                self,
                HookEvent::SessionStart
                    | HookEvent::SessionEnd
                    | HookEvent::Setup
                    | HookEvent::Notification
                    | HookEvent::PreCompact
                    | HookEvent::PostCompact
                    | HookEvent::SubagentStart
                    | HookEvent::SubagentStop
                    | HookEvent::StopFailure
                    | HookEvent::ConfigChange
                    | HookEvent::FileChanged
                    | HookEvent::InstructionsLoaded
                    | HookEvent::UserPromptExpansion
                    | HookEvent::Elicitation
                    | HookEvent::ElicitationResult
                    | HookEvent::DirectoryAdded
                    | HookEvent::PreModelSwitch
                    | HookEvent::PostModelSwitch
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// The 33 events Claude Code documents, spelled as it spells them.
    const CLAUDE_EVENTS: [&str; 33] = [
        "PreToolUse",
        "PostToolUse",
        "PostToolUseFailure",
        "PostToolBatch",
        "UserPromptSubmit",
        "UserPromptExpansion",
        "Stop",
        "StopFailure",
        "SubagentStart",
        "SubagentStop",
        "PreCompact",
        "PostCompact",
        "SessionStart",
        "SessionEnd",
        "Setup",
        "Notification",
        "PermissionRequest",
        "PermissionDenied",
        "Elicitation",
        "ElicitationResult",
        "ConfigChange",
        "CwdChanged",
        "FileChanged",
        "InstructionsLoaded",
        "TaskCreated",
        "TaskCompleted",
        "TeammateIdle",
        "WorktreeCreate",
        "WorktreeRemove",
        "DirectoryAdded",
        "MessageDisplay",
        "PreModelSwitch",
        "PostModelSwitch",
    ];

    #[test]
    fn every_claude_event_has_a_wizard_event() {
        for name in CLAUDE_EVENTS {
            let event = HookEvent::from_claude_name(name)
                .unwrap_or_else(|| panic!("{name} has no wizard event"));
            assert_eq!(event.claude_name(), name);
        }
    }

    #[test]
    fn names_are_unique_and_round_trip_through_serde() {
        let mut snake = HashSet::new();
        let mut pascal = HashSet::new();
        for event in HookEvent::ALL {
            assert!(snake.insert(event.name()), "duplicate {}", event.name());
            assert!(pascal.insert(event.claude_name()));
            let parsed: HookEvent = serde_json::from_value(serde_json::json!(event.name()))
                .unwrap_or_else(|err| panic!("{}: {err}", event.name()));
            assert_eq!(parsed, event);
        }
        assert_eq!(snake.len(), 34, "33 claude events plus turn_end");
    }

    #[test]
    fn turn_end_is_not_a_claude_event_and_unknown_names_are_none() {
        assert_eq!(HookEvent::from_claude_name("TurnEnd"), None);
        assert_eq!(HookEvent::from_claude_name("OnBoot"), None);
        assert_eq!(HookEvent::from_claude_name("pre_tool_use"), None);
    }

    #[test]
    fn stdout_is_context_only_on_the_four_claude_events() {
        let ctx: Vec<_> = HookEvent::ALL
            .into_iter()
            .filter(|e| e.stdout_is_context())
            .collect();
        assert_eq!(ctx.len(), 4);
    }
}
