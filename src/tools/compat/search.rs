//! `Glob` and `Grep` with Claude Code's arguments and result shapes.
//!
//! `search_files` and `list_files` are Wizard's own and stay the lean
//! defaults. These add what a Claude Code prompt expects of the pair:
//! `output_mode`, context lines, `head_limit` with `offset`, `multiline`, file
//! types, and a `Glob` that sorts by modification time and caps at 100.
//!
//! Both run ripgrep. Without it `Glob` falls back to a directory walk and
//! `Grep` to `grep -E`, which cannot do `multiline` or `type`; the result says
//! so instead of pretending.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{invalid, str_arg};
use crate::tools::shell::run_command;
use crate::tools::{
    Tool, ToolAccess, ToolContext, ToolError, ToolOutput, resolve_path, truncate_output,
};

/// Claude Code's default cap on `Grep` results.
const DEFAULT_HEAD_LIMIT: usize = 250;

/// Claude Code's cap on `Glob` results.
const GLOB_LIMIT: usize = 100;

/// Same budget as `search_files`: a search still going after this is a
/// pattern or a path that will not get better with more time.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(20);

/// Directories that are version-control metadata, never search results.
const VCS_DIRS: &[&str] = &[".git", ".svn", ".hg", ".bzr", ".jj", ".sl"];

/// Byte cap on one result, the one `search_files` uses.
const MAX_BYTES: usize = crate::tools::MAX_SEARCH_BYTES * 2;

pub fn tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(GlobTool), Arc::new(GrepTool)]
}

/// A number the way Claude Code reads one: models send `"3"` as often as `3`.
fn num(args: &Value, key: &str) -> Option<i64> {
    match args.get(key)? {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// A boolean the way Claude Code reads one: `"true"` counts.
fn flag(args: &Value, key: &str) -> Option<bool> {
    match args.get(key)? {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.trim() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Content,
    Files,
    Count,
}

#[derive(Debug)]
struct GrepArgs {
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
    mode: Mode,
    before: Option<i64>,
    after: Option<i64>,
    context: Option<i64>,
    line_numbers: bool,
    ignore_case: bool,
    file_type: Option<String>,
    head_limit: Option<usize>,
    offset: usize,
    multiline: bool,
}

impl GrepArgs {
    fn parse(args: &Value) -> Result<Self, String> {
        let pattern = str_arg(args, "pattern")
            .ok_or("pattern is required")?
            .to_string();
        let mode = match str_arg(args, "output_mode") {
            Some("content") => Mode::Content,
            Some("count") => Mode::Count,
            Some("files_with_matches") | None => {
                // A search sent with grep's own flags runs rather than failing.
                if flag(args, "-c") == Some(true) {
                    Mode::Count
                } else {
                    Mode::Files
                }
            }
            Some(other) => {
                return Err(format!(
                    "output_mode must be content, files_with_matches or count, not {other:?}"
                ));
            }
        };
        let negative = |key: &str| num(args, key).filter(|n| *n < 0).is_some();
        if negative("head_limit") || negative("offset") {
            return Err("head_limit and offset must not be negative".to_string());
        }
        Ok(Self {
            pattern,
            path: str_arg(args, "path")
                .or_else(|| str_arg(args, "file_path"))
                .map(str::to_string),
            glob: str_arg(args, "glob").map(str::to_string),
            mode,
            before: num(args, "-B"),
            after: num(args, "-A"),
            context: num(args, "context").or_else(|| num(args, "-C")),
            line_numbers: flag(args, "-n").unwrap_or(true),
            ignore_case: flag(args, "-i").unwrap_or(false),
            file_type: str_arg(args, "type").map(str::to_string),
            head_limit: num(args, "head_limit").map(|n| n as usize),
            offset: num(args, "offset").unwrap_or(0) as usize,
            multiline: flag(args, "multiline").unwrap_or(false),
        })
    }
}

/// Split a `glob` argument on whitespace, then on commas, leaving a brace
/// group (`*.{ts,tsx}`) whole.
fn split_globs(glob: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in glob.split_whitespace() {
        if raw.contains('{') && raw.contains('}') {
            out.push(raw.to_string());
        } else {
            out.extend(raw.split(',').filter(|p| !p.is_empty()).map(str::to_string));
        }
    }
    out
}

/// The ripgrep arguments for a search, ending in the pattern and the path.
fn rg_args(args: &GrepArgs, search_path: &Path) -> Vec<String> {
    let mut out: Vec<String> = vec!["--hidden".into(), "--color".into(), "never".into()];
    for dir in VCS_DIRS {
        out.extend(["--glob".into(), format!("!{dir}")]);
    }
    out.extend(["--max-columns".into(), "500".into()]);
    if args.multiline {
        out.extend(["-U".into(), "--multiline-dotall".into()]);
    }
    if args.ignore_case {
        out.push("-i".into());
    }
    match args.mode {
        Mode::Files => out.push("-l".into()),
        Mode::Count => out.extend(["-c".into(), "--with-filename".into()]),
        Mode::Content => {
            out.extend(["--no-heading".into(), "--with-filename".into()]);
            if args.line_numbers {
                out.push("-n".into());
            }
            if let Some(c) = args.context {
                out.extend(["-C".into(), c.to_string()]);
            } else {
                if let Some(b) = args.before {
                    out.extend(["-B".into(), b.to_string()]);
                }
                if let Some(a) = args.after {
                    out.extend(["-A".into(), a.to_string()]);
                }
            }
        }
    }
    if args.pattern.starts_with('-') {
        out.extend(["-e".into(), args.pattern.clone()]);
    } else {
        out.push(args.pattern.clone());
    }
    if let Some(kind) = &args.file_type {
        out.extend(["--type".into(), kind.clone()]);
    }
    if let Some(glob) = &args.glob {
        for g in split_globs(glob) {
            out.extend(["--glob".into(), g]);
        }
    }
    out.push("--".into());
    out.push(search_path.display().to_string());
    out
}

/// `head -N` after `tail -n +offset`, reporting the limit only when it cut
/// something, so the model knows there is more to page to.
fn page<T>(items: Vec<T>, limit: Option<usize>, offset: usize) -> (Vec<T>, Option<usize>) {
    if limit == Some(0) {
        return (items.into_iter().skip(offset).collect(), None);
    }
    let limit = limit.unwrap_or(DEFAULT_HEAD_LIMIT);
    let truncated = items.len().saturating_sub(offset) > limit;
    let kept = items.into_iter().skip(offset).take(limit).collect();
    (kept, truncated.then_some(limit))
}

fn limit_info(limit: Option<usize>, offset: usize) -> String {
    let mut parts = Vec::new();
    if let Some(limit) = limit {
        parts.push(format!("limit: {limit}"));
    }
    if offset > 0 {
        parts.push(format!("offset: {offset}"));
    }
    parts.join(", ")
}

fn relative(path: &str, cwd: &Path) -> String {
    Path::new(path)
        .strip_prefix(cwd)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.to_string())
}

/// Relativize the `path` part of `path:rest` lines. A context line uses `-`
/// where a match line uses `:` after the path, and a path can hold either, so
/// match against the search root instead of splitting on a separator.
fn relativize_line(line: &str, cwd: &Path) -> String {
    let prefix = format!("{}/", cwd.display());
    line.strip_prefix(&prefix).unwrap_or(line).to_string()
}

fn mtime(path: &str) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Turn raw search output into the text Claude Code returns for the mode.
fn format_results(mode: Mode, lines: Vec<String>, args: &GrepArgs, cwd: &Path) -> String {
    match mode {
        Mode::Content => {
            let (kept, applied) = page(lines, args.head_limit, args.offset);
            let body: Vec<String> = kept.iter().map(|l| relativize_line(l, cwd)).collect();
            let text = if body.is_empty() {
                if args.offset > 0 {
                    "No entries at this offset".to_string()
                } else {
                    "No matches found".to_string()
                }
            } else {
                body.join("\n")
            };
            let info = limit_info(applied, args.offset);
            if info.is_empty() {
                text
            } else {
                format!("{text}\n\n[Showing results with pagination = {info}]")
            }
        }
        Mode::Count => {
            let (kept, applied) = page(lines, args.head_limit, args.offset);
            let (mut total, mut files) = (0u64, 0u64);
            let body: Vec<String> = kept
                .iter()
                .map(|line| {
                    if let Some((_, n)) = line.rsplit_once(':')
                        && let Ok(n) = n.trim().parse::<u64>()
                    {
                        total += n;
                        files += 1;
                    }
                    relativize_line(line, cwd)
                })
                .collect();
            let text = if body.is_empty() {
                "No matches found".to_string()
            } else {
                body.join("\n")
            };
            let info = limit_info(applied, args.offset);
            format!(
                "{text}\n\nFound {total} total {} across {files} {}.{}",
                if total == 1 {
                    "occurrence"
                } else {
                    "occurrences"
                },
                if files == 1 { "file" } else { "files" },
                if info.is_empty() {
                    String::new()
                } else {
                    format!(" with pagination = {info}")
                }
            )
        }
        Mode::Files => {
            let mut sorted = lines;
            sorted.sort_by(|a, b| mtime(b).cmp(&mtime(a)).then_with(|| a.cmp(b)));
            let (kept, applied) = page(sorted, args.head_limit, args.offset);
            if kept.is_empty() {
                return "No files found".to_string();
            }
            let info = limit_info(applied, args.offset);
            format!(
                "Found {} {}{}\n{}",
                kept.len(),
                if kept.len() == 1 { "file" } else { "files" },
                if info.is_empty() {
                    String::new()
                } else {
                    format!(" {info}")
                },
                kept.iter()
                    .map(|p| relative(p, cwd))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        }
    }
}

/// `grep -E` standing in for ripgrep when it is not installed.
fn grep_command(args: &GrepArgs, search_path: &Path) -> Command {
    let mut grep = Command::new("grep");
    grep.args(["-r", "-I", "-E", "-H"]);
    match args.mode {
        Mode::Files => {
            grep.arg("-l");
        }
        Mode::Count => {
            grep.arg("-c");
        }
        Mode::Content => {
            if args.line_numbers {
                grep.arg("-n");
            }
            if let Some(c) = args.context {
                grep.arg(format!("-C{c}"));
            } else {
                if let Some(b) = args.before {
                    grep.arg(format!("-B{b}"));
                }
                if let Some(a) = args.after {
                    grep.arg(format!("-A{a}"));
                }
            }
        }
    }
    if args.ignore_case {
        grep.arg("-i");
    }
    for dir in VCS_DIRS {
        grep.arg(format!("--exclude-dir={dir}"));
    }
    if let Some(glob) = &args.glob {
        for g in split_globs(glob) {
            grep.arg(format!("--include={g}"));
        }
    }
    grep.arg("-e").arg(&args.pattern).arg("--").arg(search_path);
    grep
}

/// `Grep`.
pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "Grep"
    }

    fn description(&self) -> &str {
        "Search file contents with ripgrep regex syntax. output_mode: files_with_matches \
         (default, newest files first), content (matching lines; -A, -B, -C or context for \
         surrounding lines, -n for line numbers, default on), or count. Narrow with glob \
         (\"*.rs\", \"*.{ts,tsx}\") or type (\"py\", \"rust\"). Results stop at head_limit \
         (default 250, 0 for all); page on with offset. Patterns match within a line unless \
         multiline is true. .gitignore is respected; to search an ignored file, pass its path."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "The regular expression pattern to search for in file contents" },
                "path": { "type": "string", "description": "File or directory to search in. Defaults to the project root." },
                "glob": { "type": "string", "description": "Glob pattern to filter files (e.g. \"*.js\", \"*.{ts,tsx}\")" },
                "output_mode": { "type": "string", "enum": ["content", "files_with_matches", "count"], "description": "Defaults to files_with_matches." },
                "-B": { "type": "number", "description": "Lines to show before each match. content mode only." },
                "-A": { "type": "number", "description": "Lines to show after each match. content mode only." },
                "-C": { "type": "number", "description": "Alias for context." },
                "context": { "type": "number", "description": "Lines to show before and after each match. content mode only." },
                "-n": { "type": "boolean", "description": "Show line numbers. content mode only. Defaults to true." },
                "-i": { "type": "boolean", "description": "Case insensitive search" },
                "type": { "type": "string", "description": "File type to search (rg --type): js, py, rust, go, java, ..." },
                "head_limit": { "type": "number", "description": "Limit output to the first N lines or entries. Defaults to 250; 0 is unlimited." },
                "offset": { "type": "number", "description": "Skip the first N lines or entries before applying head_limit. Defaults to 0." },
                "multiline": { "type": "boolean", "description": "Let . match newlines and patterns span lines (rg -U --multiline-dotall). Default false." }
            },
            "required": ["pattern"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args = match GrepArgs::parse(&args) {
            Ok(args) => args,
            Err(message) => return Ok(invalid(self.name(), message)),
        };
        if args.pattern.is_empty() {
            return Ok(invalid(self.name(), "pattern must not be empty"));
        }
        let search_path = args
            .path
            .as_deref()
            .map_or_else(|| ctx.cwd.clone(), |p| resolve_path(ctx, p));
        if !search_path.exists() {
            return Ok(ToolOutput::error(format!(
                "Path does not exist: {}. Note: your current working directory is {}.",
                search_path.display(),
                ctx.cwd.display()
            )));
        }

        let mut rg = Command::new("rg");
        rg.args(rg_args(&args, &search_path)).current_dir(&ctx.cwd);
        let (result, used_rg) = match run_command(self.name(), rg, SEARCH_TIMEOUT).await {
            Ok(result) => (result, true),
            Err(ToolError::Execution { .. }) => {
                if args.multiline || args.file_type.is_some() {
                    return Ok(ToolOutput::error(
                        "ripgrep (rg) is not installed, and multiline and type need it",
                    ));
                }
                let mut grep = grep_command(&args, &search_path);
                grep.current_dir(&ctx.cwd);
                (run_command(self.name(), grep, SEARCH_TIMEOUT).await?, false)
            }
            Err(err) => return Err(err),
        };
        if let Some(seconds) = result.timed_out {
            return Ok(ToolOutput::error(format!(
                "Search timed out after {seconds}s; narrow the path or the pattern."
            )));
        }
        // Both tools: 0 matches, 1 none, 2 an error (possibly with partial output).
        if result.code.is_some_and(|c| c > 1) && result.stdout.trim().is_empty() {
            let stderr = result.stderr.trim();
            return Ok(ToolOutput::error(if stderr.is_empty() {
                "search failed".to_string()
            } else {
                stderr.to_string()
            }));
        }
        let mut lines: Vec<String> = result.stdout.lines().map(str::to_string).collect();
        if !used_rg && args.mode == Mode::Count {
            lines.retain(|l| !l.ends_with(":0"));
        }
        let text = format_results(args.mode, lines, &args, &ctx.cwd);
        Ok(ToolOutput::ok(truncate_output(text, MAX_BYTES)))
    }
}

/// `Glob`.
pub struct GlobTool;

/// Split an absolute pattern into the directory it names and the pattern
/// below it: everything before the first glob character, up to the last `/`.
fn split_absolute(pattern: &str) -> (Option<PathBuf>, String) {
    if !pattern.starts_with('/') {
        return (None, pattern.to_string());
    }
    let first_glob = pattern.find(['*', '?', '[', '{']);
    let Some(first_glob) = first_glob else {
        let path = Path::new(pattern);
        return (
            path.parent().map(Path::to_path_buf),
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        );
    };
    match pattern[..first_glob].rfind('/') {
        Some(0) => (Some(PathBuf::from("/")), pattern[1..].to_string()),
        Some(slash) => (
            Some(PathBuf::from(&pattern[..slash])),
            pattern[slash + 1..].to_string(),
        ),
        None => (None, pattern.to_string()),
    }
}

/// Walk `root` for files matching `pattern`, oldest first, for when ripgrep is
/// not installed. Same gitignore-style matching ripgrep's `--glob` gives a
/// bare name (`*.rs` matches at any depth) is approximated with a leading
/// `**/` when the pattern has no `/`.
fn walk_glob(root: &Path, pattern: &str) -> Result<Vec<String>, String> {
    let effective = if pattern.contains('/') {
        pattern.trim_start_matches('/').to_string()
    } else {
        format!("**/{pattern}")
    };
    let matcher = globset::Glob::new(&effective)
        .map_err(|err| format!("invalid glob {pattern:?}: {err}"))?
        .compile_matcher();
    let mut found: Vec<(SystemTime, String)> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let mut visited = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > 200_000 {
                return Ok(sorted_paths(found));
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                stack.push(path);
            } else if let Ok(rel) = path.strip_prefix(root)
                && matcher.is_match(rel)
            {
                let modified = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                found.push((modified, path.display().to_string()));
            }
        }
    }
    Ok(sorted_paths(found))
}

fn sorted_paths(mut found: Vec<(SystemTime, String)>) -> Vec<String> {
    found.sort();
    found.into_iter().map(|(_, path)| path).collect()
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "Glob"
    }

    fn description(&self) -> &str {
        "Find files by name pattern, e.g. \"**/*.rs\" or \"src/**/*.ts\". Returns up to 100 \
         paths sorted by modification time, oldest first, and says when it cut the list. \
         Dotfiles are included and .gitignore is not applied."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "The glob pattern to match files against" },
                "path": { "type": "string", "description": "The directory to search in. Omit for the project root." }
            },
            "required": ["pattern"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let Some(pattern) = str_arg(&args, "pattern") else {
            return Ok(invalid(self.name(), "pattern is required"));
        };
        let path = str_arg(&args, "path");
        if pattern.contains('\0') || path.is_some_and(|p| p.contains('\0')) {
            return Ok(invalid(
                self.name(),
                "pattern and path must not contain a null byte",
            ));
        }
        let (base, relative_pattern) = split_absolute(pattern);
        let root =
            base.unwrap_or_else(|| path.map_or_else(|| ctx.cwd.clone(), |p| resolve_path(ctx, p)));
        if !root.is_dir() {
            return Ok(ToolOutput::error(format!(
                "Directory does not exist: {}. Note: your current working directory is {}.",
                root.display(),
                ctx.cwd.display()
            )));
        }

        let mut rg = Command::new("rg");
        rg.args([
            "--files",
            "--glob",
            &relative_pattern,
            "--sort=modified",
            "--no-ignore",
            "--hidden",
        ])
        .current_dir(&root);
        let all: Vec<String> = match run_command(self.name(), rg, SEARCH_TIMEOUT).await {
            Ok(result) if result.code.is_some_and(|c| c <= 1) => result
                .stdout
                .lines()
                .map(|line| root.join(line).display().to_string())
                .collect(),
            Ok(result) => {
                let stderr = result.stderr.trim();
                return Ok(ToolOutput::error(if stderr.is_empty() {
                    "glob failed".to_string()
                } else {
                    stderr.to_string()
                }));
            }
            Err(ToolError::Execution { .. }) => match walk_glob(&root, &relative_pattern) {
                Ok(paths) => paths,
                Err(message) => return Ok(invalid(self.name(), message)),
            },
            Err(err) => return Err(err),
        };

        if all.is_empty() {
            return Ok(ToolOutput::ok("No files found"));
        }
        let truncated = all.len() > GLOB_LIMIT;
        let mut lines: Vec<String> = all
            .iter()
            .take(GLOB_LIMIT)
            .map(|p| relative(p, &ctx.cwd))
            .collect();
        if truncated {
            lines.push(
                "(Results are truncated. Consider using a more specific path or pattern.)".into(),
            );
        }
        Ok(ToolOutput::ok(truncate_output(lines.join("\n"), MAX_BYTES)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: Value) -> GrepArgs {
        GrepArgs::parse(&args).unwrap()
    }

    fn has_rg() -> bool {
        std::process::Command::new("rg")
            .arg("--version")
            .output()
            .is_ok()
    }

    #[test]
    fn numbers_and_flags_arrive_as_strings_too() {
        let a = parse(
            json!({ "pattern": "x", "-A": "2", "-i": "true", "head_limit": "5",
                              "output_mode": "content" }),
        );
        assert_eq!(a.after, Some(2));
        assert!(a.ignore_case);
        assert_eq!(a.head_limit, Some(5));
    }

    #[test]
    fn greps_own_flags_and_file_path_are_accepted() {
        let a = parse(json!({ "pattern": "x", "-c": true, "file_path": "src" }));
        assert_eq!(a.mode, Mode::Count);
        assert_eq!(a.path.as_deref(), Some("src"));
    }

    #[test]
    fn context_wins_over_before_and_after() {
        let a = parse(
            json!({ "pattern": "x", "output_mode": "content", "context": 3,
                              "-A": 9, "-B": 9 }),
        );
        let args = rg_args(&a, Path::new("/p"));
        assert!(args.windows(2).any(|w| w == ["-C", "3"]));
        assert!(!args.iter().any(|a| a == "-A" || a == "-B"));
    }

    #[test]
    fn a_pattern_starting_with_a_dash_goes_through_dash_e() {
        let a = parse(json!({ "pattern": "-foo" }));
        let args = rg_args(&a, Path::new("/p"));
        assert!(args.windows(2).any(|w| w == ["-e", "-foo"]));
    }

    #[test]
    fn context_flags_are_ignored_outside_content_mode() {
        let a = parse(json!({ "pattern": "x", "-A": 3, "-n": true }));
        let args = rg_args(&a, Path::new("/p"));
        assert!(!args.iter().any(|a| a == "-A" || a == "-n"));
        assert!(args.iter().any(|a| a == "-l"));
    }

    #[test]
    fn multiline_and_type_reach_ripgrep() {
        let a = parse(json!({ "pattern": "x", "multiline": true, "type": "rust" }));
        let args = rg_args(&a, Path::new("/p"));
        assert!(args.iter().any(|a| a == "-U"));
        assert!(args.iter().any(|a| a == "--multiline-dotall"));
        assert!(args.windows(2).any(|w| w == ["--type", "rust"]));
    }

    #[test]
    fn globs_split_on_commas_and_spaces_but_not_inside_braces() {
        assert_eq!(
            split_globs("*.js,*.ts *.{a,b}"),
            vec!["*.js", "*.ts", "*.{a,b}"]
        );
    }

    #[test]
    fn head_limit_reports_only_when_it_cut_something() {
        let (kept, applied) = page((0..10).collect::<Vec<_>>(), Some(3), 0);
        assert_eq!(kept, vec![0, 1, 2]);
        assert_eq!(applied, Some(3));
        let (kept, applied) = page((0..3).collect::<Vec<_>>(), Some(3), 0);
        assert_eq!(kept.len(), 3);
        assert_eq!(applied, None);
        let (kept, applied) = page((0..400).collect::<Vec<_>>(), Some(0), 5);
        assert_eq!(kept.len(), 395);
        assert_eq!(applied, None);
    }

    #[test]
    fn content_mode_paginates_and_says_so() {
        let a = parse(
            json!({ "pattern": "x", "output_mode": "content", "head_limit": 2,
                              "offset": 1 }),
        );
        let lines = ["/w/a:1:x", "/w/a:2:x", "/w/a:3:x", "/w/a:4:x"]
            .map(str::to_string)
            .to_vec();
        let text = format_results(Mode::Content, lines, &a, Path::new("/w"));
        assert_eq!(
            text,
            "a:2:x\na:3:x\n\n[Showing results with pagination = limit: 2, offset: 1]"
        );
    }

    #[test]
    fn an_offset_past_the_last_match_says_so() {
        let a = parse(json!({ "pattern": "x", "output_mode": "content", "offset": 9 }));
        let text = format_results(Mode::Content, vec!["/w/a:1:x".into()], &a, Path::new("/w"));
        assert!(text.starts_with("No entries at this offset"));
    }

    #[test]
    fn count_mode_totals_what_it_shows() {
        let a = parse(json!({ "pattern": "x", "output_mode": "count" }));
        let lines = vec!["/w/a.rs:3".to_string(), "/w/b.rs:1".to_string()];
        let text = format_results(Mode::Count, lines, &a, Path::new("/w"));
        assert_eq!(
            text,
            "a.rs:3\nb.rs:1\n\nFound 4 total occurrences across 2 files."
        );
    }

    #[test]
    fn absolute_patterns_split_at_the_static_prefix() {
        assert_eq!(
            split_absolute("/a/b/*.rs"),
            (Some(PathBuf::from("/a/b")), "*.rs".to_string())
        );
        assert_eq!(
            split_absolute("/*.txt"),
            (Some(PathBuf::from("/")), "*.txt".to_string())
        );
        assert_eq!(
            split_absolute("src/**/*.rs"),
            (None, "src/**/*.rs".to_string())
        );
    }

    #[tokio::test]
    async fn grep_runs_each_mode_against_real_files() {
        if !has_rg() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "alpha\nalpha\n").unwrap();
        let ctx = ToolContext::new(dir.path());

        let files = GrepTool
            .execute(json!({ "pattern": "alpha" }), &ctx)
            .await
            .unwrap();
        assert!(
            files.content.starts_with("Found 2 files\n"),
            "{}",
            files.content
        );

        let content = GrepTool
            .execute(
                json!({ "pattern": "alpha", "output_mode": "content", "glob": "*.rs", "-A": 1 }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(content.content, "a.rs:1:fn alpha() {}\na.rs-2-fn beta() {}");

        let count = GrepTool
            .execute(json!({ "pattern": "alpha", "output_mode": "count" }), &ctx)
            .await
            .unwrap();
        assert!(
            count
                .content
                .contains("Found 3 total occurrences across 2 files.")
        );

        let none = GrepTool
            .execute(json!({ "pattern": "zzz" }), &ctx)
            .await
            .unwrap();
        assert_eq!(none.content, "No files found");

        let bad = GrepTool
            .execute(json!({ "pattern": "(" }), &ctx)
            .await
            .unwrap();
        assert!(
            bad.is_error,
            "a regex ripgrep rejects is an error: {}",
            bad.content
        );
    }

    #[tokio::test]
    async fn grep_multiline_spans_lines() {
        if !has_rg() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "struct A {\n    field: u8,\n}\n").unwrap();
        let ctx = ToolContext::new(dir.path());
        let single = GrepTool
            .execute(json!({ "pattern": "struct A \\{[\\s\\S]*?field" }), &ctx)
            .await
            .unwrap();
        assert_eq!(single.content, "No files found");
        let multi = GrepTool
            .execute(
                json!({ "pattern": "struct A \\{[\\s\\S]*?field", "multiline": true }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(multi.content.contains("a.rs"), "{}", multi.content);
    }

    #[tokio::test]
    async fn glob_lists_matches_and_says_when_nothing_matches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/deep")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "").unwrap();
        std::fs::write(dir.path().join("src/deep/b.rs"), "").unwrap();
        std::fs::write(dir.path().join("src/c.txt"), "").unwrap();
        let ctx = ToolContext::new(dir.path());

        let out = GlobTool
            .execute(json!({ "pattern": "src/**/*.rs" }), &ctx)
            .await
            .unwrap();
        let mut lines: Vec<&str> = out.content.lines().collect();
        lines.sort_unstable();
        assert_eq!(lines, vec!["src/a.rs", "src/deep/b.rs"]);

        let none = GlobTool
            .execute(json!({ "pattern": "**/*.py" }), &ctx)
            .await
            .unwrap();
        assert_eq!(none.content, "No files found");

        let bad_dir = GlobTool
            .execute(json!({ "pattern": "*", "path": "nope" }), &ctx)
            .await
            .unwrap();
        assert!(bad_dir.is_error);
    }

    #[tokio::test]
    async fn glob_caps_at_one_hundred_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..120 {
            std::fs::write(dir.path().join(format!("f{i}.txt")), "").unwrap();
        }
        let ctx = ToolContext::new(dir.path());
        let out = GlobTool
            .execute(json!({ "pattern": "*.txt" }), &ctx)
            .await
            .unwrap();
        assert_eq!(out.content.lines().count(), 101);
        assert!(out.content.ends_with("more specific path or pattern.)"));
    }

    #[test]
    fn the_walk_fallback_matches_like_the_glob_would() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::write(dir.path().join("a/b/x.rs"), "").unwrap();
        std::fs::write(dir.path().join("y.rs"), "").unwrap();
        let found = walk_glob(dir.path(), "*.rs").unwrap();
        assert_eq!(found.len(), 2);
        let nested = walk_glob(dir.path(), "a/**/*.rs").unwrap();
        assert_eq!(nested.len(), 1);
    }
}
