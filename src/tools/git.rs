//! Native git tools: `git_status` and `git_diff` (shelling out to `git`).

use std::ffi::OsStr;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;

use super::shell::{CommandResult, render_command_result, run_command};
use super::{
    MAX_DIFF_BYTES, MAX_ERROR_BYTES, MAX_LISTING_BYTES, Tool, ToolAccess, ToolContext, ToolError,
    ToolOutput, parse_args, truncate_output,
};

/// Timeout for git subprocesses. Status and diff are local operations, so
/// anything slower than this indicates a wedged repository.
const GIT_TIMEOUT: Duration = Duration::from_secs(30);

/// Run `git <args>` in the project root.
async fn run_git<I, S>(tool: &str, ctx: &ToolContext, args: I) -> Result<CommandResult, ToolError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new("git");
    command.args(args).current_dir(&ctx.cwd);
    run_command(tool, command, GIT_TIMEOUT).await
}

/// Model-facing error output for a failed git invocation.
fn git_failure(result: &CommandResult, fallback: &str) -> ToolOutput {
    if result.timed_out.is_some() {
        return render_command_result(result);
    }
    let stderr = result.stderr.trim_end();
    let detail = if stderr.is_empty() { fallback } else { stderr };
    ToolOutput::error(truncate_output(detail.to_string(), MAX_ERROR_BYTES))
}

/// `git_status` — working tree status (`git status --porcelain=v1 -b`).
pub struct GitStatusTool;

#[async_trait]
impl Tool for GitStatusTool {
    fn name(&self) -> &str {
        "git_status"
    }

    fn description(&self) -> &str {
        "Show the git working tree status of the project (branch, staged, modified, and untracked files)."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let _ = args; // no arguments

        let result = run_git(self.name(), ctx, ["status", "--porcelain=v1", "-b"]).await?;
        if result.code != Some(0) {
            return Ok(git_failure(&result, "git status failed"));
        }

        let status = result.stdout.trim_end();
        // Porcelain v1 with `-b` always emits a `## branch` header first;
        // a header-only output means a clean tree.
        let content = if status.lines().count() <= 1 {
            format!("{status}\n(clean working tree)")
        } else {
            status.to_string()
        };
        Ok(ToolOutput::ok(truncate_output(content, MAX_LISTING_BYTES)))
    }
}

/// Arguments for [`GitDiffTool`].
#[derive(Debug, Deserialize)]
pub struct GitDiffArgs {
    /// Diff the index (staged changes) instead of the working tree.
    #[serde(default)]
    pub staged: bool,
    /// Limit the diff to a single path.
    #[serde(default)]
    pub path: Option<String>,
}

/// `git_diff` — staged or unstaged diff. Also backs the TUI diff sidebar.
pub struct GitDiffTool;

#[async_trait]
impl Tool for GitDiffTool {
    fn name(&self) -> &str {
        "git_diff"
    }

    fn description(&self) -> &str {
        "Show the git diff of the project: unstaged changes by default, staged with staged=true, optionally limited to one path."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "staged": { "type": "boolean", "description": "Diff staged changes instead of the working tree" },
                "path": { "type": "string", "description": "Limit the diff to this path" }
            }
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: GitDiffArgs = parse_args(self.name(), args)?;

        let mut argv: Vec<String> = vec!["diff".to_string()];
        if args.staged {
            argv.push("--cached".to_string());
        }
        if let Some(path) = args.path {
            argv.push("--".to_string());
            argv.push(path);
        }

        let result = run_git(self.name(), ctx, &argv).await?;
        if result.code != Some(0) {
            return Ok(git_failure(&result, "git diff failed"));
        }

        let diff = result.stdout.trim_end();
        if diff.is_empty() {
            Ok(ToolOutput::ok("No changes."))
        } else {
            Ok(ToolOutput::ok(truncate_output(
                diff.to_string(),
                MAX_DIFF_BYTES,
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

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

    /// Run `git <args>` for test setup, isolated from user/system git config.
    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git available");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn commit_all(dir: &Path, message: &str) {
        git(dir, &["add", "-A"]);
        git(
            dir,
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-q",
                "-m",
                message,
            ],
        );
    }

    #[test]
    fn git_failure_prefers_stderr_over_fallback() {
        let result = CommandResult {
            stdout: String::new(),
            stderr: "fatal: boom\n".to_string(),
            code: Some(128),
            timed_out: None,
        };
        assert_eq!(git_failure(&result, "fallback").content, "fatal: boom");

        let silent = CommandResult {
            stdout: String::new(),
            stderr: String::new(),
            code: Some(1),
            timed_out: None,
        };
        let out = git_failure(&silent, "git status failed");
        assert!(out.is_error);
        assert_eq!(out.content, "git status failed");
    }

    #[test]
    fn git_failure_renders_timeout_with_partial_output() {
        let result = CommandResult {
            stdout: "partial diff\n".to_string(),
            stderr: String::new(),
            code: None,
            timed_out: Some(30),
        };
        let out = git_failure(&result, "git diff failed");
        assert!(out.is_error);
        assert!(out.content.contains("partial diff"), "{}", out.content);
        assert!(
            out.content.contains("timed out after 30s"),
            "{}",
            out.content
        );
    }

    #[test]
    fn diff_args_default_to_unstaged_whole_tree() {
        let args: GitDiffArgs = parse_args("git_diff", json!({})).unwrap();
        assert!(!args.staged);
        assert!(args.path.is_none());
    }

    #[tokio::test]
    async fn status_reports_clean_tree() {
        let tmp = TempDir::new();
        git(&tmp.0, &["init", "-q"]);
        let out = GitStatusTool.execute(json!({}), &tmp.ctx()).await.unwrap();
        assert!(!out.is_error);
        assert!(out.content.starts_with("##"), "{}", out.content);
        assert!(
            out.content.contains("(clean working tree)"),
            "{}",
            out.content
        );
    }

    #[tokio::test]
    async fn status_lists_untracked_files() {
        let tmp = TempDir::new();
        git(&tmp.0, &["init", "-q"]);
        std::fs::write(tmp.0.join("new.txt"), "hello\n").unwrap();
        let out = GitStatusTool.execute(json!({}), &tmp.ctx()).await.unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("?? new.txt"), "{}", out.content);
        assert!(!out.content.contains("(clean working tree)"));
    }

    #[tokio::test]
    async fn status_outside_a_repository_is_an_error() {
        let tmp = TempDir::new();
        let out = GitStatusTool.execute(json!({}), &tmp.ctx()).await.unwrap();
        assert!(out.is_error);
        assert!(!out.content.is_empty());
    }

    #[tokio::test]
    async fn diff_reports_no_changes_in_fresh_repo() {
        let tmp = TempDir::new();
        git(&tmp.0, &["init", "-q"]);
        let out = GitDiffTool.execute(json!({}), &tmp.ctx()).await.unwrap();
        assert!(!out.is_error);
        assert_eq!(out.content, "No changes.");
    }

    #[tokio::test]
    async fn diff_distinguishes_unstaged_staged_and_paths() {
        let tmp = TempDir::new();
        git(&tmp.0, &["init", "-q"]);
        std::fs::write(tmp.0.join("f.txt"), "old\n").unwrap();
        commit_all(&tmp.0, "init");
        std::fs::write(tmp.0.join("f.txt"), "new\n").unwrap();

        let unstaged = GitDiffTool.execute(json!({}), &tmp.ctx()).await.unwrap();
        assert!(unstaged.content.contains("-old"), "{}", unstaged.content);
        assert!(unstaged.content.contains("+new"), "{}", unstaged.content);

        // Not staged yet, so the staged diff is empty.
        let staged = GitDiffTool
            .execute(json!({ "staged": true }), &tmp.ctx())
            .await
            .unwrap();
        assert_eq!(staged.content, "No changes.");

        git(&tmp.0, &["add", "f.txt"]);
        let staged = GitDiffTool
            .execute(json!({ "staged": true }), &tmp.ctx())
            .await
            .unwrap();
        assert!(staged.content.contains("+new"), "{}", staged.content);

        // A path filter that matches nothing yields no changes.
        let other = GitDiffTool
            .execute(json!({ "staged": true, "path": "other.txt" }), &tmp.ctx())
            .await
            .unwrap();
        assert_eq!(other.content, "No changes.");
    }

    /// A diff of a generated file used to be able to spend 30 KB of the
    /// window, and then be re-sent on every following step. It gets a diff's
    /// budget now, not the one an arbitrary command's stdout gets.
    #[tokio::test]
    async fn a_huge_diff_is_cut_to_the_diff_budget() {
        let tmp = TempDir::new();
        git(&tmp.0, &["init", "-q"]);
        std::fs::write(tmp.0.join("f.txt"), "old\n").unwrap();
        commit_all(&tmp.0, "init");
        let generated: String = (0..20_000).map(|line| format!("line {line}\n")).collect();
        assert!(generated.len() > super::super::MAX_OUTPUT_BYTES);
        std::fs::write(tmp.0.join("f.txt"), generated).unwrap();

        let out = GitDiffTool.execute(json!({}), &tmp.ctx()).await.unwrap();
        assert!(
            out.content.len() <= MAX_DIFF_BYTES,
            "{} bytes",
            out.content.len()
        );
        assert!(
            out.content.contains("[output truncated]"),
            "{}",
            out.content
        );
        // Head and tail framing survives the smaller budget: the first hunk
        // header and the last lines are both still there.
        assert!(out.content.contains("--- a/f.txt"), "{}", out.content);
        assert!(out.content.contains("line 19999"), "{}", out.content);
    }
}
