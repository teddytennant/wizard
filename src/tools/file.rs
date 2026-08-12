//! Native file tools: `read_file`, `write_file`, `edit_file`, `list_files`,
//! `search_files`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use globset::{Glob, GlobMatcher};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;

use super::shell::{render_command_result, run_command};
use super::{
    MAX_ERROR_BYTES, MAX_LISTING_BYTES, MAX_OUTPUT_BYTES, MAX_SEARCH_BYTES, Tool, ToolAccess,
    ToolContext, ToolError, ToolOutput, parse_args, resolve_path, truncate_output,
};

/// Maximum number of lines a single `read_file` call returns.
const MAX_READ_LINES: usize = 2_000;

/// Maximum number of entries `list_files` returns.
const MAX_LIST_ENTRIES: usize = 500;

/// Cap on directory entries visited during a manual `list_files` walk, so a
/// glob over a huge tree cannot spin forever.
const MAX_WALK_VISITS: usize = 100_000;

/// Timeout for the external search process (`rg`/`grep`).
const SEARCH_TIMEOUT: Duration = Duration::from_secs(60);

/// Arguments for [`ReadFileTool`].
#[derive(Debug, Deserialize)]
pub struct ReadFileArgs {
    /// Path to read, relative to the project root or absolute.
    pub path: String,
    /// 1-based first line to include (default: start of file).
    #[serde(default)]
    pub start_line: Option<usize>,
    /// 1-based last line to include (default: end of file).
    #[serde(default)]
    pub end_line: Option<usize>,
}

/// `read_file` — read file contents with optional line range.
pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read the contents of a file, optionally limited to a 1-based line range."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path (relative to project root or absolute)" },
                "start_line": { "type": "integer", "description": "1-based first line to include" },
                "end_line": { "type": "integer", "description": "1-based last line to include" }
            },
            "required": ["path"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: ReadFileArgs = parse_args(self.name(), args)?;
        if matches!(args.start_line, Some(0)) || matches!(args.end_line, Some(0)) {
            return Err(ToolError::InvalidArgs {
                tool: self.name().to_string(),
                message: "start_line and end_line are 1-based; 0 is not a valid line".to_string(),
            });
        }
        if let (Some(start), Some(end)) = (args.start_line, args.end_line)
            && end < start
        {
            return Err(ToolError::InvalidArgs {
                tool: self.name().to_string(),
                message: format!("end_line ({end}) is before start_line ({start})"),
            });
        }

        let path = resolve_path(ctx, &args.path);
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(content) => content,
            Err(err) => {
                return Ok(ToolOutput::error(format!(
                    "failed to read {}: {err}",
                    path.display()
                )));
            }
        };

        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        if total == 0 {
            return Ok(ToolOutput::ok("(empty file)"));
        }

        let start = args.start_line.unwrap_or(1);
        let end = args.end_line.unwrap_or(total).min(total);
        if start > total {
            return Ok(ToolOutput::error(format!(
                "start_line {start} is past the end of {} ({total} lines)",
                path.display()
            )));
        }

        let slice = &lines[start - 1..end];
        let (shown, line_capped) = if slice.len() > MAX_READ_LINES {
            (&slice[..MAX_READ_LINES], true)
        } else {
            (slice, false)
        };

        let mut numbered: String = shown
            .iter()
            .enumerate()
            .map(|(offset, line)| format!("{:>6}\t{}", start + offset, line))
            .collect::<Vec<_>>()
            .join("\n");
        if line_capped {
            numbered.push_str(&format!(
                "\n... [showing {MAX_READ_LINES} of {} requested lines; total {total} lines — use start_line/end_line to read more]",
                slice.len()
            ));
        }
        Ok(ToolOutput::ok(truncate_output(numbered, MAX_OUTPUT_BYTES)))
    }
}

/// Arguments for [`WriteFileTool`].
#[derive(Debug, Deserialize)]
pub struct WriteFileArgs {
    pub path: String,
    /// Full contents to write (creates or overwrites; parents created).
    pub content: String,
}

/// `write_file` — create or overwrite a file.
pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        r#"Create or overwrite a file with the given content, creating parent directories as needed.

Tips:
- Use for new files or full rewrites; prefer `edit_file` for surgical changes.
- Write required deliverables as soon as you know the path and a schema-valid payload — do not defer the only required output to a narration-only final turn.
- When a verification script already found the answer, write the file in that same step.
- For JSONL/CWE reports, copy the demonstration schema exactly (key names, types; `cwe_id` is a **list** of lowercase `cwe-N` strings). Only use IDs from the task's candidate list.
- When multiple answers/IDs/moves are required, write them all."#
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path to create or overwrite" },
                "content": { "type": "string", "description": "Full file contents" }
            },
            "required": ["path", "content"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::Edit
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: WriteFileArgs = parse_args(self.name(), args)?;
        let path = resolve_path(ctx, &args.path);

        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && let Err(err) = tokio::fs::create_dir_all(parent).await
        {
            return Ok(ToolOutput::error(format!(
                "failed to create parent directory {}: {err}",
                parent.display()
            )));
        }

        let existed = path.exists();
        if let Err(err) = tokio::fs::write(&path, &args.content).await {
            return Ok(ToolOutput::error(format!(
                "failed to write {}: {err}",
                path.display()
            )));
        }

        let verb = if existed { "Overwrote" } else { "Created" };
        Ok(ToolOutput::ok(format!(
            "{verb} {} ({} bytes)",
            path.display(),
            args.content.len()
        )))
    }
}

/// Arguments for [`EditFileTool`].
#[derive(Debug, Deserialize)]
pub struct EditFileArgs {
    pub path: String,
    /// Exact text to find. Must match exactly once unless `replace_all`.
    pub old_string: String,
    /// Replacement text.
    pub new_string: String,
    /// Replace every occurrence instead of requiring a unique match.
    #[serde(default)]
    pub replace_all: bool,
}

/// `edit_file` — exact search-and-replace edit.
pub struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Edit a file by exact search-and-replace. old_string must match exactly once unless replace_all is true."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File to edit" },
                "old_string": { "type": "string", "description": "Exact text to replace" },
                "new_string": { "type": "string", "description": "Replacement text" },
                "replace_all": { "type": "boolean", "description": "Replace all occurrences (default false)" }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::Edit
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: EditFileArgs = parse_args(self.name(), args)?;
        if args.old_string.is_empty() {
            return Err(ToolError::InvalidArgs {
                tool: self.name().to_string(),
                message: "old_string must not be empty".to_string(),
            });
        }
        if args.old_string == args.new_string {
            return Err(ToolError::InvalidArgs {
                tool: self.name().to_string(),
                message: "old_string and new_string are identical".to_string(),
            });
        }

        let path = resolve_path(ctx, &args.path);
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(content) => content,
            Err(err) => {
                return Ok(ToolOutput::error(format!(
                    "failed to read {}: {err}",
                    path.display()
                )));
            }
        };

        let count = content.matches(&args.old_string).count();
        if count == 0 {
            return Ok(ToolOutput::error(format!(
                "old_string not found in {}",
                path.display()
            )));
        }
        if count > 1 && !args.replace_all {
            return Ok(ToolOutput::error(format!(
                "old_string matches {count} times in {}; provide more surrounding context to make it unique, or set replace_all",
                path.display()
            )));
        }

        // Line of the first match, for the confirmation message.
        let first_line = content
            .find(&args.old_string)
            .map(|idx| content[..idx].matches('\n').count() + 1)
            .unwrap_or(1);

        let updated = if args.replace_all {
            content.replace(&args.old_string, &args.new_string)
        } else {
            content.replacen(&args.old_string, &args.new_string, 1)
        };

        if let Err(err) = tokio::fs::write(&path, &updated).await {
            return Ok(ToolOutput::error(format!(
                "failed to write {}: {err}",
                path.display()
            )));
        }

        let message = if count == 1 {
            format!(
                "Edited {}: replaced 1 occurrence (line {first_line})",
                path.display()
            )
        } else {
            format!(
                "Edited {}: replaced {count} occurrences (first at line {first_line})",
                path.display()
            )
        };
        Ok(ToolOutput::ok(message))
    }
}

/// Arguments for [`ListFilesTool`].
#[derive(Debug, Deserialize)]
pub struct ListFilesArgs {
    /// Directory to list (default: project root).
    #[serde(default)]
    pub path: Option<String>,
    /// Glob filter, e.g. `**/*.rs` (default: all entries).
    #[serde(default)]
    pub glob: Option<String>,
}

/// `list_files` — directory listing with optional glob filter.
pub struct ListFilesTool;

#[async_trait]
impl Tool for ListFilesTool {
    fn name(&self) -> &str {
        "list_files"
    }

    fn description(&self) -> &str {
        "List files and directories under a path, optionally filtered by a glob pattern. Respects .gitignore."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory to list (default: project root)" },
                "glob": { "type": "string", "description": "Glob filter, e.g. **/*.rs" }
            }
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: ListFilesArgs = parse_args(self.name(), args)?;
        let dir = resolve_path(ctx, args.path.as_deref().unwrap_or("."));
        if !dir.is_dir() {
            return Ok(ToolOutput::error(format!(
                "{} is not a directory",
                dir.display()
            )));
        }

        match args.glob {
            None => list_single_level(self.name(), &dir),
            Some(glob) => {
                let matcher = Glob::new(&glob)
                    .map_err(|err| ToolError::InvalidArgs {
                        tool: self.name().to_string(),
                        message: format!("invalid glob '{glob}': {err}"),
                    })?
                    .compile_matcher();
                list_recursive(self.name(), &dir, &glob, &matcher).await
            }
        }
    }
}

/// Plain single-level listing (no glob): directories first with a trailing
/// `/`, then files, both alphabetical.
fn list_single_level(tool: &str, dir: &Path) -> Result<ToolOutput, ToolError> {
    let entries = std::fs::read_dir(dir).map_err(|err| ToolError::Execution {
        tool: tool.to_string(),
        source: anyhow::Error::new(err).context(format!("reading {}", dir.display())),
    })?;

    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        if is_dir {
            dirs.push(format!("{name}/"));
        } else {
            files.push(name);
        }
    }
    dirs.sort();
    files.sort();

    let mut listing: Vec<String> = dirs;
    listing.extend(files);
    if listing.is_empty() {
        return Ok(ToolOutput::ok("(empty directory)"));
    }

    let truncated = listing.len() > MAX_LIST_ENTRIES;
    listing.truncate(MAX_LIST_ENTRIES);
    let mut content = listing.join("\n");
    if truncated {
        content.push_str(&format!(
            "\n... [listing truncated at {MAX_LIST_ENTRIES} entries]"
        ));
    }
    Ok(ToolOutput::ok(truncate_output(content, MAX_LISTING_BYTES)))
}

/// Recursive glob listing. Prefers `git ls-files` (which respects
/// `.gitignore`); falls back to a manual walk that skips `.git` when the
/// directory is not inside a git repository.
async fn list_recursive(
    tool: &str,
    dir: &Path,
    glob: &str,
    matcher: &GlobMatcher,
) -> Result<ToolOutput, ToolError> {
    let mut matches = match git_tracked_files(tool, dir).await {
        Some(files) => files
            .into_iter()
            .filter(|file| matcher.is_match(Path::new(file)))
            .collect(),
        None => walk_matching(dir, matcher),
    };
    matches.sort();

    if matches.is_empty() {
        return Ok(ToolOutput::ok(format!(
            "No files matching '{glob}' under {}",
            dir.display()
        )));
    }

    let truncated = matches.len() > MAX_LIST_ENTRIES;
    matches.truncate(MAX_LIST_ENTRIES);
    let mut content = matches.join("\n");
    if truncated {
        content.push_str(&format!(
            "\n... [listing truncated at {MAX_LIST_ENTRIES} entries]"
        ));
    }
    Ok(ToolOutput::ok(truncate_output(content, MAX_LISTING_BYTES)))
}

/// `git ls-files --cached --others --exclude-standard` relative to `dir`.
/// Returns `None` when git is unavailable or `dir` is not in a repository.
async fn git_tracked_files(tool: &str, dir: &Path) -> Option<Vec<String>> {
    let mut command = Command::new("git");
    command
        .args(["ls-files", "--cached", "--others", "--exclude-standard"])
        .current_dir(dir);
    match run_command(tool, command, Duration::from_secs(30)).await {
        Ok(result) if result.code == Some(0) => Some(
            result
                .stdout
                .lines()
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect(),
        ),
        _ => None,
    }
}

/// Manual recursive walk used outside git repositories. Skips `.git` and
/// stops after [`MAX_WALK_VISITS`] entries.
fn walk_matching(root: &Path, matcher: &GlobMatcher) -> Vec<String> {
    let mut results = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    let mut visited = 0usize;

    'walk: while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > MAX_WALK_VISITS || results.len() > MAX_LIST_ENTRIES {
                break 'walk;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if file_type.is_dir() {
                if entry.file_name() != ".git" {
                    stack.push(path);
                }
            } else {
                let rel = path.strip_prefix(root).unwrap_or(&path);
                if matcher.is_match(rel) {
                    results.push(rel.to_string_lossy().into_owned());
                }
            }
        }
    }
    results
}

/// Arguments for [`SearchFilesTool`].
#[derive(Debug, Deserialize)]
pub struct SearchFilesArgs {
    /// Regex pattern to search for.
    pub pattern: String,
    /// Directory to search (default: project root).
    #[serde(default)]
    pub path: Option<String>,
    /// Restrict to files matching this glob, e.g. `*.rs`.
    #[serde(default)]
    pub glob: Option<String>,
}

/// `search_files` — content search via ripgrep, falling back to grep.
pub struct SearchFilesTool;

#[async_trait]
impl Tool for SearchFilesTool {
    fn name(&self) -> &str {
        "search_files"
    }

    fn description(&self) -> &str {
        "Search file contents for a regex pattern (ripgrep if available, grep otherwise). Returns matching lines with file and line number."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regex pattern" },
                "path": { "type": "string", "description": "Directory to search (default: project root)" },
                "glob": { "type": "string", "description": "Restrict to files matching this glob, e.g. *.rs" }
            },
            "required": ["pattern"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: SearchFilesArgs = parse_args(self.name(), args)?;
        if args.pattern.is_empty() {
            return Err(ToolError::InvalidArgs {
                tool: self.name().to_string(),
                message: "pattern must not be empty".to_string(),
            });
        }

        // Keep the path relative when possible so match locations come back
        // relative to the project root.
        let search_path = shellexpand::tilde(args.path.as_deref().unwrap_or(".")).into_owned();

        let mut rg = Command::new("rg");
        rg.args([
            "--line-number",
            "--no-heading",
            "--color",
            "never",
            "--max-columns",
            "500",
        ]);
        if let Some(glob) = &args.glob {
            rg.arg("--glob").arg(glob);
        }
        rg.arg("--regexp")
            .arg(&args.pattern)
            .arg("--")
            .arg(&search_path)
            .current_dir(&ctx.cwd);

        let result = match run_command(self.name(), rg, SEARCH_TIMEOUT).await {
            Ok(result) => result,
            // rg missing or unspawnable: fall back to grep.
            Err(ToolError::Execution { .. }) => {
                let mut grep = Command::new("grep");
                grep.args(["-r", "-n", "-I", "-E"]);
                if let Some(glob) = &args.glob {
                    grep.arg(format!("--include={glob}"));
                }
                grep.arg("-e")
                    .arg(&args.pattern)
                    .arg("--")
                    .arg(&search_path)
                    .current_dir(&ctx.cwd);
                run_command(self.name(), grep, SEARCH_TIMEOUT).await?
            }
            Err(err) => return Err(err),
        };

        // A timed-out search still reports the matches found so far.
        if result.timed_out.is_some() {
            return Ok(render_command_result(&result));
        }

        // Both rg and grep: 0 = matches, 1 = no matches, >1 = error.
        match result.code {
            Some(0) => Ok(ToolOutput::ok(truncate_output(
                result.stdout.trim_end().to_string(),
                MAX_SEARCH_BYTES,
            ))),
            Some(1) => Ok(ToolOutput::ok(format!(
                "No matches for pattern '{}'.",
                args.pattern
            ))),
            _ => {
                let stderr = result.stderr.trim_end();
                let detail = if stderr.is_empty() {
                    "search failed"
                } else {
                    stderr
                };
                Ok(ToolOutput::error(truncate_output(
                    detail.to_string(),
                    MAX_ERROR_BYTES,
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Temp project dir removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("wizard-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }

        fn ctx(&self) -> ToolContext {
            ToolContext::new(&self.0)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn read_file_line_range() {
        let tmp = TempDir::new();
        std::fs::write(tmp.0.join("f.txt"), "one\ntwo\nthree\nfour\n").unwrap();

        let out = ReadFileTool
            .execute(
                json!({ "path": "f.txt", "start_line": 2, "end_line": 3 }),
                &tmp.ctx(),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("two"));
        assert!(out.content.contains("three"));
        assert!(!out.content.contains("one"));
        assert!(!out.content.contains("four"));
    }

    #[tokio::test]
    async fn read_file_missing_is_tool_output_error() {
        let tmp = TempDir::new();
        let out = ReadFileTool
            .execute(json!({ "path": "nope.txt" }), &tmp.ctx())
            .await
            .unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn read_file_rejects_bad_range() {
        let tmp = TempDir::new();
        let err = ReadFileTool
            .execute(
                json!({ "path": "f.txt", "start_line": 5, "end_line": 2 }),
                &tmp.ctx(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn write_file_creates_parents() {
        let tmp = TempDir::new();
        let out = WriteFileTool
            .execute(json!({ "path": "a/b/c.txt", "content": "hi" }), &tmp.ctx())
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(
            std::fs::read_to_string(tmp.0.join("a/b/c.txt")).unwrap(),
            "hi"
        );
    }

    #[tokio::test]
    async fn edit_file_requires_unique_match() {
        let tmp = TempDir::new();
        std::fs::write(tmp.0.join("f.txt"), "foo bar foo").unwrap();
        let ctx = tmp.ctx();

        // Ambiguous match is reported as a tool-level error.
        let out = EditFileTool
            .execute(
                json!({ "path": "f.txt", "old_string": "foo", "new_string": "baz" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("2 times"));

        // replace_all succeeds.
        let out = EditFileTool
            .execute(
                json!({ "path": "f.txt", "old_string": "foo", "new_string": "baz", "replace_all": true }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(
            std::fs::read_to_string(tmp.0.join("f.txt")).unwrap(),
            "baz bar baz"
        );
    }

    #[tokio::test]
    async fn edit_file_missing_old_string_errors() {
        let tmp = TempDir::new();
        std::fs::write(tmp.0.join("f.txt"), "hello").unwrap();
        let out = EditFileTool
            .execute(
                json!({ "path": "f.txt", "old_string": "absent", "new_string": "x" }),
                &tmp.ctx(),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("not found"));
    }

    #[tokio::test]
    async fn list_files_glob_filters() {
        let tmp = TempDir::new();
        std::fs::create_dir_all(tmp.0.join("src")).unwrap();
        std::fs::write(tmp.0.join("src/main.rs"), "").unwrap();
        std::fs::write(tmp.0.join("notes.md"), "").unwrap();

        let out = ListFilesTool
            .execute(json!({ "glob": "**/*.rs" }), &tmp.ctx())
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("src/main.rs"));
        assert!(!out.content.contains("notes.md"));
    }

    #[tokio::test]
    async fn list_files_single_level_marks_dirs() {
        let tmp = TempDir::new();
        std::fs::create_dir_all(tmp.0.join("sub")).unwrap();
        std::fs::write(tmp.0.join("file.txt"), "").unwrap();

        let out = ListFilesTool.execute(json!({}), &tmp.ctx()).await.unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("sub/"));
        assert!(out.content.contains("file.txt"));
    }

    #[tokio::test]
    async fn search_files_finds_pattern() {
        let tmp = TempDir::new();
        std::fs::write(tmp.0.join("f.txt"), "alpha\nneedle here\nomega\n").unwrap();

        let out = SearchFilesTool
            .execute(json!({ "pattern": "needle" }), &tmp.ctx())
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("needle here"));

        let out = SearchFilesTool
            .execute(json!({ "pattern": "zzz_absent" }), &tmp.ctx())
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("No matches"));
    }
}
