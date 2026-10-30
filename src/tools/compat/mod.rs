//! Claude Code's tool names and input schemas, so a prompt, a skill or a habit
//! written for Claude Code finds the tool it names.
//!
//! Wizard's own tools keep their names and stay the lean defaults. Everything
//! here is registered on top of them, advertised by name only until
//! `tool_search` loads it ([`Tool::deferred_by_default`]), so the default token
//! cost is a few words of tool names in the system prompt rather than a schema
//! each. The `stock` profile defers nothing, so it gets none of these.
//!
//! Three kinds of tool live here:
//!
//! - **Aliases** ([`alias`], [`files`]): `Read`, `Write`, `Edit`, `Bash`,
//!   `WebFetch`, `WebSearch`, `Agent`, `TodoWrite`, `AskUserQuestion`,
//!   `ExitPlanMode`, `ToolSearch`. Claude's argument names in, Wizard's tool
//!   underneath, Claude's error wording where a prompt might key on it.
//! - **New behaviour**: `Glob`, `Grep`, `NotebookEdit`, the cron and wakeup
//!   tools, `Monitor`, the worktree pair, the task board and the rest.
//! - **Honest gaps**: tools whose backend Wizard does not have report that, in
//!   the result, rather than being left out of the roster.
//!
//! A name an MCP server or a scripted tool already took is left alone: first
//! registration wins everywhere else in the registry, and this is no different.

mod agents;
pub mod alias;
pub mod board;
mod cron;
mod delivery;
mod files;
mod lsp;
mod monitor;
mod notebook;
#[cfg(test)]
mod probe;
mod search;
mod session;
mod skill;
pub mod worktree;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use serde_json::Value;

use super::registry::ToolRegistry;
use super::{Tool, ToolOutput};

/// What a failed Claude Code `validateInput` looks like to the model.
///
/// Claude Code prefixes these with the tool name, and a skill that has seen
/// the text once may match on it, so the Claude-named tools use the same
/// frame.
pub(crate) fn invalid(tool: &str, message: impl std::fmt::Display) -> ToolOutput {
    ToolOutput::error(format!("Tool {tool} input is invalid: {message}"))
}

/// Pull `key` out of an argument object as a string, if it is one.
pub(crate) fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

/// Register every Claude Code name that applies to this session.
///
/// Does nothing under the `stock` token profile, which defers no tool and so
/// would pay a full schema for each of these on every request.
pub fn install(registry: &mut ToolRegistry, plan_mode: Arc<AtomicBool>) {
    if !crate::token_profile::current().safe() {
        return;
    }
    install_all(registry, plan_mode);
}

/// [`install`] without the profile check (tests).
pub fn install_all(registry: &mut ToolRegistry, plan_mode: Arc<AtomicBool>) {
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    tools.extend(alias::aliases(registry));
    tools.extend(files::tools(registry));
    tools.extend(search::tools());
    tools.extend(notebook::tools());
    tools.extend(cron::tools());
    tools.extend(monitor::tools());
    tools.extend(delivery::tools());
    tools.extend(skill::tools());
    tools.extend(board::tools());
    tools.extend(agents::tools());
    tools.extend(worktree::tools());
    tools.extend(lsp::tools(plan_mode));
    tools.extend(session::tools(registry));
    for tool in tools {
        if registry.get(tool.name()).is_none() {
            registry.register(tool);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{ToolAccess, ToolContext, ToolError};
    use async_trait::async_trait;

    /// Stands in for a Wizard tool the aliases wrap.
    struct Inner(&'static str);

    #[async_trait]
    impl Tool for Inner {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "inner"
        }
        fn parameters(&self) -> Value {
            serde_json::json!({ "type": "object", "properties": {} })
        }
        fn access(&self) -> ToolAccess {
            ToolAccess::Execute
        }
        async fn execute(&self, _: Value, _: &ToolContext) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::ok("inner"))
        }
    }

    fn full_registry() -> ToolRegistry {
        let mut registry = ToolRegistry::with_native_tools();
        for name in [
            "web_fetch",
            "web_search",
            "spawn_subagent",
            "exit_plan",
            "interview",
        ] {
            registry.register(Arc::new(Inner(name)));
        }
        install_all(&mut registry, Arc::new(AtomicBool::new(false)));
        registry
    }

    /// The input schemas of Claude Code 2.1.314, read from its own
    /// `claude mcp serve` `tools/list`: tool, every property, the required
    /// ones. Ours must accept every property a call may carry (more is fine,
    /// fewer is a rejected call) and require nothing the real one does not.
    const CLAUDE_CODE_2_1_314: &[(&str, &[&str], &[&str])] = &[
        (
            "Agent",
            &[
                "description",
                "effort",
                "isolation",
                "mode",
                "model",
                "name",
                "prompt",
                "subagent_type",
                "team_name",
            ],
            &["description", "prompt"],
        ),
        (
            "Bash",
            &[
                "command",
                "dangerouslyDisableSandbox",
                "description",
                "run_in_background",
                "timeout",
            ],
            &["command"],
        ),
        (
            "Read",
            &["allow_large", "file_path", "limit", "offset", "pages"],
            &["file_path"],
        ),
        (
            "Edit",
            &["file_path", "new_string", "old_string", "replace_all"],
            &["file_path", "new_string", "old_string"],
        ),
        (
            "Write",
            &["content", "file_path"],
            &["content", "file_path"],
        ),
        (
            "NotebookEdit",
            &[
                "cell_id",
                "cell_type",
                "edit_mode",
                "new_source",
                "notebook_path",
            ],
            &["new_source", "notebook_path"],
        ),
        ("WebFetch", &["offset", "prompt", "url"], &["prompt", "url"]),
        ("ReportFindings", &["findings", "level"], &["findings"]),
        (
            "WebSearch",
            &["allowed_domains", "blocked_domains", "query"],
            &["query"],
        ),
        ("TaskStop", &["shell_id", "task_id"], &[]),
        ("Skill", &["args", "skill"], &["skill"]),
        ("EnterWorktree", &["name", "path"], &[]),
        ("ExitWorktree", &["action", "discard_changes"], &["action"]),
        (
            "SendMessage",
            &["message", "notify_when_idle", "summary", "to"],
            &["message", "to"],
        ),
        ("ListAgents", &["channel", "q"], &[]),
        (
            "CronCreate",
            &["cron", "durable", "prompt", "recurring"],
            &["cron", "prompt"],
        ),
        ("CronDelete", &["id"], &["id"]),
        ("CronList", &[], &[]),
        (
            "ScheduleWakeup",
            &["delaySeconds", "noop", "prompt", "reason", "stop"],
            &[],
        ),
        (
            "Monitor",
            &["command", "description", "timeout_ms", "ws"],
            &["description", "timeout_ms"],
        ),
    ];

    #[test]
    fn schemas_accept_everything_claude_code_2_1_314_accepts() {
        let registry = full_registry();
        for (name, props, required) in CLAUDE_CODE_2_1_314 {
            let tool = registry
                .get(name)
                .unwrap_or_else(|| panic!("{name} is not registered"));
            let schema = tool.parameters();
            let ours: Vec<&str> = schema["properties"]
                .as_object()
                .map(|m| m.keys().map(String::as_str).collect())
                .unwrap_or_default();
            for prop in *props {
                assert!(
                    ours.contains(prop),
                    "{name} lacks the property {prop}: {ours:?}"
                );
            }
            let ours_required: Vec<&str> = schema["required"]
                .as_array()
                .map(|r| r.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            for req in &ours_required {
                assert!(
                    required.contains(req),
                    "{name} requires {req}, which Claude Code does not"
                );
            }
        }
    }

    #[test]
    fn every_claude_name_is_deferred_and_a_taken_name_is_left_alone() {
        let registry = full_registry();
        for (name, _, _) in CLAUDE_CODE_2_1_314 {
            assert!(
                registry.get(name).unwrap().deferred_by_default(),
                "{name} must be deferred under the default profile"
            );
        }
        // A scripted or MCP tool that took the name first keeps it.
        let mut registry = ToolRegistry::with_native_tools();
        registry.register(Arc::new(Inner("Grep")));
        install_all(&mut registry, Arc::new(AtomicBool::new(false)));
        assert!(!registry.get("Grep").unwrap().deferred_by_default());
    }

    #[test]
    fn the_default_profile_advertises_none_of_them_in_full() {
        let mut registry = full_registry();
        registry.set_deferral(Arc::new(crate::tools::registry::Deferral::mcp_only()));
        let advertised: Vec<String> = registry
            .advertised_specs()
            .into_iter()
            .map(|s| s.function.name)
            .collect();
        for name in [
            "Read",
            "Grep",
            "Glob",
            "NotebookEdit",
            "CronCreate",
            "Monitor",
        ] {
            assert!(
                !advertised.contains(&name.to_string()),
                "{name} leaked: {advertised:?}"
            );
        }
        assert!(advertised.contains(&"read_file".to_string()));
        assert!(registry.is_deferred("Grep"));
    }

    #[test]
    fn invalid_uses_the_claude_code_frame() {
        let out = invalid("Edit", "String to replace not found in file.");
        assert!(out.is_error);
        assert_eq!(
            out.content,
            "Tool Edit input is invalid: String to replace not found in file."
        );
    }
}
