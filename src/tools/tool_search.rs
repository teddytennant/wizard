//! `tool_search`: load the schema of a deferred tool.
//!
//! Under the `lean` and `min` token profiles most tools go out by name only
//! (see [`crate::tools::registry::Deferral`]). This tool returns the full
//! schema of the ones asked for and marks them loaded, so from the next
//! request on they are advertised like any core tool.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::llm::ToolSpec;
use crate::tools::patch::ApplyPatchTool;
use crate::tools::registry::Deferral;
use crate::tools::{Tool, ToolAccess, ToolContext, ToolError, ToolOutput, parse_args};

pub const TOOL_SEARCH_TOOL_NAME: &str = "tool_search";

/// How many tools a keyword query loads at most.
const DEFAULT_MAX_RESULTS: usize = 5;

pub struct ToolSearchTool {
    deferral: Arc<Deferral>,
}

impl ToolSearchTool {
    pub fn new(deferral: Arc<Deferral>) -> Self {
        Self { deferral }
    }
}

#[derive(Deserialize)]
struct ToolSearchArgs {
    query: String,
    #[serde(default)]
    max_results: Option<usize>,
}

/// Rank `catalog` against a keyword query: a name hit counts more than a
/// description hit. Tools that match nothing are dropped.
fn rank<'a>(catalog: &'a [ToolSpec], query: &str) -> Vec<&'a ToolSpec> {
    let terms: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| !t.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    let mut scored: Vec<(usize, &ToolSpec)> = catalog
        .iter()
        .filter_map(|spec| {
            let name = spec.function.name.to_ascii_lowercase();
            let description = spec.function.description.to_ascii_lowercase();
            let score: usize = terms
                .iter()
                .map(|t| {
                    if name == *t {
                        10
                    } else if name.contains(t.as_str()) {
                        4
                    } else if description.contains(t.as_str()) {
                        1
                    } else {
                        0
                    }
                })
                .sum();
            (score > 0).then_some((score, spec))
        })
        .collect();
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    scored.into_iter().map(|(_, spec)| spec).collect()
}

#[async_trait]
impl Tool for ToolSearchTool {
    fn name(&self) -> &str {
        TOOL_SEARCH_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Load deferred tools. `select:a,b` loads those by name; other text searches names and descriptions. Returns their schemas; they are callable from your next call."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "select:name1,name2 or keywords" },
                "max_results": { "type": "integer", "description": "Keyword matches to load (default 5)" }
            },
            "required": ["query"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: ToolSearchArgs = parse_args(self.name(), args)?;
        let catalog = self.deferral.catalog();
        let query = args.query.trim();
        let (found, missing): (Vec<&ToolSpec>, Vec<String>) =
            if let Some(names) = query.strip_prefix("select:") {
                let mut found = Vec::new();
                let mut missing = Vec::new();
                for name in names.split(',').map(str::trim).filter(|n| !n.is_empty()) {
                    match catalog.iter().find(|spec| spec.function.name == name) {
                        Some(spec) => found.push(spec),
                        None if self.deferral.is_available(name) => {}
                        None => missing.push(name.to_string()),
                    }
                }
                (found, missing)
            } else {
                let limit = args.max_results.unwrap_or(DEFAULT_MAX_RESULTS).max(1);
                (
                    rank(&catalog, query).into_iter().take(limit).collect(),
                    Vec::new(),
                )
            };

        if found.is_empty() {
            let mut names: Vec<&str> = catalog.iter().map(|s| s.function.name.as_str()).collect();
            names.sort_unstable();
            return Ok(ToolOutput::error(format!(
                "no deferred tool matches {query:?}. Deferred tools: {}",
                names.join(", ")
            )));
        }
        let mut out = String::new();
        for spec in &found {
            self.deferral.load(&spec.function.name);
            out.push_str(&serde_json::to_string(&spec.function).unwrap_or_default());
            out.push('\n');
        }
        if !missing.is_empty() {
            out.push_str(&format!("not found: {}\n", missing.join(", ")));
        }
        Ok(ToolOutput::ok(out.trim_end().to_string()))
    }
}

/// Core tools under the `lean` profile: everything the session files show
/// being called in more than a few percent of calls.
pub const LEAN_CORE: &[&str] = &[
    "execute",
    "read_file",
    "edit_file",
    "apply_patch",
    "write_file",
    "search_files",
    "todo",
    "spawn_subagent",
    TOOL_SEARCH_TOOL_NAME,
];

/// Core tools under the `min` profile.
pub const MIN_CORE: &[&str] = &[
    "execute",
    "read_file",
    "edit_file",
    "apply_patch",
    "write_file",
    TOOL_SEARCH_TOOL_NAME,
];

/// Short specs for the `min` profile's core tools. Same argument names as
/// the real tools, most of the prose dropped.
///
/// A boolean that defaults to false is named in the description and left out
/// of the schema. Listed there, GPT-6 sent `"replace_all":false` on every
/// `edit_file` and `"run_in_background":false` on nine `execute` calls in ten.
pub fn terse_core_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec::function(
            "execute",
            "Run a shell command in the project root (sh -c, pipefail). Returns stdout, stderr and exit code. A command still running after 30s becomes a background task; read it with task_output. run_in_background:true starts it as one.",
            json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "timeout_secs": { "type": "integer", "description": "only to wait longer than 30, max 600" }
                },
                "required": ["command"]
            }),
        ),
        ToolSpec::function(
            "read_file",
            "Read a file's exact text, optionally a 1-based line range.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "start_line": { "type": "integer" },
                    "end_line": { "type": "integer" }
                },
                "required": ["path"]
            }),
        ),
        ToolSpec::function(
            "edit_file",
            "Replace old_string with new_string. old_string must match exactly once, or pass replace_all:true.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_string": { "type": "string" },
                    "new_string": { "type": "string" }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        ),
        ApplyPatchTool.spec(),
        ToolSpec::function(
            "write_file",
            "Create or overwrite a file.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
        ),
    ]
}
