//! `EnterWorktree` and `ExitWorktree`.
//!
//! Entering makes a git worktree under `.wizard/worktrees/<name>` on a new
//! branch `worktree-<name>` (from the current `HEAD`, not from `origin`) and
//! points the session at it: [`ToolContext::in_session_worktree`] swaps `cwd`
//! on every later tool call, so `execute`, the file tools and subagents all
//! work there. The system prompt still names the directory the session
//! started in. Exiting restores the original directory and either keeps the
//! worktree or removes it, refusing to remove one with uncommitted files or
//! commits not on the original branch unless told to discard them.
//!
//! Only worktrees this tool made can be removed by it. One entered by `path`
//! can be left with `keep` and never deleted.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{invalid, str_arg};
use crate::tools::shell::run_command;
use crate::tools::{Tool, ToolAccess, ToolContext, ToolError, ToolOutput};

const GIT_TIMEOUT: Duration = Duration::from_secs(60);

/// Where worktrees go, relative to the repository root.
const WORKTREE_DIR: &str = ".wizard/worktrees";

pub fn tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(EnterWorktreeTool), Arc::new(ExitWorktreeTool)]
}

/// The worktree a session is working in.
#[derive(Debug, Clone)]
pub struct WorktreeSession {
    /// The worktree's directory.
    pub path: PathBuf,
    /// Its branch, when this tool made it.
    pub branch: Option<String>,
    /// Where the session was before the first `EnterWorktree`.
    pub original_cwd: PathBuf,
    /// The repository the worktree belongs to.
    pub repo_root: PathBuf,
    /// `HEAD` of the repository when the worktree was made, to count the
    /// commits made since.
    pub base_commit: Option<String>,
    /// Made by `EnterWorktree` (`name`) rather than entered by `path`.
    pub created: bool,
}

/// Run git in `dir`; stdout on success, git's own words on failure.
async fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(dir);
    let result = run_command("git", cmd, GIT_TIMEOUT)
        .await
        .map_err(|err| err.to_string())?;
    if result.code == Some(0) {
        Ok(result.stdout.trim_end().to_string())
    } else {
        let stderr = result.stderr.trim();
        Err(if stderr.is_empty() {
            result.stdout.trim().to_string()
        } else {
            stderr.to_string()
        })
    }
}

/// Check a worktree name the way Claude Code does: `/`-separated segments of
/// letters, digits, dots, underscores and dashes, 64 characters at most.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        })
}

fn random_name() -> String {
    format!("wt-{}", &uuid::Uuid::new_v4().simple().to_string()[..8])
}

/// Keep the worktrees out of the parent repository's `git status`.
async fn exclude_worktree_dir(repo_root: &Path) {
    let Ok(rel) = git(repo_root, &["rev-parse", "--git-path", "info/exclude"]).await else {
        return;
    };
    let exclude = repo_root.join(rel);
    let line = format!("/{WORKTREE_DIR}/");
    let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == line) {
        return;
    }
    if let Some(parent) = exclude.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let sep = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let _ = std::fs::write(&exclude, format!("{existing}{sep}{line}\n"));
}

/// `EnterWorktree`.
pub struct EnterWorktreeTool;

#[async_trait]
impl Tool for EnterWorktreeTool {
    fn name(&self) -> &str {
        "EnterWorktree"
    }

    fn description(&self) -> &str {
        "Make an isolated git worktree on a new branch and move this session's working \
         directory into it. Use only when the user or project instructions ask for a worktree, \
         not for ordinary branching. `name` makes a new one (random name if omitted); `path` \
         enters one that already exists. Leave with ExitWorktree."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Name for a new worktree. Each /-separated segment may hold only letters, digits, dots, underscores and dashes; 64 characters at most. Random if omitted. Not with path." },
                "path": { "type": "string", "description": "Path of an existing worktree of this repository to switch into. Not with name." }
            }
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::Execute
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let name = str_arg(&args, "name");
        let path = str_arg(&args, "path");
        if name.is_some() && path.is_some() {
            return Ok(invalid(self.name(), "name and path are mutually exclusive"));
        }
        if let Some(name) = name
            && !valid_name(name)
        {
            return Ok(ToolOutput::error(format!(
                "Invalid worktree name \"{name}\": each \"/\"-separated segment must be non-empty \
                 and contain only letters, digits, dots, underscores, and dashes"
            )));
        }
        // A path that is not there is its own answer, before any question about
        // repositories; the wording is the real tool's.
        if let Some(path) = path {
            let wanted = crate::tools::resolve_path(ctx, path);
            if let Err(err) = std::fs::canonicalize(&wanted) {
                let reason = if err.kind() == std::io::ErrorKind::NotFound {
                    format!("ENOENT: no such file or directory, realpath '{path}'")
                } else {
                    err.to_string()
                };
                return Ok(ToolOutput::error(format!(
                    "Cannot enter worktree: {path}: {reason}"
                )));
            }
        }
        let current = ctx.worktree.lock().expect("worktree lock poisoned").clone();
        let Some(repo_root) = (match git(&ctx.cwd, &["rev-parse", "--show-toplevel"]).await {
            Ok(root) => Some(PathBuf::from(root)),
            Err(_) => None,
        }) else {
            return Ok(ToolOutput::error(format!(
                "Cannot create a worktree: {} is not inside a git repository.",
                ctx.cwd.display()
            )));
        };

        if let Some(path) = path {
            return self.enter_existing(ctx, current, &repo_root, path).await;
        }
        if current.is_some() {
            return Ok(ToolOutput::error(
                "Already in a worktree session. ExitWorktree first, or pass `path` to switch \
                 into another existing worktree.",
            ));
        }

        let name = name.map_or_else(random_name, str::to_string);
        let branch = format!("worktree-{}", name.replace('/', "-"));
        let target = repo_root.join(WORKTREE_DIR).join(&name);
        exclude_worktree_dir(&repo_root).await;
        let base_commit = git(&repo_root, &["rev-parse", "HEAD"]).await.ok();
        let target_str = target.display().to_string();
        if let Err(err) = git(
            &repo_root,
            &["worktree", "add", "-b", &branch, &target_str, "HEAD"],
        )
        .await
        {
            return Ok(ToolOutput::error(format!(
                "Cannot create the worktree: {err}"
            )));
        }
        let original_cwd = ctx.cwd.clone();
        *ctx.worktree.lock().expect("worktree lock poisoned") = Some(WorktreeSession {
            path: target.clone(),
            branch: Some(branch.clone()),
            original_cwd,
            repo_root,
            base_commit,
            created: true,
        });
        Ok(ToolOutput::ok(format!(
            "Created worktree at {} on branch {branch}. This session now works there; \
             ExitWorktree returns to {}.",
            target.display(),
            ctx.cwd.display()
        )))
    }
}

impl EnterWorktreeTool {
    async fn enter_existing(
        &self,
        ctx: &ToolContext,
        current: Option<WorktreeSession>,
        repo_root: &Path,
        path: &str,
    ) -> Result<ToolOutput, ToolError> {
        let wanted = crate::tools::resolve_path(ctx, path);
        let target = match std::fs::canonicalize(&wanted) {
            Ok(target) => target,
            Err(err) => {
                return Ok(ToolOutput::error(format!(
                    "Cannot enter worktree: {}: {err}",
                    wanted.display()
                )));
            }
        };
        let listed = git(repo_root, &["worktree", "list", "--porcelain"])
            .await
            .unwrap_or_default();
        let known = listed
            .lines()
            .filter_map(|line| line.strip_prefix("worktree "))
            .any(|p| std::fs::canonicalize(p).is_ok_and(|p| p == target));
        if !known {
            return Ok(ToolOutput::error(format!(
                "Cannot enter worktree: {} is not in `git worktree list` for this repository.",
                target.display()
            )));
        }
        let original_cwd = current
            .as_ref()
            .map_or_else(|| ctx.cwd.clone(), |s| s.original_cwd.clone());
        *ctx.worktree.lock().expect("worktree lock poisoned") = Some(WorktreeSession {
            path: target.clone(),
            branch: None,
            original_cwd,
            repo_root: repo_root.to_path_buf(),
            base_commit: None,
            created: false,
        });
        Ok(ToolOutput::ok(format!(
            "Entered existing worktree {}. ExitWorktree with action keep returns to the \
             original directory; it will not be removed.",
            target.display()
        )))
    }
}

/// `ExitWorktree`.
pub struct ExitWorktreeTool;

#[async_trait]
impl Tool for ExitWorktreeTool {
    fn name(&self) -> &str {
        "ExitWorktree"
    }

    fn description(&self) -> &str {
        "Leave the worktree EnterWorktree put this session in and return to the original \
         directory. action keep leaves the worktree and branch on disk; remove deletes both, and \
         refuses when there are uncommitted files or commits not on the original branch unless \
         discard_changes is true. Only for worktrees EnterWorktree made this session; call it \
         when the user asks."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["keep", "remove"], "description": "keep leaves the worktree and branch on disk; remove deletes both." },
                "discard_changes": { "type": "boolean", "description": "Required true when action is remove and the worktree has uncommitted files or unmerged commits." }
            },
            "required": ["action"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::Execute
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let session = ctx.worktree.lock().expect("worktree lock poisoned").clone();
        let Some(session) = session else {
            return Ok(invalid(
                self.name(),
                "No-op: there is no active EnterWorktree session to exit. This tool only \
                 operates on worktrees created by EnterWorktree in the current session — it \
                 will not touch worktrees created manually or in a previous session. No \
                 filesystem changes were made.",
            ));
        };
        let action = str_arg(&args, "action");
        if !matches!(action, Some("keep" | "remove")) {
            return Ok(invalid(
                self.name(),
                "action must be \"keep\" or \"remove\"",
            ));
        }
        let discard = args
            .get("discard_changes")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        if action == Some("remove") {
            if !session.created {
                return Ok(invalid(
                    self.name(),
                    "This worktree was entered by path, not created by EnterWorktree, so it \
                     is not removed here. Use action keep.",
                ));
            }
            let dirty = git(&session.path, &["status", "--porcelain"])
                .await
                .map(|out| out.lines().count())
                .unwrap_or(0);
            let ahead = match &session.base_commit {
                Some(base) => git(
                    &session.path,
                    &["rev-list", "--count", &format!("{base}..HEAD")],
                )
                .await
                .ok()
                .and_then(|n| n.trim().parse::<usize>().ok())
                .unwrap_or(0),
                None => 0,
            };
            if (dirty > 0 || ahead > 0) && !discard {
                return Ok(ToolOutput::error(format!(
                    "Worktree {} has {dirty} uncommitted file(s) and {ahead} commit(s) not on \
                     the original branch. Removing it would lose them. Confirm with the user, \
                     then call again with discard_changes: true, or use action keep.",
                    session.path.display()
                )));
            }
            let path = session.path.display().to_string();
            if let Err(err) = git(
                &session.repo_root,
                &["worktree", "remove", "--force", &path],
            )
            .await
            {
                return Ok(ToolOutput::error(format!(
                    "Could not remove the worktree: {err}"
                )));
            }
            if let Some(branch) = &session.branch {
                let _ = git(&session.repo_root, &["branch", "-D", branch]).await;
            }
        }

        *ctx.worktree.lock().expect("worktree lock poisoned") = None;
        Ok(ToolOutput::ok(match action {
            Some("remove") => format!(
                "Removed worktree {} and its branch. The session is back in {}.",
                session.path.display(),
                session.original_cwd.display()
            ),
            _ => format!(
                "Left worktree {} in place{}. The session is back in {}.",
                session.path.display(),
                session
                    .branch
                    .as_ref()
                    .map(|b| format!(" (branch {b})"))
                    .unwrap_or_default(),
                session.original_cwd.display()
            ),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn have_git() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok()
    }

    /// A repository with one commit.
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}: {out:?}");
        };
        run(&["init", "-q", "-b", "main"]);
        std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "init"]);
        dir
    }

    #[test]
    fn names_follow_claude_codes_rule() {
        assert!(valid_name("feature/login-2"));
        assert!(valid_name("a.b_c"));
        assert!(!valid_name("bad name!"));
        assert!(!valid_name(""));
        assert!(!valid_name("a//b"));
        assert!(!valid_name("../x"));
        assert!(!valid_name(&"x".repeat(65)));
    }

    #[tokio::test]
    async fn enter_moves_the_session_and_exit_remove_cleans_up() {
        if !have_git() {
            return;
        }
        let dir = repo();
        let ctx = ToolContext::new(dir.path());

        let out = EnterWorktreeTool
            .execute(json!({ "name": "probe" }), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let wt = dir.path().join(".wizard/worktrees/probe");
        assert!(wt.join("a.txt").exists());
        assert_eq!(ctx.in_session_worktree().cwd, wt);

        // A tool that resolves against cwd now works in the worktree.
        let read = crate::tools::file::ReadFileTool
            .execute(json!({ "path": "a.txt" }), &ctx.in_session_worktree())
            .await
            .unwrap();
        assert_eq!(read.content, "hello");

        // The parent repo does not see the worktree directory as untracked.
        let status = git(dir.path(), &["status", "--porcelain"]).await.unwrap();
        assert_eq!(status, "", "worktree dir is excluded");

        let exit = ExitWorktreeTool
            .execute(json!({ "action": "remove" }), &ctx)
            .await
            .unwrap();
        assert!(!exit.is_error, "{}", exit.content);
        assert!(!wt.exists());
        assert_eq!(ctx.in_session_worktree().cwd, dir.path());
        let branches = git(dir.path(), &["branch", "--list", "worktree-probe"])
            .await
            .unwrap();
        assert_eq!(branches, "");
    }

    #[tokio::test]
    async fn remove_refuses_to_lose_work_without_discard_changes() {
        if !have_git() {
            return;
        }
        let dir = repo();
        let ctx = ToolContext::new(dir.path());
        EnterWorktreeTool
            .execute(json!({ "name": "w" }), &ctx)
            .await
            .unwrap();
        let wt = dir.path().join(".wizard/worktrees/w");
        std::fs::write(wt.join("new.txt"), "x").unwrap();

        let refused = ExitWorktreeTool
            .execute(json!({ "action": "remove" }), &ctx)
            .await
            .unwrap();
        assert!(refused.is_error);
        assert!(
            refused.content.contains("1 uncommitted file(s)"),
            "{}",
            refused.content
        );
        assert!(wt.exists(), "nothing was removed");
        assert_eq!(ctx.in_session_worktree().cwd, wt, "still in the worktree");

        let kept = ExitWorktreeTool
            .execute(json!({ "action": "keep" }), &ctx)
            .await
            .unwrap();
        assert!(!kept.is_error);
        assert!(wt.exists());
        assert_eq!(ctx.in_session_worktree().cwd, dir.path());
    }

    #[tokio::test]
    async fn exit_outside_a_session_is_the_same_no_op_claude_code_reports() {
        let ctx = ToolContext::new(std::env::temp_dir());
        let out = ExitWorktreeTool
            .execute(json!({ "action": "keep" }), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(
            out.content
                .starts_with("Tool ExitWorktree input is invalid: No-op: there is no active EnterWorktree session to exit.")
        );
    }

    #[tokio::test]
    async fn bad_names_a_missing_repo_and_a_second_session_are_refused() {
        let ctx = ToolContext::new(std::env::temp_dir());
        let bad = EnterWorktreeTool
            .execute(json!({ "name": "bad name!" }), &ctx)
            .await
            .unwrap();
        assert!(
            bad.content
                .starts_with("Invalid worktree name \"bad name!\"")
        );
        let gone = EnterWorktreeTool
            .execute(json!({ "path": "/nonexistent" }), &ctx)
            .await
            .unwrap();
        assert_eq!(
            gone.content,
            "Cannot enter worktree: /nonexistent: ENOENT: no such file or directory, realpath '/nonexistent'"
        );

        if !have_git() {
            return;
        }
        let not_repo = tempfile::tempdir().unwrap();
        let out = EnterWorktreeTool
            .execute(json!({}), &ToolContext::new(not_repo.path()))
            .await
            .unwrap();
        assert!(
            out.content.contains("is not inside a git repository"),
            "{}",
            out.content
        );

        let dir = repo();
        let ctx = ToolContext::new(dir.path());
        EnterWorktreeTool
            .execute(json!({ "name": "one" }), &ctx)
            .await
            .unwrap();
        let again = EnterWorktreeTool
            .execute(json!({ "name": "two" }), &ctx.in_session_worktree())
            .await
            .unwrap();
        assert!(again.is_error);
        assert!(again.content.starts_with("Already in a worktree session"));
    }

    #[tokio::test]
    async fn entering_an_existing_worktree_by_path_cannot_be_removed() {
        if !have_git() {
            return;
        }
        let dir = repo();
        let other = dir.path().join("elsewhere");
        git(
            dir.path(),
            &[
                "worktree",
                "add",
                "-b",
                "side",
                &other.display().to_string(),
                "HEAD",
            ],
        )
        .await
        .unwrap();
        let ctx = ToolContext::new(dir.path());
        let out = EnterWorktreeTool
            .execute(json!({ "path": other.display().to_string() }), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let remove = ExitWorktreeTool
            .execute(json!({ "action": "remove" }), &ctx)
            .await
            .unwrap();
        assert!(remove.is_error);
        assert!(other.exists());

        let outside = EnterWorktreeTool
            .execute(
                json!({ "path": std::env::temp_dir().display().to_string() }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(outside.is_error);
    }
}
