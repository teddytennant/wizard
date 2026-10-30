//! `LSP`, and `EnterPlanMode`.
//!
//! `LSP` is a stub. Wizard has no language-server client: running one means
//! spawning the right server per language, waiting for it to index, and keeping
//! it alive between calls, and a one-shot server per call answers `null` for
//! most requests on a project of any size (rust-analyzer needs seconds to
//! index before it knows anything). The tool takes Claude Code's arguments and
//! says plainly that no server is wired, so a prompt that reaches for it falls
//! back to `Grep` and `Read` instead of waiting for an answer that cannot come.
//!
//! `EnterPlanMode` flips the same flag `/plan` does; the turn loop re-composes
//! the system prompt before the next completion.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{invalid, str_arg};
use crate::tools::{Tool, ToolAccess, ToolContext, ToolError, ToolOutput, resolve_path};

pub fn tools(plan_mode: Arc<AtomicBool>) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(LspTool), Arc::new(EnterPlanModeTool { plan_mode })]
}

const OPERATIONS: &[&str] = &[
    "goToDefinition",
    "findReferences",
    "hover",
    "documentSymbol",
    "workspaceSymbol",
    "goToImplementation",
    "prepareCallHierarchy",
    "incomingCalls",
    "outgoingCalls",
];

/// `LSP`.
pub struct LspTool;

#[async_trait]
impl Tool for LspTool {
    fn name(&self) -> &str {
        "LSP"
    }

    fn description(&self) -> &str {
        "Code intelligence by language server: definitions, references, hover, symbols, call \
         hierarchy. Wizard has no language-server client yet, so this always reports that; use \
         Grep for references and Read for definitions."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "operation": { "type": "string", "enum": OPERATIONS, "description": "The LSP operation to perform" },
                "filePath": { "type": "string", "description": "The file to operate on" },
                "line": { "type": "integer", "minimum": 1, "description": "Line number (1-based, as shown in editors)" },
                "character": { "type": "integer", "minimum": 1, "description": "Character offset (1-based, as shown in editors)" }
            },
            "required": ["operation", "filePath", "line", "character"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let Some(operation) = str_arg(&args, "operation") else {
            return Ok(invalid(self.name(), "operation is required"));
        };
        if !OPERATIONS.contains(&operation) {
            return Ok(invalid(
                self.name(),
                format!("operation must be one of {}", OPERATIONS.join(", ")),
            ));
        }
        let Some(file) = str_arg(&args, "filePath") else {
            return Ok(invalid(self.name(), "filePath is required"));
        };
        let path = resolve_path(ctx, file);
        if !path.exists() {
            return Ok(ToolOutput::error(format!(
                "File does not exist: {}",
                path.display()
            )));
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{e}"))
            .unwrap_or_default();
        Ok(ToolOutput::error(format!(
            "No language server is configured for {ext} files: Wizard has no LSP client, so {operation} \
             cannot be answered. Fall back to Grep (references, symbol names) and Read (definitions)."
        )))
    }
}

/// `EnterPlanMode`.
pub struct EnterPlanModeTool {
    plan_mode: Arc<AtomicBool>,
}

#[async_trait]
impl Tool for EnterPlanModeTool {
    fn name(&self) -> &str {
        "EnterPlanMode"
    }

    fn description(&self) -> &str {
        "Switch to plan mode for a task that needs design before code: only read-only tools run \
         until you present the plan with ExitPlanMode and it is approved."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    /// Read-only so the plan-mode gate lets it through when already planning.
    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        if self.plan_mode.swap(true, Ordering::SeqCst) {
            return Ok(ToolOutput::ok(
                "Already in plan mode. Finish with ExitPlanMode when the plan is ready.",
            ));
        }
        Ok(ToolOutput::ok(
            "Entered plan mode. Explore with read-only tools, then present the plan with \
             ExitPlanMode. Nothing is edited or run until it is approved.",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lsp_says_it_has_no_server_and_names_the_extension() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn main() {}").unwrap();
        let ctx = ToolContext::new(dir.path());
        let out = LspTool
            .execute(
                json!({ "operation": "hover", "filePath": "a.rs", "line": 1, "character": 1 }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(
            out.content
                .starts_with("No language server is configured for .rs files"),
            "{}",
            out.content
        );
        let bad = LspTool
            .execute(json!({ "operation": "rename", "filePath": "a.rs" }), &ctx)
            .await
            .unwrap();
        assert!(bad.content.contains("operation must be one of"));
        let missing = LspTool
            .execute(
                json!({ "operation": "hover", "filePath": "nope.rs", "line": 1, "character": 1 }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(missing.content.starts_with("File does not exist"));
    }

    #[tokio::test]
    async fn enter_plan_mode_sets_the_shared_flag_once() {
        let flag = Arc::new(AtomicBool::new(false));
        let tool = EnterPlanModeTool {
            plan_mode: Arc::clone(&flag),
        };
        let ctx = ToolContext::new(std::env::temp_dir());
        let first = tool.execute(json!({}), &ctx).await.unwrap();
        assert!(first.content.starts_with("Entered plan mode."));
        assert!(flag.load(Ordering::SeqCst));
        let second = tool.execute(json!({}), &ctx).await.unwrap();
        assert!(second.content.starts_with("Already in plan mode."));
    }
}
