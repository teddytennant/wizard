//! The bundled Lua plugins, exercised as tools.
//!
//! Most of this file is `src/tools/git.rs`'s test module, moved and pointed at
//! the Lua implementation. That is the point: the port is only worth anything
//! if the tests that held the Rust tool to its behaviour still pass against
//! the Lua one, so the assertions are the same assertions — the same porcelain
//! flags, the same "(clean working tree)", the same "No changes.", the same
//! diff budget — and only the thing under them changed.
//!
//! Two shapes of test, because the tool has two halves.
//!
//! The behavioural half runs **real git in a temp directory** through the real
//! [`WizardHost`], so what is being tested is the whole path: the Lua chunk,
//! the `ctx` table, `wizard.process.exec`, the shell runner, the process, and
//! the string that comes back. A mock at any layer of that would test the
//! mock.
//!
//! The rendering half cannot be reached that way. A git that exits non-zero
//! and says nothing, and a git that outlives its budget, are both states this
//! plugin has to render and neither is a state a test can arrange by running
//! git. Those go through [`ScriptedHost`], which is a `HostBridge` whose `exec`
//! answers from a canned [`ExecOutcome`] — the same fixture the Rust tests used
//! when they built a `CommandResult` by hand and called `git_failure` on it,
//! one layer further out.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::kernel::testing::TempDir;
use crate::kernel::{ExecOutcome, ExecRequest, HostBridge, Kernel, KernelOptions};
use crate::tools::{MAX_DIFF_BYTES, ToolAccess, ToolContext, ToolOutput};

/// A kernel rooted at `root`, with the bundled plugins loaded into it.
async fn bundled_kernel(root: &Path) -> Kernel {
    let kernel = super::test_kernel(root);
    super::load_into(&kernel).await;
    kernel
}

/// Call a bundled tool the way the dispatcher would.
async fn call(kernel: &Kernel, tool: &str, args: Value, cwd: &Path) -> ToolOutput {
    kernel
        .tool(tool)
        .unwrap_or_else(|| panic!("'{tool}' is registered"))
        .execute(args, &ToolContext::new(cwd))
        .await
        .expect("the tool ran")
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

// ---------------------------------------------------------------------------
// The mechanism: what shipping in the binary buys.
// ---------------------------------------------------------------------------

/// The plugin loads from the bytes in this repository, with no directory laid
/// down anywhere and nothing installed.
///
/// The kernel's plugin root here is a subdirectory of a fresh temp dir that
/// does not exist, which is the whole assertion: a first-party Lua plugin is
/// not a file the loader found.
#[tokio::test]
async fn the_git_plugin_ships_in_the_binary_and_needs_nothing_on_disk() {
    let dir = TempDir::new("bundled-ships");
    let kernel = bundled_kernel(&dir.path).await;

    assert!(
        !kernel.plugin_root().exists(),
        "nothing was installed into the plugin directory"
    );
    assert!(kernel.loaded().iter().any(|id| id.as_str() == "git"));
    assert!(kernel.tool("git_status").is_some());
    assert!(kernel.tool("git_diff").is_some());
}

/// The two tools advertise exactly what the native ones did.
///
/// Written out as literals rather than compared against the deleted structs,
/// because the deleted structs are deleted: this is the contract with the
/// model, and it is a string in a request body. The empty-object schema is
/// checked as an object specifically — Lua has one table type, so a
/// `properties = {}` written the obvious way serialises as `[]` and would tell
/// the provider something else entirely.
#[tokio::test]
async fn the_ported_tools_advertise_the_schema_the_native_ones_did() {
    let dir = TempDir::new("bundled-schema");
    let kernel = bundled_kernel(&dir.path).await;

    let status = kernel.tool("git_status").expect("git_status");
    assert_eq!(status.name(), "git_status");
    assert_eq!(
        status.description(),
        "Show the git working tree status of the project (branch, staged, modified, and untracked files)."
    );
    assert_eq!(status.access(), ToolAccess::ReadOnly);
    assert_eq!(
        status.parameters(),
        json!({ "type": "object", "properties": {} })
    );
    assert!(
        status.parameters()["properties"].is_object(),
        "an empty schema is an object, not an empty array"
    );

    let diff = kernel.tool("git_diff").expect("git_diff");
    assert_eq!(diff.name(), "git_diff");
    assert_eq!(
        diff.description(),
        "Show the git diff of the project: unstaged changes by default, staged with staged=true, optionally limited to one path."
    );
    assert_eq!(diff.access(), ToolAccess::ReadOnly);
    let params = diff.parameters();
    assert_eq!(params["type"], "object");
    assert_eq!(params["properties"]["staged"]["type"], "boolean");
    assert_eq!(
        params["properties"]["staged"]["description"],
        "Diff staged changes instead of the working tree"
    );
    assert_eq!(params["properties"]["path"]["type"], "string");
    assert_eq!(
        params["properties"]["path"]["description"],
        "Limit the diff to this path"
    );
}

// ---------------------------------------------------------------------------
// The behaviour, against real git.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn status_reports_clean_tree() {
    let dir = TempDir::new("bundled-git-clean");
    git(&dir.path, &["init", "-q"]);
    let kernel = bundled_kernel(&dir.path).await;

    let out = call(&kernel, "git_status", json!({}), &dir.path).await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.starts_with("##"), "{}", out.content);
    assert!(
        out.content.contains("(clean working tree)"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn status_lists_untracked_files() {
    let dir = TempDir::new("bundled-git-untracked");
    git(&dir.path, &["init", "-q"]);
    std::fs::write(dir.path.join("new.txt"), "hello\n").unwrap();
    let kernel = bundled_kernel(&dir.path).await;

    let out = call(&kernel, "git_status", json!({}), &dir.path).await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("?? new.txt"), "{}", out.content);
    assert!(!out.content.contains("(clean working tree)"));
}

/// git exits 128 and says why on stderr, and the tool reports that as a
/// failure with git's own words.
///
/// This is the assertion that needed a result protocol richer than a string.
/// The only failure channel a Lua tool had was an `error:` prefix in the text,
/// which would have put a marker word in front of `fatal: not a git
/// repository` and changed what the model reads.
#[tokio::test]
async fn status_outside_a_repository_is_an_error() {
    let dir = TempDir::new("bundled-git-norepo");
    let kernel = bundled_kernel(&dir.path).await;

    let out = call(&kernel, "git_status", json!({}), &dir.path).await;
    assert!(out.is_error, "{}", out.content);
    assert!(!out.content.is_empty());
    assert!(
        !out.content.starts_with("error:"),
        "the model gets git's message, not a marker word: {}",
        out.content
    );
}

#[tokio::test]
async fn diff_reports_no_changes_in_fresh_repo() {
    let dir = TempDir::new("bundled-git-fresh");
    git(&dir.path, &["init", "-q"]);
    let kernel = bundled_kernel(&dir.path).await;

    let out = call(&kernel, "git_diff", json!({}), &dir.path).await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(out.content, "No changes.");
}

#[tokio::test]
async fn diff_distinguishes_unstaged_staged_and_paths() {
    let dir = TempDir::new("bundled-git-diff");
    git(&dir.path, &["init", "-q"]);
    std::fs::write(dir.path.join("f.txt"), "old\n").unwrap();
    commit_all(&dir.path, "init");
    std::fs::write(dir.path.join("f.txt"), "new\n").unwrap();
    let kernel = bundled_kernel(&dir.path).await;

    let unstaged = call(&kernel, "git_diff", json!({}), &dir.path).await;
    assert!(unstaged.content.contains("-old"), "{}", unstaged.content);
    assert!(unstaged.content.contains("+new"), "{}", unstaged.content);

    // Not staged yet, so the staged diff is empty.
    let staged = call(&kernel, "git_diff", json!({ "staged": true }), &dir.path).await;
    assert_eq!(staged.content, "No changes.");

    git(&dir.path, &["add", "f.txt"]);
    let staged = call(&kernel, "git_diff", json!({ "staged": true }), &dir.path).await;
    assert!(staged.content.contains("+new"), "{}", staged.content);

    // A path filter that matches nothing yields no changes.
    let other = call(
        &kernel,
        "git_diff",
        json!({ "staged": true, "path": "other.txt" }),
        &dir.path,
    )
    .await;
    assert_eq!(other.content, "No changes.");
}

/// A diff of a generated file used to be able to spend 30 KB of the window and
/// then be re-sent on every following step. It gets a diff's budget, not the
/// one an arbitrary command's stdout gets — which is the assertion that needed
/// `wizard.limits` and `wizard.truncate`, since the only cap a Lua tool had
/// was the blanket 30 KB the host applies on the way out.
#[tokio::test]
async fn a_huge_diff_is_cut_to_the_diff_budget() {
    let dir = TempDir::new("bundled-git-huge");
    git(&dir.path, &["init", "-q"]);
    std::fs::write(dir.path.join("f.txt"), "old\n").unwrap();
    commit_all(&dir.path, "init");
    let generated: String = (0..20_000).map(|line| format!("line {line}\n")).collect();
    assert!(generated.len() > crate::tools::MAX_OUTPUT_BYTES);
    std::fs::write(dir.path.join("f.txt"), generated).unwrap();
    let kernel = bundled_kernel(&dir.path).await;

    let out = call(&kernel, "git_diff", json!({}), &dir.path).await;
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
    // Head and tail framing survives the smaller budget: the first hunk header
    // and the last lines are both still there.
    assert!(out.content.contains("--- a/f.txt"), "{}", out.content);
    assert!(out.content.contains("line 19999"), "{}", out.content);
}

/// The tool runs where the *call* says, not where the kernel was rooted.
///
/// `ToolContext::cwd` is the one field of it that crosses into Lua, and this is
/// why: a subagent, a `run_code` program and a `--cwd` run all execute the same
/// registered tool against a different directory, and a plugin that read the
/// kernel's root would answer about the wrong tree without failing.
#[tokio::test]
async fn the_tool_runs_in_the_directory_the_call_names() {
    let dir = TempDir::new("bundled-git-cwd");
    let elsewhere = dir.path.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    git(&elsewhere, &["init", "-q"]);
    std::fs::write(elsewhere.join("only-here.txt"), "x\n").unwrap();

    // The kernel is rooted at the parent, which is not a repository at all.
    let kernel = bundled_kernel(&dir.path).await;

    let out = call(&kernel, "git_status", json!({}), &elsewhere).await;
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains("?? only-here.txt"),
        "{}",
        out.content
    );
}

// ---------------------------------------------------------------------------
// The rendering, against a host that answers from a script.
// ---------------------------------------------------------------------------

/// A `HostBridge` whose `exec` answers from a canned outcome and records the
/// request it was given.
struct ScriptedHost {
    outcome: ExecOutcome,
    seen: Mutex<Vec<Vec<String>>>,
}

impl ScriptedHost {
    fn arc(outcome: ExecOutcome) -> Arc<ScriptedHost> {
        Arc::new(ScriptedHost {
            outcome,
            seen: Mutex::new(Vec::new()),
        })
    }

    fn argv(&self) -> Vec<Vec<String>> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl HostBridge for ScriptedHost {
    async fn http(&self, _method: &str, _url: &str, _body: Option<String>) -> anyhow::Result<String> {
        anyhow::bail!("the git plugin does not fetch")
    }

    async fn model(&self, _plugin: &str, _prompt: &str) -> anyhow::Result<String> {
        anyhow::bail!("the git plugin does not call a model")
    }

    async fn notify(&self, _plugin: &str, _text: &str) -> anyhow::Result<()> {
        anyhow::bail!("the git plugin does not notify")
    }

    async fn spawn_agent(&self, _plugin: &str, _task: &str) -> anyhow::Result<String> {
        anyhow::bail!("the git plugin does not spawn")
    }

    async fn run(&self, _plugin: &str, _command: &str) -> anyhow::Result<String> {
        anyhow::bail!("the git plugin runs argv, not a shell line")
    }

    async fn exec(&self, _plugin: &str, request: ExecRequest) -> anyhow::Result<ExecOutcome> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request.argv);
        Ok(self.outcome.clone())
    }
}

async fn kernel_with(root: &Path, host: Arc<ScriptedHost>) -> Kernel {
    let kernel = Kernel::new(KernelOptions {
        project_root: root.to_path_buf(),
        plugin_root: root.join("plugins"),
        host,
        ..KernelOptions::default()
    });
    super::load_into(&kernel).await;
    kernel
}

#[tokio::test]
async fn git_failure_prefers_stderr_over_fallback() {
    let dir = TempDir::new("bundled-git-stderr");

    let host = ScriptedHost::arc(ExecOutcome {
        stderr: "fatal: boom\n".to_string(),
        code: Some(128),
        ..ExecOutcome::default()
    });
    let kernel = kernel_with(&dir.path, Arc::clone(&host)).await;
    let out = call(&kernel, "git_status", json!({}), &dir.path).await;
    assert!(out.is_error);
    assert_eq!(out.content, "fatal: boom");

    // Non-zero and silent: the fallback names which invocation failed, which is
    // otherwise all the model would have.
    let host = ScriptedHost::arc(ExecOutcome {
        code: Some(1),
        ..ExecOutcome::default()
    });
    let kernel = kernel_with(&dir.path, host).await;
    let out = call(&kernel, "git_status", json!({}), &dir.path).await;
    assert!(out.is_error);
    assert_eq!(out.content, "git status failed");

    let host = ScriptedHost::arc(ExecOutcome {
        code: Some(1),
        ..ExecOutcome::default()
    });
    let kernel = kernel_with(&dir.path, host).await;
    let out = call(&kernel, "git_diff", json!({}), &dir.path).await;
    assert!(out.is_error);
    assert_eq!(out.content, "git diff failed");
}

#[tokio::test]
async fn git_failure_renders_timeout_with_partial_output() {
    let dir = TempDir::new("bundled-git-timeout");
    let host = ScriptedHost::arc(ExecOutcome {
        stdout: "partial diff\n".to_string(),
        timed_out: Some(30),
        ..ExecOutcome::default()
    });
    let kernel = kernel_with(&dir.path, host).await;

    let out = call(&kernel, "git_diff", json!({}), &dir.path).await;
    assert!(out.is_error);
    assert!(out.content.contains("partial diff"), "{}", out.content);
    assert!(
        out.content.contains("timed out after 30s"),
        "{}",
        out.content
    );
    assert!(
        out.content.contains("output above is partial"),
        "{}",
        out.content
    );
}

/// A timeout with nothing captured says so rather than trailing a blank line.
#[tokio::test]
async fn a_silent_timeout_says_there_was_no_output() {
    let dir = TempDir::new("bundled-git-silent-timeout");
    let host = ScriptedHost::arc(ExecOutcome {
        timed_out: Some(30),
        ..ExecOutcome::default()
    });
    let kernel = kernel_with(&dir.path, host).await;

    let out = call(&kernel, "git_status", json!({}), &dir.path).await;
    assert!(out.is_error);
    assert_eq!(
        out.content,
        "command timed out after 30s and was killed (no output produced)"
    );
}

/// The argv the tool builds, which the native tool built with `Command::args`
/// and which `parse_args` used to defend.
///
/// Asserted rather than inferred from the output because it is the one thing a
/// shell-line host call would have got wrong: a path is passed as its own
/// argument, after `--`, with no quoting anywhere.
#[tokio::test]
async fn diff_builds_the_argv_the_native_tool_did() {
    let dir = TempDir::new("bundled-git-argv");
    let host = ScriptedHost::arc(ExecOutcome {
        code: Some(0),
        ..ExecOutcome::default()
    });
    let kernel = kernel_with(&dir.path, Arc::clone(&host)).await;

    call(&kernel, "git_diff", json!({}), &dir.path).await;
    call(&kernel, "git_diff", json!({ "staged": true }), &dir.path).await;
    call(
        &kernel,
        "git_diff",
        json!({ "path": "a file with spaces.txt" }),
        &dir.path,
    )
    .await;
    call(&kernel, "git_status", json!({}), &dir.path).await;

    assert_eq!(
        host.argv(),
        vec![
            vec!["git", "diff"],
            vec!["git", "diff", "--cached"],
            vec!["git", "diff", "--", "a file with spaces.txt"],
            vec!["git", "status", "--porcelain=v1", "-b"],
        ]
    );
}

/// An argument the schema rules out is refused, as `serde` refused it for the
/// native tool.
#[tokio::test]
async fn diff_rejects_arguments_of_the_wrong_type() {
    let dir = TempDir::new("bundled-git-badargs");
    let host = ScriptedHost::arc(ExecOutcome {
        code: Some(0),
        ..ExecOutcome::default()
    });
    let kernel = kernel_with(&dir.path, Arc::clone(&host)).await;

    let err = kernel
        .tool("git_diff")
        .expect("git_diff")
        .execute(json!({ "staged": 5 }), &ToolContext::new(&dir.path))
        .await
        .expect_err("a non-boolean 'staged' must be refused");
    // Flattened: `ToolError::Execution` prints "tool '...' failed" and the
    // reason a plugin gave is the layer under it.
    let err = format!("{:#}", anyhow::Error::new(err));
    assert!(err.contains("must be a boolean"), "{err}");

    let err = kernel
        .tool("git_diff")
        .expect("git_diff")
        .execute(json!({ "path": 5 }), &ToolContext::new(&dir.path))
        .await
        .expect_err("a non-string 'path' must be refused");
    let err = format!("{:#}", anyhow::Error::new(err));
    assert!(err.contains("must be a string"), "{err}");

    assert!(
        host.argv().is_empty(),
        "a refused call must not reach a process"
    );
}
