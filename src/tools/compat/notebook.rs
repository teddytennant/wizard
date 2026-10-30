//! `NotebookEdit`, and the notebook rendering `Read` uses for `.ipynb` files.
//!
//! A notebook is JSON, so `edit_file` can change one, but only by matching
//! escaped text inside a string. This edits by cell: replace, insert after a
//! cell, or delete, addressed by the cell's `id` (or `cell-N` for a notebook
//! old enough to have no ids). Written back with Jupyter's one-space indent and
//! trailing newline, keeping the file's key order, so a diff of the notebook
//! shows the cell and little else.

use std::path::Path;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{invalid, str_arg};
use crate::tools::{
    Tool, ToolAccess, ToolContext, ToolError, ToolOutput, parse_args, resolve_path,
};

/// How many characters of a cell's text output `Read` keeps before pointing
/// at `jq` instead.
const LARGE_OUTPUT: usize = 10_000;

/// The tools this module contributes.
pub fn tools() -> Vec<std::sync::Arc<dyn Tool>> {
    vec![std::sync::Arc::new(NotebookEditTool)]
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum EditMode {
    Replace,
    Insert,
    Delete,
}

impl EditMode {
    fn parse(mode: Option<&str>) -> Result<Self, String> {
        match mode {
            None | Some("replace") => Ok(Self::Replace),
            Some("insert") => Ok(Self::Insert),
            Some("delete") => Ok(Self::Delete),
            Some(_) => Err("Edit mode must be replace, insert, or delete.".to_string()),
        }
    }
}

/// What an edit did, for the result line.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Applied {
    pub cell_id: Option<String>,
}

fn cells_mut(notebook: &mut Value) -> Result<&mut Vec<Value>, String> {
    notebook
        .get_mut("cells")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "Notebook is not valid JSON.".to_string())
}

/// Index of the cell called `cell_id`: its `id`, else `cell-N` as an index.
fn find_cell(cells: &[Value], cell_id: &str) -> Result<usize, String> {
    if let Some(index) = cells
        .iter()
        .position(|cell| str_arg(cell, "id") == Some(cell_id))
    {
        return Ok(index);
    }
    match cell_id
        .strip_prefix("cell-")
        .and_then(|n| n.parse::<usize>().ok())
    {
        Some(index) if index < cells.len() => Ok(index),
        Some(index) => Err(format!(
            "Cell with index {index} does not exist in notebook."
        )),
        None => Err(format!("Cell with ID \"{cell_id}\" not found in notebook.")),
    }
}

/// Whether new cells carry an `id`: nbformat 4.5 and later.
fn has_cell_ids(notebook: &Value) -> bool {
    let major = notebook
        .get("nbformat")
        .and_then(Value::as_u64)
        .unwrap_or(4);
    let minor = notebook
        .get("nbformat_minor")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    major > 4 || (major == 4 && minor >= 5)
}

/// Apply one edit to a parsed notebook.
pub(super) fn apply_edit(
    notebook: &mut Value,
    cell_id: Option<&str>,
    new_source: &str,
    cell_type: Option<&str>,
    mode: EditMode,
    new_id: impl FnOnce() -> String,
) -> Result<Applied, String> {
    if mode == EditMode::Insert && cell_type.is_none() {
        return Err("Cell type is required when using edit_mode=insert.".to_string());
    }
    let with_ids = has_cell_ids(notebook);
    let cells = cells_mut(notebook)?;
    let Some(cell_id) = cell_id else {
        if mode != EditMode::Insert {
            return Err("Cell ID must be specified when not inserting a new cell.".to_string());
        }
        let cell = new_cell(cell_type, new_source, with_ids.then(new_id));
        let id = str_arg(&cell, "id").map(str::to_string);
        cells.insert(0, cell);
        return Ok(Applied { cell_id: id });
    };
    let index = find_cell(cells, cell_id)?;
    match mode {
        EditMode::Insert => {
            let cell = new_cell(cell_type, new_source, with_ids.then(new_id));
            let id = str_arg(&cell, "id").map(str::to_string);
            cells.insert(index + 1, cell);
            Ok(Applied { cell_id: id })
        }
        EditMode::Delete => {
            cells.remove(index);
            Ok(Applied {
                cell_id: Some(cell_id.to_string()),
            })
        }
        EditMode::Replace => {
            let cell = &mut cells[index];
            cell["source"] = json!(new_source);
            if str_arg(cell, "cell_type") == Some("code") {
                cell["execution_count"] = Value::Null;
                cell["outputs"] = json!([]);
            }
            if let Some(kind) = cell_type {
                cell["cell_type"] = json!(kind);
            }
            Ok(Applied {
                cell_id: Some(cell_id.to_string()),
            })
        }
    }
}

fn new_cell(cell_type: Option<&str>, source: &str, id: Option<String>) -> Value {
    let mut cell = if cell_type == Some("markdown") {
        json!({ "cell_type": "markdown", "source": source, "metadata": {} })
    } else {
        json!({
            "cell_type": "code",
            "source": source,
            "metadata": {},
            "execution_count": null,
            "outputs": [],
        })
    };
    if let Some(id) = id {
        cell["id"] = json!(id);
    }
    cell
}

/// A cell id in the shape Claude Code mints: eight hex digits.
fn fresh_cell_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

/// Serialize the way nbformat does: one-space indent, trailing newline. Key
/// order is whatever the parsed map holds.
pub(super) fn to_notebook_json(notebook: &Value) -> String {
    use serde::Serialize;
    let mut buf = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut serializer = serde_json::Serializer::with_formatter(&mut buf, formatter);
    if notebook.serialize(&mut serializer).is_err() {
        return String::new();
    }
    buf.push(b'\n');
    String::from_utf8(buf).unwrap_or_default()
}

/// Cell source as one string, whether the file stored a string or lines.
fn source_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(lines)) => lines.iter().filter_map(Value::as_str).collect(),
        _ => String::new(),
    }
}

fn output_text(output: &Value) -> String {
    let join = |value: Option<&Value>| source_text(value);
    match str_arg(output, "output_type") {
        Some("stream") => join(output.get("text")),
        Some("execute_result" | "display_data") => {
            join(output.get("data").and_then(|data| data.get("text/plain")))
        }
        Some("error") => format!(
            "{}: {}\n{}",
            str_arg(output, "ename").unwrap_or_default(),
            str_arg(output, "evalue").unwrap_or_default(),
            output
                .get("traceback")
                .and_then(Value::as_array)
                .map(|lines| lines
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("\n"))
                .unwrap_or_default()
        ),
        _ => String::new(),
    }
}

/// Render a notebook for the model: each cell as `<cell id="..">source</cell
/// id="..">`, markdown cells tagged with their type, a code cell's text outputs
/// below it. Image outputs are not carried. `path` is only for the `jq` hint on
/// an output too big to show.
pub(super) fn render(notebook: &Value, path: &Path) -> String {
    let language = notebook
        .get("metadata")
        .and_then(|m| m.get("language_info"))
        .and_then(|l| str_arg(l, "name"))
        .unwrap_or("python");
    let mut parts: Vec<String> = Vec::new();
    for (index, cell) in notebook
        .get("cells")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let id = str_arg(cell, "id")
            .map(str::to_string)
            .unwrap_or_else(|| format!("cell-{index}"));
        let kind = str_arg(cell, "cell_type").unwrap_or("code");
        let mut meta = String::new();
        if kind != "code" {
            meta.push_str(&format!("<cell_type>{kind}</cell_type>"));
        } else if language != "python" {
            meta.push_str(&format!("<language>{language}</language>"));
        }
        parts.push(format!(
            "<cell id=\"{id}\">{meta}{}</cell id=\"{id}\">",
            source_text(cell.get("source"))
        ));
        if kind != "code" {
            continue;
        }
        let outputs: Vec<String> = cell
            .get("outputs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(output_text)
            .filter(|text| !text.is_empty())
            .collect();
        if outputs.iter().map(String::len).sum::<usize>() > LARGE_OUTPUT {
            parts.push(format!(
                "\nOutputs are too large to include. Use execute with: cat {} | jq '.cells[{index}].outputs'",
                path.display()
            ));
        } else {
            parts.extend(outputs.into_iter().map(|text| format!("\n{text}")));
        }
    }
    parts.join("\n")
}

/// Read a notebook file for `Read`.
pub(super) async fn read_notebook(path: &Path) -> ToolOutput {
    let raw = match tokio::fs::read_to_string(path).await {
        Ok(raw) => raw,
        Err(err) => return ToolOutput::error(format!("failed to read {}: {err}", path.display())),
    };
    match serde_json::from_str::<Value>(&raw) {
        Ok(notebook) => ToolOutput::ok(render(&notebook, path)),
        Err(_) => ToolOutput::error("Notebook is not valid JSON."),
    }
}

/// `NotebookEdit`.
pub struct NotebookEditTool;

#[async_trait]
impl Tool for NotebookEditTool {
    fn name(&self) -> &str {
        "NotebookEdit"
    }

    fn description(&self) -> &str {
        "Replace, insert or delete one cell of a Jupyter notebook (.ipynb). \
         cell_id is the id shown by Read (or cell-N, the cell's index). replace and delete \
         need it; insert puts the new cell after it, or first when it is omitted, and needs \
         cell_type. Replacing a code cell clears its outputs."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "notebook_path": { "type": "string", "description": "The path of the notebook to edit" },
                "cell_id": { "type": "string", "description": "The id of the cell to edit. When inserting, the new cell goes after this one, or first when omitted." },
                "new_source": { "type": "string", "description": "The new source for the cell" },
                "cell_type": { "type": "string", "enum": ["code", "markdown"], "description": "The type of the cell. Required for insert; otherwise defaults to the current type." },
                "edit_mode": { "type": "string", "enum": ["replace", "insert", "delete"], "description": "Defaults to replace." }
            },
            "required": ["notebook_path", "new_source"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::Edit
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let path_arg = str_arg(&args, "notebook_path")
            .ok_or_else(|| ToolError::InvalidArgs {
                tool: self.name().to_string(),
                message: "notebook_path is required".to_string(),
            })?
            .to_string();
        #[derive(serde::Deserialize)]
        struct Args {
            new_source: String,
            #[serde(default)]
            cell_id: Option<String>,
            #[serde(default)]
            cell_type: Option<String>,
            #[serde(default)]
            edit_mode: Option<String>,
        }
        let Args {
            new_source,
            cell_id,
            cell_type,
            edit_mode,
        } = parse_args(self.name(), args)?;

        let path = resolve_path(ctx, &path_arg);
        if path.extension().and_then(|e| e.to_str()) != Some("ipynb") {
            return Ok(invalid(
                self.name(),
                "File must be a Jupyter notebook (.ipynb file). For editing other file types, use the Edit tool.",
            ));
        }
        let mode = match EditMode::parse(edit_mode.as_deref()) {
            Ok(mode) => mode,
            Err(message) => return Ok(invalid(self.name(), message)),
        };
        let raw = match tokio::fs::read_to_string(&path).await {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(invalid(self.name(), "Notebook file does not exist."));
            }
            Err(err) => {
                return Ok(ToolOutput::error(format!(
                    "failed to read {}: {err}",
                    path.display()
                )));
            }
        };
        let Ok(mut notebook) = serde_json::from_str::<Value>(&raw) else {
            return Ok(invalid(self.name(), "Notebook is not valid JSON."));
        };
        let applied = match apply_edit(
            &mut notebook,
            cell_id.as_deref(),
            &new_source,
            cell_type.as_deref(),
            mode,
            fresh_cell_id,
        ) {
            Ok(applied) => applied,
            Err(message) => return Ok(invalid(self.name(), message)),
        };
        if let Err(err) = tokio::fs::write(&path, to_notebook_json(&notebook)).await {
            return Ok(ToolOutput::error(format!(
                "failed to write {}: {err}",
                path.display()
            )));
        }
        let id = applied.cell_id.unwrap_or_default();
        Ok(ToolOutput::ok(match mode {
            EditMode::Replace => format!("Updated cell {id} with {new_source}"),
            EditMode::Insert => format!("Inserted cell {id} with {new_source}"),
            EditMode::Delete => format!("Deleted cell {id}"),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Value {
        json!({
            "cells": [
                { "cell_type": "code", "id": "c1", "metadata": {}, "source": "print(1)",
                  "outputs": [{ "output_type": "stream", "name": "stdout", "text": "1\n" }],
                  "execution_count": 1 },
                { "cell_type": "markdown", "id": "c2", "metadata": {}, "source": "# hi" }
            ],
            "metadata": { "language_info": { "name": "python" } },
            "nbformat": 4,
            "nbformat_minor": 5
        })
    }

    #[test]
    fn replace_clears_outputs_and_the_execution_count() {
        let mut nb = sample();
        apply_edit(
            &mut nb,
            Some("c1"),
            "print(2)",
            None,
            EditMode::Replace,
            fresh_cell_id,
        )
        .unwrap();
        let cell = &nb["cells"][0];
        assert_eq!(cell["source"], "print(2)");
        assert_eq!(cell["outputs"], json!([]));
        assert!(cell["execution_count"].is_null());
    }

    #[test]
    fn insert_goes_after_the_named_cell_with_a_new_id() {
        let mut nb = sample();
        let applied = apply_edit(
            &mut nb,
            Some("c1"),
            "x",
            Some("markdown"),
            EditMode::Insert,
            || "abcd1234".to_string(),
        )
        .unwrap();
        assert_eq!(applied.cell_id.as_deref(), Some("abcd1234"));
        assert_eq!(nb["cells"][1]["id"], "abcd1234");
        assert_eq!(nb["cells"][1]["cell_type"], "markdown");
        assert_eq!(nb["cells"][2]["id"], "c2");
    }

    #[test]
    fn insert_without_a_cell_id_goes_first() {
        let mut nb = sample();
        apply_edit(
            &mut nb,
            None,
            "x",
            Some("code"),
            EditMode::Insert,
            fresh_cell_id,
        )
        .unwrap();
        assert_eq!(nb["cells"][0]["source"], "x");
        assert_eq!(nb["cells"][0]["outputs"], json!([]));
        assert_eq!(nb["cells"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn an_old_notebook_gets_cells_without_ids() {
        let mut nb = sample();
        nb["nbformat_minor"] = json!(2);
        apply_edit(
            &mut nb,
            None,
            "x",
            Some("code"),
            EditMode::Insert,
            fresh_cell_id,
        )
        .unwrap();
        assert!(nb["cells"][0].get("id").is_none());
    }

    #[test]
    fn delete_removes_the_cell() {
        let mut nb = sample();
        apply_edit(
            &mut nb,
            Some("c2"),
            "",
            None,
            EditMode::Delete,
            fresh_cell_id,
        )
        .unwrap();
        assert_eq!(nb["cells"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn the_errors_read_like_claude_codes() {
        let mut nb = sample();
        let err = |nb: &mut Value, id, mode, kind| {
            apply_edit(nb, id, "x", kind, mode, fresh_cell_id).unwrap_err()
        };
        assert_eq!(
            err(&mut nb, Some("c1"), EditMode::Insert, None),
            "Cell type is required when using edit_mode=insert."
        );
        assert_eq!(
            err(&mut nb, None, EditMode::Replace, None),
            "Cell ID must be specified when not inserting a new cell."
        );
        assert_eq!(
            err(&mut nb, Some("nope"), EditMode::Replace, None),
            "Cell with ID \"nope\" not found in notebook."
        );
        assert_eq!(
            err(&mut nb, Some("cell-9"), EditMode::Replace, None),
            "Cell with index 9 does not exist in notebook."
        );
        assert_eq!(
            EditMode::parse(Some("swap")).unwrap_err(),
            "Edit mode must be replace, insert, or delete."
        );
    }

    #[test]
    fn a_cell_without_an_id_is_addressed_by_index() {
        let mut nb = sample();
        nb["cells"][1].as_object_mut().unwrap().remove("id");
        apply_edit(
            &mut nb,
            Some("cell-1"),
            "new",
            None,
            EditMode::Replace,
            fresh_cell_id,
        )
        .unwrap();
        assert_eq!(nb["cells"][1]["source"], "new");
    }

    #[test]
    fn written_the_way_jupyter_writes_it() {
        let text = to_notebook_json(&json!({ "a": [1] }));
        assert_eq!(text, "{\n \"a\": [\n  1\n ]\n}\n");
    }

    #[test]
    fn read_renders_cells_tagged_like_claude_code() {
        let text = render(&sample(), Path::new("n.ipynb"));
        assert_eq!(
            text,
            "<cell id=\"c1\">print(1)</cell id=\"c1\">\n\n1\n\n<cell id=\"c2\"><cell_type>markdown</cell_type># hi</cell id=\"c2\">"
        );
    }

    #[tokio::test]
    async fn the_tool_edits_a_file_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("n.ipynb");
        std::fs::write(&path, serde_json::to_string(&sample()).unwrap()).unwrap();
        let ctx = ToolContext::new(dir.path());

        let out = NotebookEditTool
            .execute(
                json!({ "notebook_path": "n.ipynb", "cell_id": "c1", "new_source": "print(2)" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(out.content, "Updated cell c1 with print(2)");
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.ends_with("}\n"));
        let nb: Value = serde_json::from_str(&written).unwrap();
        assert_eq!(nb["cells"][0]["source"], "print(2)");

        let wrong = NotebookEditTool
            .execute(json!({ "notebook_path": "n.txt", "new_source": "x" }), &ctx)
            .await
            .unwrap();
        assert!(wrong.is_error);
        assert!(
            wrong
                .content
                .starts_with("Tool NotebookEdit input is invalid: File must be")
        );
    }
}
