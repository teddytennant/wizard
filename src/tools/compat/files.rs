//! `Read` and `Edit` under Claude Code's argument names.
//!
//! Not plain aliases: `Read` renders a notebook as cells, and `Edit` checks
//! its input first and words the refusals the way Claude Code does, because
//! those strings are what a prompt that has seen them once will look for.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{invalid, notebook, str_arg};
use crate::tools::registry::ToolRegistry;
use crate::tools::{Tool, ToolAccess, ToolContext, ToolError, ToolOutput, resolve_path};

/// The tools this module contributes, for whichever inner tools exist.
pub fn tools(registry: &ToolRegistry) -> Vec<Arc<dyn Tool>> {
    let mut out: Vec<Arc<dyn Tool>> = Vec::new();
    if let Some(inner) = registry.get("read_file") {
        out.push(Arc::new(ReadTool {
            inner: Arc::clone(inner),
        }));
    }
    if let Some(inner) = registry.get("edit_file") {
        out.push(Arc::new(EditTool {
            inner: Arc::clone(inner),
            write: registry.get("write_file").map(Arc::clone),
        }));
    }
    out
}

/// `Read`: `read_file` with `offset`/`limit`, and notebooks as cells.
struct ReadTool {
    inner: Arc<dyn Tool>,
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "Read"
    }

    fn description(&self) -> &str {
        "Read a file. offset is the 1-based line to start at and limit the number of lines. \
         A .ipynb file comes back as its cells, each tagged with the id NotebookEdit takes. \
         Images come back as images."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "The path of the file to read" },
                "offset": { "type": "integer", "minimum": 0, "description": "The line number to start reading from. Only provide if the file is too large to read at once" },
                "limit": { "type": "integer", "exclusiveMinimum": 0, "description": "The number of lines to read. Only provide if the file is too large to read at once." },
                "pages": { "type": "string", "description": "Accepted and ignored; Wizard does not read PDFs." },
                "allow_large": { "type": "boolean", "description": "Accepted and ignored." }
            },
            "required": ["file_path"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let Some(file_path) = str_arg(&args, "file_path") else {
            return Ok(invalid(self.name(), "file_path is required"));
        };
        let path = resolve_path(ctx, file_path);
        if path.is_dir() {
            return Ok(ToolOutput::error(format!(
                "EISDIR: illegal operation on a directory, read '{}'",
                path.display()
            )));
        }
        if !path.exists() {
            return Ok(ToolOutput::error(format!(
                "File does not exist. Note: your current working directory is {}.",
                ctx.cwd.display()
            )));
        }
        if path.extension().and_then(|e| e.to_str()) == Some("ipynb") {
            return Ok(notebook::read_notebook(&path).await);
        }
        let offset = args
            .get("offset")
            .and_then(Value::as_u64)
            .filter(|n| *n > 0);
        let limit = args.get("limit").and_then(Value::as_u64);
        let mut mapped = json!({ "path": file_path });
        if let Some(start) = offset {
            mapped["start_line"] = json!(start);
        }
        if let Some(limit) = limit {
            let start = offset.unwrap_or(1);
            mapped["end_line"] = json!(start + limit.max(1) - 1);
        }
        self.inner.execute(mapped, ctx).await
    }
}

/// `Edit`: `edit_file` checked the way Claude Code checks it.
struct EditTool {
    inner: Arc<dyn Tool>,
    write: Option<Arc<dyn Tool>>,
}

/// What is wrong with an edit before anything is written, in Claude Code's
/// words, or `None` when it can go ahead.
fn refusal(old: &str, new: &str, content: &str, replace_all: bool) -> Option<String> {
    if old == new {
        return Some("No changes to make: old_string and new_string are exactly the same.".into());
    }
    match content.matches(old).count() {
        0 => Some(format!(
            "String to replace not found in file.\nString: {old}"
        )),
        1 => None,
        _ if replace_all => None,
        n => Some(format!(
            "Found {n} matches of the string to replace, but replace_all is false. To replace \
             all occurrences, set replace_all to true. To replace only one occurrence, please \
             provide more context to uniquely identify the instance.\nString: {old}"
        )),
    }
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "Edit"
    }

    fn description(&self) -> &str {
        "Replace old_string with new_string in a file. old_string must match exactly once, \
         or pass replace_all to change every occurrence. An empty old_string creates a file \
         that does not exist yet."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "The path of the file to modify" },
                "old_string": { "type": "string", "description": "The text to replace" },
                "new_string": { "type": "string", "description": "The text to replace it with (must be different from old_string)" },
                "replace_all": { "type": "boolean", "default": false, "description": "Replace all occurrences of old_string (default false)" }
            },
            "required": ["file_path", "old_string", "new_string"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::Edit
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let (Some(file_path), Some(old), Some(new)) = (
            str_arg(&args, "file_path"),
            str_arg(&args, "old_string"),
            str_arg(&args, "new_string"),
        ) else {
            return Ok(invalid(
                self.name(),
                "file_path, old_string and new_string are required",
            ));
        };
        let replace_all = args
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let path = resolve_path(ctx, file_path);

        if old.is_empty() {
            let blank = match tokio::fs::read_to_string(&path).await {
                Ok(content) => content.is_empty(),
                Err(_) => true,
            };
            return match (&self.write, blank) {
                (Some(write), true) => {
                    write
                        .execute(json!({ "path": file_path, "content": new }), ctx)
                        .await
                }
                _ => Ok(invalid(
                    self.name(),
                    "Cannot create new file - file already exists.",
                )),
            };
        }

        let content = match tokio::fs::read_to_string(&path).await {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ToolOutput::error(format!(
                    "File does not exist. Note: your current working directory is {}.",
                    ctx.cwd.display()
                )));
            }
            Err(err) => {
                return Ok(ToolOutput::error(format!(
                    "failed to read {}: {err}",
                    path.display()
                )));
            }
        };
        if let Some(message) = refusal(old, new, &content, replace_all) {
            return Ok(invalid(self.name(), message));
        }
        self.inner
            .execute(
                json!({
                    "path": file_path,
                    "old_string": old,
                    "new_string": new,
                    "replace_all": replace_all,
                }),
                ctx,
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::registry::ToolRegistry;

    fn setup() -> (tempfile::TempDir, ToolContext, Vec<Arc<dyn Tool>>) {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        let registry = ToolRegistry::with_native_tools();
        (dir, ctx, tools(&registry))
    }

    fn named<'a>(tools: &'a [Arc<dyn Tool>], name: &str) -> &'a Arc<dyn Tool> {
        tools.iter().find(|t| t.name() == name).unwrap()
    }

    #[tokio::test]
    async fn edit_refuses_an_ambiguous_match_in_claude_codes_words() {
        let (dir, ctx, tools) = setup();
        std::fs::write(dir.path().join("a.txt"), "alpha\nbeta\nalpha\n").unwrap();
        let edit = named(&tools, "Edit");

        let out = edit
            .execute(
                json!({ "file_path": "a.txt", "old_string": "alpha", "new_string": "gamma" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(
            out.content.starts_with(
                "Tool Edit input is invalid: Found 2 matches of the string to replace, but \
                 replace_all is false."
            ),
            "{}",
            out.content
        );
        assert!(out.content.ends_with("String: alpha"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "alpha\nbeta\nalpha\n"
        );

        let all = edit
            .execute(
                json!({ "file_path": "a.txt", "old_string": "alpha", "new_string": "gamma",
                        "replace_all": true }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!all.is_error, "{}", all.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "gamma\nbeta\ngamma\n"
        );
    }

    #[tokio::test]
    async fn edit_says_not_found_and_same_string_like_claude_code() {
        let (dir, ctx, tools) = setup();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        let edit = named(&tools, "Edit");
        let missing = edit
            .execute(
                json!({ "file_path": "a.txt", "old_string": "zzz", "new_string": "y" }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(
            missing.content,
            "Tool Edit input is invalid: String to replace not found in file.\nString: zzz"
        );
        let same = edit
            .execute(
                json!({ "file_path": "a.txt", "old_string": "alpha", "new_string": "alpha" }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(
            same.content,
            "Tool Edit input is invalid: No changes to make: old_string and new_string are \
             exactly the same."
        );
    }

    #[tokio::test]
    async fn edit_with_an_empty_old_string_creates_a_file() {
        let (dir, ctx, tools) = setup();
        let edit = named(&tools, "Edit");
        let out = edit
            .execute(
                json!({ "file_path": "new/b.txt", "old_string": "", "new_string": "hi\n" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("new/b.txt")).unwrap(),
            "hi\n"
        );
        let again = edit
            .execute(
                json!({ "file_path": "new/b.txt", "old_string": "", "new_string": "x" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(again.is_error);
    }

    #[tokio::test]
    async fn read_takes_offset_and_limit_and_names_missing_files_like_claude_code() {
        let (dir, ctx, tools) = setup();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        let read = named(&tools, "Read");
        let out = read
            .execute(
                json!({ "file_path": "a.txt", "offset": 2, "limit": 2 }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out.content, "two\nthree");

        let missing = read
            .execute(json!({ "file_path": "nope.txt" }), &ctx)
            .await
            .unwrap();
        assert!(missing.is_error);
        assert!(
            missing
                .content
                .starts_with("File does not exist. Note: your current working directory is ")
        );
        let directory = read
            .execute(json!({ "file_path": "." }), &ctx)
            .await
            .unwrap();
        assert!(
            directory
                .content
                .starts_with("EISDIR: illegal operation on a directory")
        );
    }

    #[tokio::test]
    async fn read_renders_a_notebook_as_cells() {
        let (dir, ctx, tools) = setup();
        let nb = json!({
            "cells": [{ "cell_type": "code", "id": "c1", "source": ["a = 1\n", "a"],
                        "outputs": [], "metadata": {}, "execution_count": null }],
            "metadata": {}, "nbformat": 4, "nbformat_minor": 5
        });
        std::fs::write(dir.path().join("n.ipynb"), nb.to_string()).unwrap();
        let out = named(&tools, "Read")
            .execute(json!({ "file_path": "n.ipynb" }), &ctx)
            .await
            .unwrap();
        assert_eq!(out.content, "<cell id=\"c1\">a = 1\na</cell id=\"c1\">");
    }
}
