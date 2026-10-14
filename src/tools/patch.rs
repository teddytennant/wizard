//! `apply_patch`: every hunk of a change, across files, in one call.
//!
//! The format is the one Codex uses, so a model trained on it needs no
//! teaching: `*** Update File: <path>`, then hunks of context, `-` and `+`
//! lines, each optionally opened by `@@ <a line above it>`. A context line is
//! sent once, where `edit_file` sends it in both `old_string` and
//! `new_string`. A `--- a/x` / `+++ b/x` pair from a unified diff is read as a
//! file header too.
//!
//! Hunks are matched against the file as it was, so their order does not
//! matter, and a file is written only if all of its hunks found one place
//! each. Files are independent: one that fails leaves the others applied, and
//! the error names the lines the file really has, so the retry is the failed
//! hunks and no re-read.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::tools::{
    Tool, ToolAccess, ToolContext, ToolError, ToolOutput, parse_args, resolve_path,
};

pub const APPLY_PATCH_TOOL_NAME: &str = "apply_patch";

/// Lines of the file quoted back in a "not found" error.
const MAX_QUOTED_LINES: usize = 40;

const DESCRIPTION: &str = "Edit several places or files in one call. Prefer it to edit_file for more than one hunk. patch is:
*** Update File: <path>
@@ <optional: a line above the hunk that occurs once, e.g. its func header>
 <unchanged line>
-<removed line>
+<added line>
Further `@@` hunks and files follow. `*** Add File: <path>` takes only + lines; `*** Delete File: <path>` takes none. Without `@@` a hunk's lines must occur once in the file. A file is changed only if all its hunks apply.";

#[derive(Debug, PartialEq)]
enum Change {
    Add(String),
    Delete,
    Update {
        hunks: Vec<Hunk>,
        move_to: Option<String>,
    },
}

#[derive(Debug, PartialEq)]
struct FilePatch {
    path: String,
    change: Change,
}

impl FilePatch {
    fn new(path: &str, change: Change) -> Self {
        Self {
            path: path.trim().to_string(),
            change,
        }
    }

    fn update(path: &str) -> Self {
        Self::new(
            path,
            Change::Update {
                hunks: Vec::new(),
                move_to: None,
            },
        )
    }
}

/// One hunk: the `@@` lines that place it, then `(op, text)` per line, where
/// op is `' '`, `'-'` or `'+'`.
#[derive(Debug, Default, PartialEq)]
struct Hunk {
    anchors: Vec<String>,
    lines: Vec<(char, String)>,
}

impl Hunk {
    fn old(&self) -> Vec<&str> {
        self.side('+')
    }

    /// The lines that are not `skip`: the old side without `+`, the new side
    /// without `-`.
    fn side(&self, skip: char) -> Vec<&str> {
        self.lines
            .iter()
            .filter(|(op, _)| *op != skip)
            .map(|(_, text)| text.as_str())
            .collect()
    }
}

/// A path out of a unified diff header: no `a/` or `b/`, no timestamp.
fn diff_path(raw: &str) -> &str {
    let path = raw.split('\t').next().unwrap_or(raw).trim();
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
}

fn parse(patch: &str) -> Result<Vec<FilePatch>, String> {
    let mut files: Vec<FilePatch> = Vec::new();
    let mut lines = patch.lines().enumerate().peekable();
    let mut git_header = false;
    while let Some((index, line)) = lines.next() {
        let marker = line.trim_end();
        if matches!(
            marker,
            "*** Begin Patch" | "*** End Patch" | "*** End of File"
        ) || line.starts_with("\\ ")
        {
            continue;
        }
        if line.starts_with("diff --git ") {
            git_header = true;
            continue;
        }
        if let Some(path) = marker.strip_prefix("*** Update File: ") {
            files.push(FilePatch::update(path));
            continue;
        }
        if let Some(path) = marker.strip_prefix("*** Add File: ") {
            files.push(FilePatch::new(path, Change::Add(String::new())));
            continue;
        }
        if let Some(path) = marker.strip_prefix("*** Delete File: ") {
            files.push(FilePatch::new(path, Change::Delete));
            continue;
        }
        if let Some(to) = marker.strip_prefix("*** Move to: ") {
            match files.last_mut().map(|file| &mut file.change) {
                Some(Change::Update { move_to, .. }) => *move_to = Some(to.trim().to_string()),
                _ => {
                    return Err(format!(
                        "line {}: `*** Move to:` must follow `*** Update File:`",
                        index + 1
                    ));
                }
            }
            continue;
        }
        if let Some(old) = marker.strip_prefix("--- ")
            && let Some((_, next)) = lines.peek()
            && let Some(new) = next.trim_end().strip_prefix("+++ ")
        {
            files.push(match (diff_path(old), diff_path(new)) {
                ("/dev/null", new) => FilePatch::new(new, Change::Add(String::new())),
                (old, "/dev/null") => FilePatch::new(old, Change::Delete),
                (_, new) => FilePatch::update(new),
            });
            lines.next();
            git_header = false;
            continue;
        }
        if git_header {
            continue;
        }
        let Some(file) = files.last_mut() else {
            return Err(format!(
                "line {}: expected `*** Update File: <path>` before {line:?}",
                index + 1
            ));
        };
        match &mut file.change {
            Change::Add(content) => {
                if let Some(text) = line.strip_prefix('+') {
                    content.push_str(text);
                    content.push('\n');
                } else if !line.starts_with("@@") && !line.trim().is_empty() {
                    return Err(format!(
                        "line {}: every line of an added file starts with `+`",
                        index + 1
                    ));
                }
            }
            Change::Delete => {
                if !line.trim().is_empty() {
                    return Err(format!("line {}: a deleted file takes no lines", index + 1));
                }
            }
            Change::Update { hunks, .. } => {
                if let Some(rest) = line.strip_prefix("@@") {
                    // Stacked `@@` lines narrow one hunk; they do not open two.
                    if hunks.last().is_none_or(|hunk| !hunk.lines.is_empty()) {
                        hunks.push(Hunk::default());
                    }
                    let rest = rest.trim();
                    // A unified diff's `-12,5 +12,6 @@` is line numbers, and
                    // a model's line numbers are not to be trusted.
                    let numbers = rest.starts_with('-') && rest.contains("@@");
                    if !numbers
                        && !rest.is_empty()
                        && let Some(hunk) = hunks.last_mut()
                    {
                        hunk.anchors.push(rest.to_string());
                    }
                    continue;
                }
                if hunks.is_empty() {
                    hunks.push(Hunk::default());
                }
                // A line with no marker is context: the usual slip is to drop
                // the leading space, and a wrong guess here only fails to match.
                let (op, text) = match line.chars().next() {
                    Some(op @ (' ' | '+' | '-')) => (op, &line[1..]),
                    _ => (' ', line),
                };
                if let Some(hunk) = hunks.last_mut() {
                    hunk.lines.push((op, text.to_string()));
                }
            }
        }
    }

    for file in &mut files {
        let Change::Update { hunks, move_to } = &mut file.change else {
            continue;
        };
        for hunk in hunks.iter_mut() {
            // Blank lines between hunks are spacing, not context.
            while hunk
                .lines
                .last()
                .is_some_and(|(op, text)| *op == ' ' && text.trim().is_empty())
            {
                hunk.lines.pop();
            }
        }
        hunks.retain(|hunk| hunk.lines.iter().any(|(op, _)| *op != ' '));
        if hunks.is_empty() && move_to.is_none() {
            return Err(format!("{}: no `+` or `-` lines", file.path));
        }
    }
    if files.is_empty() {
        return Err("no `*** Update File: <path>` in the patch".to_string());
    }
    Ok(files)
}

fn patch_arg(args: &Value) -> Option<Vec<FilePatch>> {
    parse(args.get("patch")?.as_str()?).ok()
}

/// Every file a call's patch writes, for checkpoints and stale-read tracking.
pub fn targets(args: &Value) -> Vec<String> {
    let mut paths = Vec::new();
    for file in patch_arg(args).unwrap_or_default() {
        if let Change::Update {
            move_to: Some(to), ..
        } = &file.change
        {
            paths.push(to.clone());
        }
        paths.push(file.path);
    }
    paths
}

/// What a surface titles the call with: `a.go`, or `a.go +2` for three files.
pub fn label(args: &Value) -> Option<String> {
    let files = patch_arg(args)?;
    let first = &files.first()?.path;
    Some(match files.len() {
        1 => first.clone(),
        n => format!("{first} +{}", n - 1),
    })
}

/// The text a patch removes from and the text it leaves, hunk after hunk, for
/// a surface that draws an edit as old against new.
pub fn sides(args: &Value) -> Option<(String, String)> {
    let (mut old, mut new) = (String::new(), String::new());
    for file in patch_arg(args)? {
        match &file.change {
            Change::Add(content) => new.push_str(content),
            Change::Delete => {}
            Change::Update { hunks, .. } => {
                for hunk in hunks {
                    for (side, skip) in [(&mut old, '+'), (&mut new, '-')] {
                        for line in hunk.side(skip) {
                            side.push_str(line);
                            side.push('\n');
                        }
                    }
                }
            }
        }
    }
    Some((old, new))
}

fn numbers(lines: &[usize]) -> String {
    let mut shown: Vec<String> = lines.iter().take(8).map(|n| (n + 1).to_string()).collect();
    if lines.len() > shown.len() {
        shown.push("...".to_string());
    }
    shown.join(", ")
}

fn indent(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

/// Where `old` comes nearest to matching at or after line `from`, and what the
/// file has there, so the caller can fix its text from the error alone.
pub(crate) fn closest(lines: &[&str], old: &[&str], from: usize) -> String {
    let score = |start: usize| {
        old.iter()
            .enumerate()
            .filter(|(offset, want)| {
                !want.trim().is_empty()
                    && lines
                        .get(start + offset)
                        .is_some_and(|line| line.trim() == want.trim())
            })
            .count()
    };
    let best = (from..lines.len())
        .map(|start| (score(start), std::cmp::Reverse(start)))
        .max()
        .filter(|(hits, _)| *hits > 0);
    let Some((_, std::cmp::Reverse(start))) = best else {
        return "None of its lines are in the file.".to_string();
    };
    let end = (start + old.len().min(MAX_QUOTED_LINES)).min(lines.len());
    let differs = (start..end)
        .find(|&at| lines[at].trim() != old[at - start].trim())
        .map(|at| format!(" Line {} is not your {:?}.", at + 1, old[at - start].trim()))
        .unwrap_or_default();
    // A few lines above as well: what the caller got wrong is often the
    // line it expected just before.
    let above = start.saturating_sub(3);
    format!(
        "Closest is line {}.{differs} The file has, lines {}-{end}:\n{}",
        start + 1,
        above + 1,
        lines[above..end].join("\n")
    )
}

fn exact(line: &str) -> &str {
    line
}

/// Where one hunk goes: the first line it replaces, how many, and what with.
fn locate(lines: &[&str], hunk: &Hunk, eol: &str) -> Result<(usize, usize, Vec<String>), String> {
    let mut from = 0;
    for (depth, anchor) in hunk.anchors.iter().enumerate() {
        let after = |keep: &dyn Fn(&str) -> bool| -> Vec<usize> {
            (from..lines.len()).filter(|&at| keep(lines[at])).collect()
        };
        let mut hits = after(&|line| line.trim() == anchor);
        if hits.is_empty() {
            hits = after(&|line| line.contains(anchor.as_str()));
        }
        match hits.as_slice() {
            [] => return Err(format!("no line matches `@@ {anchor}`")),
            [_, _, ..] if depth == 0 => {
                return Err(format!(
                    "`@@ {anchor}` matches lines {}; use a line that occurs once",
                    numbers(&hits)
                ));
            }
            [first, ..] => from = *first,
        }
    }

    let old = hunk.old();
    if old.is_empty() {
        let at = if hunk.anchors.is_empty() {
            lines.len()
        } else {
            from + 1
        };
        return Ok((at, 0, replacement(lines, hunk, at, eol)));
    }

    // Exact first, then forgiving trailing whitespace, then indentation. Each
    // tier has to settle on one place before a looser one is tried.
    let tiers: [fn(&str) -> &str; 3] = [exact, str::trim_end, str::trim];
    for same in tiers {
        let hits: Vec<usize> = (from..(lines.len() + 1).saturating_sub(old.len()))
            .filter(|&start| {
                old.iter()
                    .enumerate()
                    .all(|(offset, want)| same(lines[start + offset]) == same(want))
            })
            .collect();
        let start = match hits.as_slice() {
            [] => continue,
            [one] => *one,
            // Under `@@` the hunk is the first match after that line.
            [first, ..] if !hunk.anchors.is_empty() => *first,
            many => {
                return Err(format!(
                    "matches at lines {}; open the hunk with `@@ <a line above it that occurs once>` or add context",
                    numbers(many)
                ));
            }
        };
        return Ok((start, old.len(), replacement(lines, hunk, start, eol)));
    }
    Err(format!("lines not found. {}", closest(lines, &old, from)))
}

/// The lines a hunk leaves at `start`. Context keeps the file's own text, and
/// when the hunk was written one indent off, its `+` lines are moved to match.
fn replacement(lines: &[&str], hunk: &Hunk, start: usize, eol: &str) -> Vec<String> {
    let (strip, add) = hunk
        .old()
        .iter()
        .zip(&lines[start.min(lines.len())..])
        .find(|(want, _)| !want.trim().is_empty())
        .map(|(want, line)| (indent(want), indent(line)))
        .and_then(|(sent, real)| {
            if let Some(extra) = sent.strip_suffix(real) {
                Some((extra, ""))
            } else {
                real.strip_suffix(sent).map(|missing| ("", missing))
            }
        })
        // One space is a context line sent without its marker, not an indent.
        .filter(|shift| *shift != ("", " "))
        .unwrap_or(("", ""));
    let mut out = Vec::new();
    let mut at = start;
    for (op, text) in &hunk.lines {
        match op {
            '+' => {
                let text = text.strip_prefix(strip).unwrap_or(text);
                let add = if text.trim().is_empty() { "" } else { add };
                out.push(format!("{add}{text}{eol}"));
            }
            ' ' => {
                out.push(lines[at].to_string());
                at += 1;
            }
            _ => at += 1,
        }
    }
    out
}

/// Apply every hunk to `content`, or none. Returns the new text and the first
/// line that changed.
fn apply(content: &str, hunks: &[Hunk]) -> Result<(String, usize), String> {
    let newline_at_end = content.ends_with('\n') || content.is_empty();
    let mut lines: Vec<&str> = content.split('\n').collect();
    if newline_at_end {
        lines.pop();
    }
    let eol = if lines.first().is_some_and(|line| line.ends_with('\r')) {
        "\r"
    } else {
        ""
    };

    let mut edits = Vec::with_capacity(hunks.len());
    for (index, hunk) in hunks.iter().enumerate() {
        let name = match hunk.anchors.last() {
            Some(anchor) => format!("hunk {} (@@ {anchor})", index + 1),
            None => format!("hunk {}", index + 1),
        };
        let (start, len, new) =
            locate(&lines, hunk, eol).map_err(|err| format!("{name}: {err}"))?;
        edits.push((start, len, new, index + 1));
    }
    edits.sort_by_key(|(start, ..)| *start);
    for pair in edits.windows(2) {
        if pair[0].0 + pair[0].1 > pair[1].0 {
            return Err(format!(
                "hunks {} and {} overlap at line {}; send them as one hunk",
                pair[0].3,
                pair[1].3,
                pair[1].0 + 1
            ));
        }
    }

    let first = edits.first().map_or(1, |(start, ..)| start + 1);
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut at = 0;
    for (start, len, new, _) in edits {
        out.extend(lines[at..start].iter().map(|line| line.to_string()));
        out.extend(new);
        at = start + len;
    }
    out.extend(lines[at..].iter().map(|line| line.to_string()));
    let mut text = out.join("\n");
    if newline_at_end && !out.is_empty() {
        text.push('\n');
    }
    Ok((text, first))
}

#[derive(Deserialize)]
struct ApplyPatchArgs {
    patch: String,
}

/// `apply_patch`: see the module docs.
pub struct ApplyPatchTool;

impl ApplyPatchTool {
    async fn apply_file(file: &FilePatch, ctx: &ToolContext) -> Result<String, String> {
        let path = resolve_path(ctx, &file.path);
        let shown = &file.path;
        let write = |target: std::path::PathBuf, text: String| async move {
            if let Some(parent) = target.parent()
                && !parent.as_os_str().is_empty()
            {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
            }
            tokio::fs::write(&target, text)
                .await
                .map_err(|err| format!("failed to write {}: {err}", target.display()))
        };
        match &file.change {
            Change::Add(content) => {
                if path.exists() {
                    return Err(format!(
                        "{shown} exists; use `*** Update File:` or write_file"
                    ));
                }
                write(path, content.clone()).await?;
                Ok(format!("Created {shown}"))
            }
            Change::Delete => {
                tokio::fs::remove_file(&path)
                    .await
                    .map_err(|err| format!("failed to delete {shown}: {err}"))?;
                Ok(format!("Deleted {shown}"))
            }
            Change::Update { hunks, move_to } => {
                let content = tokio::fs::read_to_string(&path)
                    .await
                    .map_err(|err| format!("failed to read {shown}: {err}"))?;
                let (updated, line) =
                    apply(&content, hunks).map_err(|err| format!("{shown}: {err}"))?;
                let count = match hunks.len() {
                    1 => "1 hunk".to_string(),
                    n => format!("{n} hunks"),
                };
                match move_to {
                    Some(to) => {
                        write(resolve_path(ctx, to), updated).await?;
                        tokio::fs::remove_file(&path)
                            .await
                            .map_err(|err| format!("failed to remove {shown}: {err}"))?;
                        Ok(format!("Moved {shown} to {to}: {count} (line {line})"))
                    }
                    None => {
                        write(path, updated).await?;
                        Ok(format!("Edited {shown}: {count} (line {line})"))
                    }
                }
            }
        }
    }
}

#[async_trait]
impl Tool for ApplyPatchTool {
    fn name(&self) -> &str {
        APPLY_PATCH_TOOL_NAME
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "patch": { "type": "string" } },
            "required": ["patch"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::Edit
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: ApplyPatchArgs = parse_args(self.name(), args)?;
        let files = parse(&args.patch).map_err(|message| ToolError::InvalidArgs {
            tool: self.name().to_string(),
            message,
        })?;

        let mut done = Vec::new();
        let mut failed = Vec::new();
        for file in &files {
            match Self::apply_file(file, ctx).await {
                Ok(line) => done.push(line),
                Err(err) => failed.push(err),
            }
        }
        if failed.is_empty() {
            return Ok(ToolOutput::ok(done.join("\n")));
        }
        let mut message = failed.join("\n");
        message.push_str(if done.is_empty() {
            "\nNothing was changed."
        } else {
            "\nThose files were not changed. Resend only their hunks. The rest applied:\n"
        });
        message.push_str(&done.join("\n"));
        Ok(ToolOutput::error(message))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GO: &str = "package store\n\nfunc Ship(n int) int {\n\tif n < 0 {\n\t\treturn 0\n\t}\n\treturn n\n}\n\nfunc Audit(n int) int {\n\tif n < 0 {\n\t\treturn 0\n\t}\n\treturn n\n}\n";

    fn hunks(patch: &str) -> Vec<Hunk> {
        match parse(&format!("*** Update File: f\n{patch}"))
            .unwrap()
            .remove(0)
            .change
        {
            Change::Update { hunks, .. } => hunks,
            other => panic!("expected an update, got {other:?}"),
        }
    }

    fn patched(content: &str, patch: &str) -> Result<String, String> {
        apply(content, &hunks(patch)).map(|(text, _)| text)
    }

    #[test]
    fn hunks_under_an_anchor_land_in_that_function_only() {
        let out = patched(
            GO,
            "@@ func Audit(n int) int {\n \tif n < 0 {\n-\t\treturn 0\n+\t\treturn -1\n \t}\n",
        )
        .unwrap();
        assert_eq!(out.matches("return -1").count(), 1);
        assert!(out.find("return -1").unwrap() > out.find("func Audit").unwrap());
        assert!(out.ends_with("\treturn n\n}\n"));
    }

    #[test]
    fn an_ambiguous_hunk_is_refused_with_its_line_numbers() {
        let err = patched(GO, "-\t\treturn 0\n+\t\treturn -1\n").unwrap_err();
        assert!(err.contains("matches at lines 5, 12"), "{err}");
        let err = patched(GO, "@@ if n < 0 {\n-\t\treturn 0\n+\t\treturn 1\n").unwrap_err();
        assert!(err.contains("matches lines 4, 11"), "{err}");
    }

    #[test]
    fn hunks_match_the_original_file_in_any_order() {
        let out = patched(
            GO,
            "@@ func Audit(n int) int {\n-\treturn n\n+\treturn n + 1\n@@ func Ship(n int) int {\n-\treturn n\n+\treturn n - 1\n",
        )
        .unwrap();
        assert!(out.contains("\treturn n - 1\n}\n\nfunc Audit"), "{out}");
        assert!(out.ends_with("\treturn n + 1\n}\n"), "{out}");
    }

    #[test]
    fn overlapping_hunks_change_nothing() {
        let err = patched(
            GO,
            "@@ func Ship(n int) int {\n \tif n < 0 {\n-\t\treturn 0\n+\t\treturn 1\n@@ func Ship(n int) int {\n-\t\treturn 0\n+\t\treturn 2\n",
        )
        .unwrap_err();
        assert!(err.contains("overlap"), "{err}");
    }

    /// The slip the Go traces show: every line sent one tab too deep. The
    /// hunk still lands, and its new lines come back out at the file's depth.
    #[test]
    fn a_hunk_one_indent_off_matches_and_is_moved_back() {
        let out = patched(
            GO,
            "@@ func Ship(n int) int {\n \t\tif n < 0 {\n-\t\t\treturn 0\n+\t\t\treturn -1\n+\n \t\t}\n",
        )
        .unwrap();
        assert!(
            out.contains("\tif n < 0 {\n\t\treturn -1\n\n\t}\n"),
            "{out:?}"
        );
    }

    #[test]
    fn a_context_line_without_its_leading_space_is_still_context() {
        let out = patched(
            GO,
            "func Ship(n int) int {\n-\tif n < 0 {\n+\tif n <= 0 {\n",
        )
        .unwrap();
        assert!(out.contains("func Ship(n int) int {\n\tif n <= 0 {\n"));
        let out = patched("def f():\n    a = 1\n", "    a = 1\n+    b = 2\n").unwrap();
        assert_eq!(out, "def f():\n    a = 1\n    b = 2\n");
    }

    #[test]
    fn a_miss_quotes_the_lines_the_file_has() {
        let err = patched(
            GO,
            "@@ func Audit(n int) int {\n \tif n < 0 {\n-\t\treturn nil\n+\t\treturn 1\n \t}\n",
        )
        .unwrap_err();
        assert!(err.starts_with("hunk 1 (@@ func Audit(n int) int {): lines not found."));
        assert!(err.contains("Line 12 is not your \"return nil\"."), "{err}");
        assert!(
            err.contains(
                "lines 8-13:\n}\n\nfunc Audit(n int) int {\n\tif n < 0 {\n\t\treturn 0\n\t}"
            ),
            "{err}"
        );
    }

    #[test]
    fn additions_alone_go_after_the_anchor_or_at_the_end() {
        let out = patched("a\nb\n", "@@ a\n+x\n").unwrap();
        assert_eq!(out, "a\nx\nb\n");
        let out = patched("a\nb\n", "+x\n").unwrap();
        assert_eq!(out, "a\nb\nx\n");
        let out = patched("a\r\nb\r\n", "@@ a\n+x\n").unwrap();
        assert_eq!(out, "a\r\nx\r\nb\r\n");
    }

    #[test]
    fn a_unified_diff_is_read_too() {
        let files = parse(
            "diff --git a/src/x.rs b/src/x.rs\nindex 1..2 100644\n--- a/src/x.rs\n+++ b/src/x.rs\n@@ -1,2 +1,2 @@ fn main\n a\n-b\n+c\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+hi\n",
        )
        .unwrap();
        assert_eq!(files[0].path, "src/x.rs");
        let Change::Update { hunks, .. } = &files[0].change else {
            panic!("{files:?}");
        };
        assert!(hunks[0].anchors.is_empty());
        assert_eq!(hunks[0].old(), ["a", "b"]);
        assert_eq!(
            files[1],
            FilePatch::new("new.txt", Change::Add("hi\n".into()))
        );
    }

    #[test]
    fn a_patch_with_no_file_or_no_change_is_refused() {
        assert!(parse(" a\n-b\n").unwrap_err().contains("line 1"));
        assert!(
            parse("*** Update File: f\n a\n")
                .unwrap_err()
                .contains("no `+`")
        );
        assert!(parse("*** Begin Patch\n*** End Patch").is_err());
    }

    #[test]
    fn surfaces_get_the_files_and_both_sides() {
        let args = json!({ "patch": "*** Begin Patch\n*** Update File: a.go\n@@ func A\n x\n-y\n+z\n*** Update File: b.go\n*** Move to: c.go\n-q\n*** Add File: d.go\n+new\n*** End Patch" });
        assert_eq!(targets(&args), ["a.go", "c.go", "b.go", "d.go"]);
        assert_eq!(label(&args).as_deref(), Some("a.go +2"));
        assert_eq!(
            sides(&args),
            Some(("x\ny\nq\n".into(), "x\nz\nnew\n".into()))
        );
        assert_eq!(label(&json!({ "path": "a.go" })), None);
    }

    #[tokio::test]
    async fn files_apply_independently_and_a_failed_one_is_untouched() {
        let dir = std::env::temp_dir().join(format!("wizard-patch-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(dir.join("b.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(dir.join("old.txt"), "bye\n").unwrap();
        let ctx = ToolContext::new(&dir);

        let out = ApplyPatchTool
            .execute(
                json!({ "patch": "*** Update File: a.txt\n-two\n+2\n*** Update File: b.txt\n-one\n+1\n@@\n-tree\n+3\n*** Add File: sub/new.txt\n+hello\n*** Delete File: old.txt\n" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(
            out.content.contains("b.txt: hunk 2: lines not found"),
            "{}",
            out.content
        );
        assert!(
            out.content.contains("Edited a.txt: 1 hunk (line 2)"),
            "{}",
            out.content
        );
        let read = |name: &str| std::fs::read_to_string(dir.join(name)).unwrap();
        assert_eq!(read("a.txt"), "one\n2\n");
        assert_eq!(read("b.txt"), "one\ntwo\nthree\n");
        assert_eq!(read("sub/new.txt"), "hello\n");
        assert!(!dir.join("old.txt").exists());

        let out = ApplyPatchTool
            .execute(
                json!({ "patch": "*** Update File: b.txt\n*** Move to: c.txt\n-one\n+1\n" }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out.content, "Moved b.txt to c.txt: 1 hunk (line 1)");
        assert_eq!(read("c.txt"), "1\ntwo\nthree\n");
        assert!(!dir.join("b.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
