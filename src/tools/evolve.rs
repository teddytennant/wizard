//! `evolve` tool: lets the agent extend or rebuild ITSELF at runtime.
//! Wraps the tiered self-extension pipeline (`crate::evolve`). In sovereign
//! mode this is auto-approved; in genie mode it is gated behind confirmation
//! (`requires_approval` = true). A successful deep rebuild drops a re-exec
//! marker so the continuous loop relaunches into the new binary.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Tool, ToolContext, ToolError, ToolOutput, parse_args};
use crate::config::Config;
use crate::evolve::{EvolveOutcome, EvolveRequest, EvolveTier, Evolver};

/// `evolve` — add a new capability to Wizard itself, or deep-rebuild its
/// binary.
pub struct EvolveTool {
    config: Config,
}

impl EvolveTool {
    pub fn new(config: Config) -> Self {
        Self { config }
    }
}

/// Arguments for [`EvolveTool`].
#[derive(Debug, Deserialize)]
pub struct EvolveArgs {
    /// Precise natural-language spec of the capability to add.
    pub description: String,
    /// When true, change Wizard's own Rust source and rebuild the binary
    /// (Tier 2). Defaults to a fast runtime extension (Tier 1).
    #[serde(default)]
    pub deep: bool,
}

#[async_trait]
impl Tool for EvolveTool {
    fn name(&self) -> &str {
        "evolve"
    }

    fn description(&self) -> &str {
        "Add a NEW capability to yourself when the current task needs one you \
         lack. By default (deep=false) this performs a fast runtime extension — \
         it adds a skill, MCP server, scripted tool, or subagent under \
         ~/.wizard/ with no recompile. Set deep=true ONLY when the capability \
         genuinely requires changing Wizard's own Rust source: this rebuilds and \
         replaces the running binary, is much slower, and is gated by a build \
         plus smoke test (falling back to a runtime extension if no toolchain or \
         source is available). The `description` argument is a precise \
         natural-language specification of the capability you want."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "Precise natural-language spec of the capability to add to yourself."
                },
                "deep": {
                    "type": "boolean",
                    "default": false,
                    "description": "Change Wizard's own Rust source and rebuild the binary (slow). Default false uses a fast runtime extension."
                }
            },
            "required": ["description"]
        })
    }

    fn requires_approval(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: EvolveArgs = parse_args(self.name(), args)?;

        let request = EvolveRequest {
            description: args.description,
            tier: if args.deep {
                EvolveTier::Deep
            } else {
                EvolveTier::Runtime
            },
            // The Tool-layer/genie approval gate already governs whether this
            // `execute` is reached, so the pipeline itself need not re-prompt.
            auto_approve: true,
        };

        let outcome = match Evolver::new(self.config.clone()).run(request).await {
            Ok(outcome) => outcome,
            Err(err) => return Ok(ToolOutput::error(format!("evolve failed: {err:#}"))),
        };

        let summary = match outcome {
            EvolveOutcome::DeepRebuilt { binary } => {
                let marker_note = write_marker(ctx, "evolve-reexec");
                format!(
                    "Deep evolve rebuilt Wizard's binary at {}. {marker_note}",
                    binary.display()
                )
            }
            EvolveOutcome::SkillAdded { name, path } => {
                let marker_note = write_marker(ctx, "evolve-reload");
                format!("Added skill '{name}' at {}. {marker_note}", path.display())
            }
            EvolveOutcome::McpServerRegistered { name } => {
                let marker_note = write_marker(ctx, "evolve-reload");
                format!("Registered MCP server '{name}'. {marker_note}")
            }
            EvolveOutcome::ScriptedToolAdded { name, path } => {
                let marker_note = write_marker(ctx, "evolve-reload");
                format!(
                    "Added scripted tool '{name}' at {}. {marker_note}",
                    path.display()
                )
            }
            EvolveOutcome::SubagentAdded { name } => {
                let marker_note = write_marker(ctx, "evolve-reload");
                format!("Added subagent '{name}'. {marker_note}")
            }
            EvolveOutcome::FellBackToRuntime { reason, outcome } => {
                let marker_note = write_marker(ctx, "evolve-reload");
                format!(
                    "Deep evolve fell back to a runtime extension ({reason}): {}. {marker_note}",
                    describe_outcome(&outcome)
                )
            }
            EvolveOutcome::Denied => {
                return Ok(ToolOutput::error("evolve was denied"));
            }
        };

        Ok(ToolOutput::ok(summary))
    }
}

/// Drop an empty marker file under `<cwd>/.wizard/` so the supervising loop
/// knows to react (relaunch on `evolve-reexec`, hot-reload the registry on
/// `evolve-reload`). Returns a note describing success or failure rather than
/// propagating the error.
fn write_marker(ctx: &ToolContext, name: &str) -> String {
    let dir = ctx.cwd.join(".wizard");
    if let Err(err) = std::fs::create_dir_all(&dir) {
        return format!("(could not create {} marker: {err})", dir.display());
    }
    let marker = dir.join(name);
    match std::fs::write(&marker, b"") {
        Ok(()) => format!("Wrote {} marker for the loop.", marker.display()),
        Err(err) => format!("(could not write {} marker: {err})", marker.display()),
    }
}

/// One-line description of a nested Tier-1 outcome (used for fallbacks).
fn describe_outcome(outcome: &EvolveOutcome) -> String {
    match outcome {
        EvolveOutcome::SkillAdded { name, .. } => format!("added skill '{name}'"),
        EvolveOutcome::McpServerRegistered { name } => format!("registered MCP server '{name}'"),
        EvolveOutcome::ScriptedToolAdded { name, .. } => format!("added scripted tool '{name}'"),
        EvolveOutcome::SubagentAdded { name } => format!("added subagent '{name}'"),
        EvolveOutcome::DeepRebuilt { binary } => format!("rebuilt binary at {}", binary.display()),
        EvolveOutcome::FellBackToRuntime { reason, outcome } => {
            format!("fell back ({reason}): {}", describe_outcome(outcome))
        }
        EvolveOutcome::Denied => "denied".to_string(),
    }
}
