//! Tool system: the [`Tool`] trait implemented by native tools
//! ([`file`], [`shell`], [`git`]), agent-authored [`scripted`] tools, and
//! MCP tools (`crate::mcp`). All three present a uniform interface through
//! [`registry::ToolRegistry`], so the model calls them identically.

pub mod evolve;
pub mod file;
pub mod git;
pub mod registry;
pub mod scripted;
pub mod shell;

use std::path::PathBuf;

use async_trait::async_trait;

use crate::llm::ToolSpec;

/// Where a tool comes from. Affects display and default approval policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    /// Compiled into the binary.
    Native,
    /// Agent-authored script under `~/.wizard/tools/`.
    Scripted,
    /// Served by an external MCP server.
    Mcp,
}

/// Per-call execution context.
#[derive(Debug, Clone)]
pub struct ToolContext {
    /// Project root all relative paths resolve against.
    pub cwd: PathBuf,
}

impl ToolContext {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }
}

/// Result of a tool execution, fed back to the model as a `role: tool`
/// message.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    /// Text returned to the model (stdout, file contents, diff, ...).
    pub content: String,
    /// True when the tool ran but reported failure (non-zero exit, missing
    /// file, ...). Distinct from [`ToolError`], which means the call itself
    /// could not be carried out.
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

/// Failures in dispatching or running a tool call.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    #[error("invalid arguments for '{tool}': {message}")]
    InvalidArgs { tool: String, message: String },
    #[error("user denied execution of '{0}'")]
    Denied(String),
    #[error("tool '{tool}' timed out after {seconds}s")]
    Timeout { tool: String, seconds: u64 },
    #[error("tool '{tool}' failed")]
    Execution {
        tool: String,
        #[source]
        source: anyhow::Error,
    },
}

/// Byte cap applied to tool output returned to the model. Keeps a single
/// tool result from flooding the context window.
pub(crate) const MAX_OUTPUT_BYTES: usize = 30_000;

/// Resolve a model-supplied path against the project root, expanding a
/// leading `~`. Absolute paths are used as-is.
pub(crate) fn resolve_path(ctx: &ToolContext, path: &str) -> PathBuf {
    let expanded = shellexpand::tilde(path);
    let candidate = std::path::Path::new(expanded.as_ref());
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        ctx.cwd.join(candidate)
    }
}

/// Deserialize tool arguments, mapping shape mismatches to
/// [`ToolError::InvalidArgs`]. `null` is treated as an empty object so models
/// may omit arguments for zero-parameter tools.
pub(crate) fn parse_args<T: serde::de::DeserializeOwned>(
    tool: &str,
    args: serde_json::Value,
) -> Result<T, ToolError> {
    let args = if args.is_null() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        args
    };
    serde_json::from_value(args).map_err(|err| ToolError::InvalidArgs {
        tool: tool.to_string(),
        message: err.to_string(),
    })
}

/// Truncate `text` to at most `max_bytes` (cutting on a char boundary),
/// appending a marker when content was dropped.
pub(crate) fn truncate_output(mut text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    let mut cut = max_bytes;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.truncate(cut);
    text.push_str("\n... [output truncated]");
    text
}

/// A callable capability exposed to the model.
///
/// Contract:
/// - `name` is unique within the registry (MCP tools are namespaced
///   `server__tool` on collision).
/// - `parameters` returns a JSON Schema object; `execute` receives arguments
///   already validated against nothing — implementations must deserialize
///   defensively and return [`ToolError::InvalidArgs`] on shape mismatch.
/// - `requires_approval` tools are gated behind user confirmation in genie
///   mode (sovereign mode auto-approves).
#[async_trait]
pub trait Tool: Send + Sync {
    /// Unique tool name as advertised to the model (snake_case).
    fn name(&self) -> &str;

    /// One-paragraph description shown to the model.
    fn description(&self) -> &str;

    /// JSON Schema describing the arguments object.
    fn parameters(&self) -> serde_json::Value;

    /// Whether genie mode must confirm before running this tool.
    fn requires_approval(&self) -> bool {
        false
    }

    /// Origin of this tool.
    fn kind(&self) -> ToolKind {
        ToolKind::Native
    }

    /// Wire-format spec sent to Ollama in the request `tools` array.
    fn spec(&self) -> ToolSpec {
        ToolSpec::function(self.name(), self.description(), self.parameters())
    }

    /// Run the tool with `args` (a JSON object) in `ctx`.
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, ToolError>;
}
