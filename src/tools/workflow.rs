//! `Workflow`: Claude Code's tool for running a JavaScript script that fans
//! subagents out. The input schema is Claude's; the runtime is
//! [`crate::workflow`]. Registered only when the `plugin-js` feature is on,
//! because the script runs in the QuickJS kernel.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::registry::ToolRegistry;
use super::{CommandDispatch, Tool, ToolContext, ToolError, ToolOutput};
use crate::agent::subagent::{SharedActiveModel, SubagentConfig};
use crate::hooks::HookEngine;
use crate::llm::provider::LlmProvider;
use crate::workflow::engine::Limits;
use crate::workflow::launch::{self, Deps, Input};
use crate::workflow::runner::SubagentRunner;
use crate::workflow::store::Status;

pub const NAME: &str = "Workflow";

const DESCRIPTION: &str = "Run a JavaScript workflow script that orchestrates many subagents in the background, with `agent()`, `parallel()`, `pipeline()`, `phase()` and `log()`.\n\n\
ONLY call this tool when the user has explicitly opted into multi-agent orchestration. Workflows can spawn dozens of agents and use a lot of tokens; the user must ask for that scale, not have it inferred. Opt-in means: the user asked for a workflow or fan-out in their own words (\"use a workflow\", \"fan out agents\", \"orchestrate this with subagents\"), included the keyword \"ultracode\", invoked a command whose instructions tell you to call Workflow, or asked for a specific saved workflow. For any other task, even one that would benefit from parallelism, use spawn_subagent for individual subagents, or say what a workflow could do and roughly what it would cost, and ask.\n\n\
Every script must begin with `export const meta = {...}`, a PURE LITERAL (no variables, calls or interpolation) giving `name`, a one-line `description` and optionally `phases`, one `{ title, detail? }` per phase() call. Pass the script inline via `script`; do not also set `name`, which selects a saved workflow. It is plain JavaScript, not TypeScript. Before writing a script, load the `workflow-authoring` skill (the Skill tool, or /workflow-authoring) for the full API, resume and patterns.\n\n\
The canonical multi-stage pattern, pipeline by default so each dimension verifies as soon as its review completes:\n\
  export const meta = { name: 'review-changes', description: 'Review changed files across dimensions, verify each finding', phases: [{ title: 'Review' }, { title: 'Verify' }] }\n\
  const results = await pipeline(DIMENSIONS,\n\
    d => agent(d.prompt, {label: `review:${d.key}`, phase: 'Review', schema: FINDINGS}),\n\
    review => parallel(review.findings.map(f => () => agent(`Adversarially verify: ${f.title}`, {phase: 'Verify', schema: VERDICT}).then(v => ({...f, verdict: v})))))\n\
  return results.flat().filter(Boolean).filter(f => f.verdict?.isReal)\n\n\
The run goes on in the background as a task; you are notified when it ends, with the result. TaskStop stops it. To resume after a stop or an edit, pass `resumeFromRunId` with the same `scriptPath`.";

/// The `Workflow` tool, built with what it needs to start subagents.
pub struct WorkflowTool {
    configs: Vec<SubagentConfig>,
    client: Arc<dyn LlmProvider>,
    registry: Arc<ToolRegistry>,
    hooks: Arc<HookEngine>,
    binding: SharedActiveModel,
    description: String,
}

/// The size guideline Claude Code tells the model about, from the
/// `workflowSizeGuideline` setting. `unrestricted` sends none; unset is
/// `medium`.
pub fn size_guideline(setting: Option<&str>) -> Option<String> {
    let (name, limit) = match setting {
        Some("unrestricted") => return None,
        Some("small") => ("small", 5),
        Some("large") => ("large", 50),
        Some("medium") => ("medium", 10),
        _ => ("medium", 10),
    };
    let lead = if setting.is_some() {
        "A workflow size guideline is configured for this session"
    } else {
        "This session has the default workflow size guideline"
    };
    let change = if setting.is_some() {
        ""
    } else {
        " Set workflowSizeGuideline in settings to raise or remove it."
    };
    Some(format!(
        "{lead}: {name}, keep workflows under {limit} agents. This is a guideline, not a hard limit; follow it unless the user's prompt calls for a different scale.{change}"
    ))
}

impl WorkflowTool {
    pub fn new(
        configs: Vec<SubagentConfig>,
        client: Arc<dyn LlmProvider>,
        registry: Arc<ToolRegistry>,
        hooks: Arc<HookEngine>,
        binding: SharedActiveModel,
    ) -> Self {
        Self {
            configs,
            client,
            registry,
            hooks,
            binding,
            description: DESCRIPTION.to_string(),
        }
    }

    /// Add the size guideline to what the model reads about the tool.
    pub fn with_size_guideline(mut self, guideline: Option<String>) -> Self {
        if let Some(text) = guideline {
            self.description = format!("{DESCRIPTION}\n\n{text}");
        }
        self
    }
}

/// Whether the environment turns workflows off.
pub fn disabled_by_env(get: &dyn Fn(&str) -> Option<String>) -> bool {
    get("CLAUDE_CODE_DISABLE_WORKFLOWS").is_some_and(|v| {
        !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        )
    })
}

const DISABLED: &str = "Dynamic workflows are disabled for this session (the environment variable CLAUDE_CODE_DISABLE_WORKFLOWS is set).";

#[async_trait]
impl Tool for WorkflowTool {
    fn name(&self) -> &str {
        NAME
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "script": {
                    "type": "string",
                    "description": "Self-contained workflow script. Must begin with `export const meta = { name, description, phases }` (pure literal, no computed values) followed by the script body using agent()/parallel()/pipeline()/phase()."
                },
                "name": {
                    "type": "string",
                    "description": "Name of a predefined workflow (from .wizard/workflows/ or .claude/workflows/). Resolves to a self-contained script."
                },
                "description": {
                    "type": "string",
                    "description": "Ignored. Set the workflow description in the script's `meta` block."
                },
                "title": {
                    "type": "string",
                    "description": "Ignored. Set the workflow title in the script's `meta` block."
                },
                "args": {
                    "description": "Optional input value exposed to the script as the global `args`, verbatim. Pass arrays/objects as actual JSON values, NOT as a JSON-encoded string; a stringified list breaks `args.filter`/`args.map` in the script. Use for parameterized named workflows (e.g. a research question)."
                },
                "scriptPath": {
                    "type": "string",
                    "description": "Path to a workflow script file on disk. Every Workflow invocation persists its script and returns the path. To iterate, edit that file with Write/Edit and re-invoke Workflow with the same `scriptPath` instead of re-sending the full script. Takes precedence over `script` and `name`."
                },
                "resumeFromRunId": {
                    "type": "string",
                    "description": "Run ID of a prior Workflow invocation to resume from. Completed agent() calls with unchanged (prompt, opts) return their cached results instantly; only edited or new calls re-run. Stop the prior run first (TaskStop) before resuming."
                }
            }
        })
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        if disabled_by_env(&|k| std::env::var(k).ok()) {
            return Ok(ToolOutput::error(DISABLED));
        }
        let text = |key: &str| {
            args.get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let input = Input {
            script: text("script"),
            name: text("name"),
            script_path: text("scriptPath"),
            args: args.get("args").filter(|v| !v.is_null()).cloned(),
            resume_from_run_id: text("resumeFromRunId"),
        };
        let home = dirs::home_dir();
        let resolved = match launch::resolve(&input, &ctx.cwd, home.as_deref()) {
            Ok(resolved) => resolved,
            Err(message) => return Ok(ToolOutput::error(message)),
        };
        if let Err(message) = launch::validate(&resolved).await {
            return Ok(ToolOutput::error(message));
        }

        let worktrees = crate::config::Config::wizard_dir()
            .map(|dir| dir.join("workflows").join("worktrees"))
            .unwrap_or_else(|_| std::env::temp_dir().join("wizard-workflow-worktrees"));
        let runner = Arc::new(SubagentRunner {
            configs: self.configs.clone(),
            client: Arc::clone(&self.client),
            registry: Arc::clone(&self.registry),
            hooks: Arc::clone(&self.hooks),
            binding: Arc::clone(&self.binding),
            ctx: ctx.clone(),
            worktrees,
        });
        let deps = Deps {
            runner,
            ctx: ctx.clone(),
            limits: Limits::from_env(&|k| std::env::var(k).ok()),
        };
        let launched = match launch::launch(deps, &input, resolved).await {
            Ok(launched) => launched,
            Err(message) => return Ok(ToolOutput::error(message)),
        };

        // Where nothing can be told later (`-p`, the gateway, ACP), wait and
        // hand back the result in this call, bounded by the turn's interrupt.
        if ctx.command_dispatch == CommandDispatch::None {
            let mut done = launched.done;
            let outcome = tokio::select! {
                outcome = &mut done => outcome,
                () = crate::agent::cancelled(ctx.cancel.as_ref()) => {
                    launched.cancel.cancel();
                    done.await
                }
            };
            return Ok(match outcome {
                Ok(outcome) if outcome.status == Status::Completed => {
                    ToolOutput::ok(format!("Run ID: {}{}", launched.run_id, outcome.summary))
                }
                Ok(outcome) => {
                    ToolOutput::error(format!("Run ID: {}{}", launched.run_id, outcome.summary))
                }
                Err(err) => ToolOutput::error(format!("workflow run crashed: {err}")),
            });
        }

        let script_path = launched.script_path.display();
        Ok(ToolOutput::ok(format!(
            "Workflow launched in background. Task ID: {task}\nSummary: {summary}\nTranscript dir: {dir}\nScript file: {script_path}\n(Edit this file with Write/Edit and re-invoke Workflow with {{scriptPath: \"{script_path}\"}} to iterate without resending the script.)\nRun ID: {run}\nTo resume after editing the script: Workflow({{scriptPath: \"{script_path}\", resumeFromRunId: \"{run}\"}}); completed agents return cached results (cached results may themselves be empty, so inspect journal.jsonl before assuming there is something to recover).\nYou will be notified when it completes. Use /workflows to watch live progress.",
            task = launched.task_id,
            summary = launched.description,
            dir = launched.dir.display(),
            run = launched.run_id,
        )))
    }
}

#[cfg(test)]
#[path = "workflow_tests.rs"]
mod tests;
