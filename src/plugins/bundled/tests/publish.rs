//! `publish`, ported.
//!
//! `src/tools/publish.rs` and `src/evolve/publish.rs` are both deleted, and
//! this file is what their two test modules became. The old ones could only
//! reach the pure helpers — `install_one_liner`, `fork_slug`, `parse_gh_login`
//! and three `parse_args` cases — because everything else in that file was a
//! blocking `Command` against somebody's real GitHub account. So the nine
//! steps that matter were tested by nothing.
//!
//! They are tested here, and the reason the port made that possible is worth
//! naming: `wizard.process.exec` is an interface. A [`ScriptHost`] answers it
//! from a table keyed on argv, so a whole publish — the `gh` probe, the auth
//! check, the login lookup, the fork, the fork's visibility check, the remote,
//! the push, the log line and the summary — runs end to end with no `gh` on
//! the machine and no repository anywhere. The Rust could not be tested that
//! way without an indirection it did not have.
//!
//! # The one thing on the real filesystem
//!
//! `ensure_source` asks whether `~/.wizard/src/Cargo.toml` opens, through
//! `io.open` rather than through the host, because that is what a file
//! predicate is. `Config::wizard_dir` is redirected to a temp directory under
//! `cfg(test)`, so that path is already harmless — but it is *one* path shared
//! by every test in this binary, and this module has tests that need the
//! checkout present and one that needs it absent. [`CHECKOUT`] serialises
//! them.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use serde_json::json;

use crate::config::Config;
use crate::kernel::testing::TempDir;
use crate::kernel::{ExecOutcome, ExecRequest, HostBridge, Kernel, KernelOptions};
use crate::tools::{ToolAccess, ToolContext};

use super::{bundled_kernel, call};

/// Serialises the tests that disagree about whether `~/.wizard/src` holds a
/// checkout. Every test in this module takes it; only [`no_checkout`] releases
/// the directory in the state the others do not want.
static CHECKOUT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Make `~/.wizard/src` look like a Wizard checkout, so `ensure_source` is
/// satisfied and the test is about the nine steps after it.
fn with_checkout() {
    let dir = Config::source_dir().expect("a redirected wizard dir");
    std::fs::create_dir_all(&dir).expect("the source dir");
    std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"wizard\"\n").expect("Cargo.toml");
}

/// Take the checkout away, for the one test about not having one.
fn without_checkout() {
    if let Ok(dir) = Config::source_dir() {
        let _ = std::fs::remove_file(dir.join("Cargo.toml"));
    }
}

// ---------------------------------------------------------------------------
// A host whose `exec` answers from a script.
// ---------------------------------------------------------------------------

/// One canned answer, plus every argv the plugin actually ran.
///
/// Keyed on the argv joined with spaces and matched by *prefix*, because half
/// of these commands carry an argument the test does not want to write out
/// twice (a fork slug derived from a login, a refspec derived from a branch).
/// First rule that matches wins, so a test can override one step of an
/// otherwise-successful run by putting it first.
#[derive(Default)]
struct ScriptHost {
    rules: Vec<(String, ExecOutcome)>,
    seen: Mutex<Vec<Vec<String>>>,
}

impl ScriptHost {
    /// A host where every step of a publish succeeds, for `login`.
    fn happy(login: &str) -> Self {
        let user = format!(r#"{{"id":7,"login":"{login}"}}"#);
        Self::default()
            .ok("gh --version", "gh version 2.0.0")
            .ok("gh auth status", "")
            .ok("gh api user", &user)
            .ok("gh repo fork", "")
            .ok("gh repo view", "")
            .ok("git -C", "")
    }

    fn ok(mut self, prefix: &str, stdout: &str) -> Self {
        self.rules.push((
            prefix.to_string(),
            ExecOutcome {
                stdout: stdout.to_string(),
                code: Some(0),
                ..ExecOutcome::default()
            },
        ));
        self
    }

    /// Put a failing answer *in front* of the happy rules.
    fn failing(mut self, prefix: &str, code: i32, stderr: &str) -> Self {
        self.rules.insert(
            0,
            (
                prefix.to_string(),
                ExecOutcome {
                    stderr: stderr.to_string(),
                    code: Some(code),
                    ..ExecOutcome::default()
                },
            ),
        );
        self
    }

    /// Put a successful answer in front of the happy rules, for a step whose
    /// stdout the test cares about.
    fn answering(mut self, prefix: &str, stdout: &str) -> Self {
        self.rules.insert(
            0,
            (
                prefix.to_string(),
                ExecOutcome {
                    stdout: stdout.to_string(),
                    code: Some(0),
                    ..ExecOutcome::default()
                },
            ),
        );
        self
    }

    fn arc(self) -> Arc<Self> {
        Arc::new(self)
    }

    /// Every argv the plugin ran, joined, in order.
    fn ran(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|argv| argv.join(" "))
            .collect()
    }
}

#[async_trait]
impl HostBridge for ScriptHost {
    async fn http(
        &self,
        _method: &str,
        _url: &str,
        _body: Option<String>,
    ) -> anyhow::Result<String> {
        anyhow::bail!("the publish plugin opens no socket of its own")
    }

    async fn model(&self, _plugin: &str, _prompt: &str) -> anyhow::Result<String> {
        anyhow::bail!("the publish plugin asks no model")
    }

    async fn notify(&self, _plugin: &str, _text: &str) -> anyhow::Result<()> {
        anyhow::bail!("the publish plugin says nothing outside its own return value")
    }

    async fn spawn_agent(&self, _plugin: &str, _task: &str) -> anyhow::Result<String> {
        anyhow::bail!("the publish plugin spawns nothing")
    }

    async fn run(&self, _plugin: &str, _command: &str) -> anyhow::Result<String> {
        anyhow::bail!("the publish plugin runs argv, not a shell line")
    }

    async fn exec(&self, _plugin: &str, request: ExecRequest) -> anyhow::Result<ExecOutcome> {
        let joined = request.argv.join(" ");
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request.argv);
        for (prefix, outcome) in &self.rules {
            if joined.starts_with(prefix.as_str()) {
                return Ok(outcome.clone());
            }
        }
        // Not an `Err`: an unscripted command is a *program that failed*, which
        // is the outcome the plugin has to render, and an `Err` would arrive as
        // "the tool broke" and hide which step went unscripted.
        Ok(ExecOutcome {
            stderr: format!("no rule for `{joined}`"),
            code: Some(127),
            ..ExecOutcome::default()
        })
    }
}

async fn kernel_with(root: &Path, host: Arc<ScriptHost>) -> Kernel {
    let kernel = Kernel::new(KernelOptions {
        project_root: root.to_path_buf(),
        plugin_root: root.join("plugins"),
        host,
        ..KernelOptions::default()
    });
    super::super::load_into(&kernel).await;
    kernel
}

/// Run `publish` against a scripted host and hand back what the model would
/// read plus every command that ran.
async fn publish(
    root: &Path,
    host: Arc<ScriptHost>,
    args: serde_json::Value,
) -> (bool, String, Vec<String>) {
    let kernel = kernel_with(root, Arc::clone(&host)).await;
    let out = call(&kernel, "publish", args, root).await;
    (out.is_error, out.content, host.ran())
}

// ---------------------------------------------------------------------------
// The mechanism.
// ---------------------------------------------------------------------------

/// The plugin loads from the bytes in this repository, with nothing installed.
#[tokio::test]
async fn the_publish_plugin_ships_in_the_binary_and_needs_nothing_on_disk() {
    let dir = TempDir::new("bundled-publish-ships");
    let kernel = bundled_kernel(&dir.path).await;

    assert!(
        !kernel.plugin_root().exists(),
        "nothing was installed into the plugin directory"
    );
    assert!(kernel.loaded().iter().any(|id| id.as_str() == "publish"));
    assert!(kernel.tool("publish").is_some());
}

/// The tool advertises exactly what the native one did.
///
/// Written out as a literal because the native struct is deleted and this is
/// the contract with the model: a description in a request body and a schema
/// beside it. `access` is asserted specifically — the native tool took the
/// trait's default and the Lua one takes the host's, and if those two ever
/// disagreed a `publish` would become runnable in plan mode.
#[tokio::test]
async fn the_ported_tool_advertises_what_the_native_one_did() {
    let dir = TempDir::new("bundled-publish-schema");
    let kernel = bundled_kernel(&dir.path).await;

    let tool = kernel.tool("publish").expect("publish");
    assert_eq!(tool.name(), "publish");
    assert_eq!(
        tool.description(),
        "Fork Wizard to your own GitHub account and get a one-line installer \
         for your personalised variant. Use this after a deep evolve (or any \
         time you want to distribute the version of Wizard running on this \
         machine). The fork is created under your authenticated GitHub account \
         via `gh`; the source checkout at ~/.wizard/src is pushed to the fork \
         and a `curl | bash` one-liner is returned that anyone can run to \
         install your variant (building from source). Requires `gh auth login`."
    );
    assert_eq!(
        tool.access(),
        ToolAccess::Execute,
        "publishing writes to somebody's GitHub account"
    );
    let params = tool.parameters();
    assert_eq!(params["type"], "object");
    assert_eq!(params["properties"]["branch"]["type"], "string");
    assert_eq!(
        params["properties"]["branch"]["description"],
        "Branch to push to on the fork. Defaults to \"main\"."
    );
    // The native tool wrote `"required": []` and this one writes nothing,
    // which is the same schema. It is not a shortcut: Lua's one table type
    // makes `required = {}` the JSON object `{}`, and a schema with an object
    // there is worse than one with the key absent. See the plugin.
    assert!(params.get("required").is_none(), "{params}");
}

// ---------------------------------------------------------------------------
// The nine steps.
// ---------------------------------------------------------------------------

/// The whole pipeline, in order, with the exact argv of every step.
///
/// This is the assertion the Rust had no way to make. Each of these lines was
/// a `Command::new(...)` builder whose arguments were only observable by
/// running it against GitHub, so "does the refspec say `HEAD:main`" and "is
/// the fork created with `--clone=false`" were questions nothing asked.
#[tokio::test]
async fn a_publish_walks_gh_then_git_in_order() {
    let _guard = CHECKOUT.lock().await;
    with_checkout();
    let dir = TempDir::new("bundled-publish-happy");
    let source = Config::source_dir().expect("source dir");
    let source = source.to_string_lossy().to_string();

    let host = ScriptHost::happy("alice")
        .answering(&format!("git -C {source} rev-parse"), "abc1234")
        .arc();

    let (is_error, content, ran) = publish(&dir.path, host, json!({})).await;
    assert!(!is_error, "{content}");

    assert_eq!(
        ran,
        vec![
            "gh --version".to_string(),
            "gh auth status".to_string(),
            "gh api user".to_string(),
            "gh repo fork teddytennant/wizard --clone=false".to_string(),
            "gh repo view alice/wizard".to_string(),
            format!("git -C {source} rev-parse --short HEAD"),
            format!("git -C {source} remote get-url fork"),
            format!("git -C {source} remote set-url fork https://github.com/alice/wizard.git"),
            format!("git -C {source} push fork HEAD:main"),
        ]
    );

    assert_eq!(
        content,
        "Published to https://github.com/alice/wizard  (branch: main)  commit: abc1234\n\n\
         Install one-liner:\n\
         curl -fsSL https://raw.githubusercontent.com/alice/wizard/main/install.sh | \
         WIZARD_REPO=alice/wizard WIZARD_REF=main WIZARD_BUILD_FROM_SOURCE=1 bash"
    );
}

/// A named branch reaches the refspec, the one-liner and the summary.
///
/// Three places, from one argument, and the old `install_one_liner` test could
/// only see the third of them.
#[tokio::test]
async fn a_named_branch_reaches_the_refspec_and_the_one_liner() {
    let _guard = CHECKOUT.lock().await;
    with_checkout();
    let dir = TempDir::new("bundled-publish-branch");
    let source = Config::source_dir().expect("source dir");
    let source = source.to_string_lossy().to_string();

    let host = ScriptHost::happy("bob").arc();
    let (is_error, content, ran) =
        publish(&dir.path, host, json!({ "branch": "my-feature" })).await;

    assert!(!is_error, "{content}");
    assert!(
        ran.contains(&format!("git -C {source} push fork HEAD:my-feature")),
        "{ran:?}"
    );
    assert!(content.contains("(branch: my-feature)"), "{content}");
    assert!(content.contains("WIZARD_REF=my-feature"), "{content}");
    assert!(content.contains("WIZARD_REPO=bob/wizard"), "{content}");
    assert!(
        content.contains("/bob/wizard/my-feature/install.sh"),
        "{content}"
    );
    assert!(content.contains("WIZARD_BUILD_FROM_SOURCE=1"), "{content}");
}

/// An empty or absent branch is `"main"`, as `unwrap_or_else` made it.
#[tokio::test]
async fn an_empty_branch_is_main() {
    let _guard = CHECKOUT.lock().await;
    with_checkout();
    let dir = TempDir::new("bundled-publish-empty-branch");
    let source = Config::source_dir().expect("source dir");
    let source = source.to_string_lossy().to_string();

    let host = ScriptHost::happy("carol").arc();
    let (is_error, content, ran) = publish(&dir.path, host, json!({ "branch": "  " })).await;
    assert!(!is_error, "{content}");
    assert!(
        ran.contains(&format!("git -C {source} push fork HEAD:main")),
        "{ran:?}"
    );
}

/// An argument the schema rules out is refused before anything runs, as
/// `parse_args` refused it for the native tool.
#[tokio::test]
async fn a_non_string_branch_is_refused_before_anything_runs() {
    let _guard = CHECKOUT.lock().await;
    with_checkout();
    let dir = TempDir::new("bundled-publish-badargs");
    let host = ScriptHost::happy("dave").arc();
    let kernel = kernel_with(&dir.path, Arc::clone(&host)).await;

    let err = kernel
        .tool("publish")
        .expect("publish")
        .execute(json!({ "branch": 5 }), &ToolContext::new(&dir.path))
        .await
        .expect_err("a non-string branch must be refused");
    let err = format!("{:#}", anyhow::Error::new(err));
    assert!(err.contains("must be a string"), "{err}");
    assert!(host.ran().is_empty(), "a refused call reached a process");
}

/// A checkout with no commits yet has no sha, and the summary simply omits it
/// rather than printing an empty `commit:`.
#[tokio::test]
async fn a_checkout_with_no_head_publishes_without_a_sha() {
    let _guard = CHECKOUT.lock().await;
    with_checkout();
    let dir = TempDir::new("bundled-publish-nohead");
    let source = Config::source_dir().expect("source dir");
    let source = source.to_string_lossy().to_string();

    let host = ScriptHost::happy("erin")
        .failing(
            &format!("git -C {source} rev-parse"),
            128,
            "fatal: ambiguous argument 'HEAD'",
        )
        .arc();
    let (is_error, content, _) = publish(&dir.path, host, json!({})).await;

    assert!(!is_error, "{content}");
    assert!(!content.contains("commit:"), "{content}");
    assert!(
        content.starts_with("Published to https://github.com/erin/wizard  (branch: main)\n"),
        "{content}"
    );
}

// ---------------------------------------------------------------------------
// The failures, each of which is bad news rather than a broken tool.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_missing_gh_says_where_to_get_it() {
    let _guard = CHECKOUT.lock().await;
    with_checkout();
    let dir = TempDir::new("bundled-publish-nogh");
    let host = ScriptHost::happy("frank")
        .failing("gh --version", 127, "gh: command not found")
        .arc();

    let (is_error, content, ran) = publish(&dir.path, host, json!({})).await;
    assert!(is_error, "{content}");
    assert!(content.starts_with("publish failed: "), "{content}");
    assert!(content.contains("https://cli.github.com"), "{content}");
    assert!(content.contains("gh auth login"), "{content}");
    assert_eq!(ran, vec!["gh --version".to_string()], "it stopped there");
}

#[tokio::test]
async fn an_unauthenticated_gh_says_to_log_in() {
    let _guard = CHECKOUT.lock().await;
    with_checkout();
    let dir = TempDir::new("bundled-publish-noauth");
    let host = ScriptHost::happy("grace")
        .failing(
            "gh auth status",
            1,
            "You are not logged into any GitHub hosts",
        )
        .arc();

    let (is_error, content, ran) = publish(&dir.path, host, json!({})).await;
    assert!(is_error, "{content}");
    assert!(content.contains("run `gh auth login` first"), "{content}");
    assert_eq!(ran.len(), 2, "{ran:?}");
}

/// `gh repo fork` exits non-zero when the fork is already there, which is the
/// common case for anybody publishing twice. It is not a failure.
#[tokio::test]
async fn an_existing_fork_is_not_a_failure() {
    let _guard = CHECKOUT.lock().await;
    with_checkout();
    let dir = TempDir::new("bundled-publish-refork");
    let host = ScriptHost::happy("heidi")
        .failing(
            "gh repo fork",
            1,
            "heidi/wizard already exists on your account",
        )
        .arc();

    let (is_error, content, ran) = publish(&dir.path, host, json!({})).await;
    assert!(!is_error, "{content}");
    assert!(
        ran.iter().any(|line| line.contains("push fork HEAD:main")),
        "{ran:?}"
    );
}

/// Any other `gh repo fork` failure is one, and it carries gh's own words.
#[tokio::test]
async fn a_fork_that_really_failed_reports_ghs_words() {
    let _guard = CHECKOUT.lock().await;
    with_checkout();
    let dir = TempDir::new("bundled-publish-forkfail");
    let host = ScriptHost::happy("ivan")
        .failing("gh repo fork", 1, "HTTP 403: Resource not accessible")
        .arc();

    let (is_error, content, _) = publish(&dir.path, host, json!({})).await;
    assert!(is_error, "{content}");
    assert!(
        content.contains("HTTP 403: Resource not accessible"),
        "{content}"
    );
}

/// The remote is *added* when it is not there and *updated* when it is, and
/// the difference matters: somebody who re-forked under an organisation has a
/// `fork` remote pointing at a repository that no longer exists.
#[tokio::test]
async fn a_missing_fork_remote_is_added_rather_than_updated() {
    let _guard = CHECKOUT.lock().await;
    with_checkout();
    let dir = TempDir::new("bundled-publish-addremote");
    let source = Config::source_dir().expect("source dir");
    let source = source.to_string_lossy().to_string();

    let host = ScriptHost::happy("judy")
        .failing(
            &format!("git -C {source} remote get-url fork"),
            2,
            "error: No such remote 'fork'",
        )
        .arc();
    let (is_error, content, ran) = publish(&dir.path, host, json!({})).await;

    assert!(!is_error, "{content}");
    assert!(
        ran.contains(&format!(
            "git -C {source} remote add fork https://github.com/judy/wizard.git"
        )),
        "{ran:?}"
    );
}

#[tokio::test]
async fn a_rejected_push_reports_gits_words() {
    let _guard = CHECKOUT.lock().await;
    with_checkout();
    let dir = TempDir::new("bundled-publish-push");
    let source = Config::source_dir().expect("source dir");
    let source = source.to_string_lossy().to_string();

    let host = ScriptHost::happy("ken")
        .failing(
            &format!("git -C {source} push"),
            1,
            "! [rejected] HEAD -> main (non-fast-forward)",
        )
        .arc();
    let (is_error, content, _) = publish(&dir.path, host, json!({})).await;

    assert!(is_error, "{content}");
    assert!(content.contains("non-fast-forward"), "{content}");
    assert!(content.contains("git push fork HEAD:main"), "{content}");
}

/// `gh api user` that answers with something other than a login object.
///
/// The three cases `parse_gh_login`'s unit tests covered, now reached through
/// the tool rather than through a `pub fn` that existed to be tested.
#[tokio::test]
async fn a_user_response_without_a_login_is_a_named_failure() {
    let _guard = CHECKOUT.lock().await;
    with_checkout();
    let dir = TempDir::new("bundled-publish-nologin");

    for (answer, expected) in [
        (r#"{"id":1,"name":"No Login Here"}"#, "`.login` is not in"),
        ("not json at all", "did not answer with JSON"),
        ("", "did not answer with JSON"),
    ] {
        let host = ScriptHost::happy("x")
            .answering("gh api user", answer)
            .arc();
        let (is_error, content, _) = publish(&dir.path, host, json!({})).await;
        assert!(is_error, "{answer}: {content}");
        assert!(content.contains(expected), "{answer}: {content}");
    }
}

/// A publish that never got a checkout says so, and the message names the
/// clone that failed rather than the step after it.
#[tokio::test]
async fn no_checkout_clones_one_and_reports_a_clone_that_failed() {
    let _guard = CHECKOUT.lock().await;
    without_checkout();
    let dir = TempDir::new("bundled-publish-noclone");
    let source = Config::source_dir().expect("source dir");
    let source = source.to_string_lossy().to_string();

    let host = ScriptHost::happy("leo")
        .failing(
            "git clone",
            128,
            "fatal: destination path already exists and is not an empty directory",
        )
        .arc();
    let (is_error, content, ran) = publish(&dir.path, host, json!({})).await;

    assert!(is_error, "{content}");
    assert_eq!(
        ran,
        vec![format!(
            "git clone --depth 1 https://github.com/teddytennant/wizard {source}"
        )],
        "the clone is the first thing that runs, and nothing follows a failed one"
    );
    assert!(content.contains("already exists"), "{content}");
    assert!(
        content.contains(&format!("Remove {source} and retry.")),
        "{content}"
    );

    // Leave it as the other tests want it.
    with_checkout();
}

/// The publish record reaches `~/.wizard/evolution.jsonl`.
///
/// Which is also the test that `wizard.paths` is `Config`'s answer and not a
/// join onto `$HOME`: under `cfg(test)` those two are different directories,
/// and a plugin that had derived the path itself would have written into the
/// developer's real one and this assertion would find nothing.
#[tokio::test]
async fn the_publish_record_lands_in_the_evolution_log() {
    let _guard = CHECKOUT.lock().await;
    with_checkout();
    let dir = TempDir::new("bundled-publish-log");
    let host = ScriptHost::happy("mallory-unique").arc();

    let (is_error, content, _) = publish(&dir.path, host, json!({ "branch": "logged" })).await;
    assert!(!is_error, "{content}");

    let path = Config::evolution_log_path().expect("the log path");
    let raw = std::fs::read_to_string(&path).expect("the log exists");
    let line = raw
        .lines()
        .rfind(|line| line.contains("mallory-unique"))
        .expect("a publish line");
    let record: serde_json::Value = serde_json::from_str(line).expect("one JSON object per line");

    // The `"event"` key is the discriminator `read_events` skips on. Without
    // it a publish line would be logged as a malformed *evolution* and
    // `wizard evolve list` would warn about it once per publish, forever.
    assert_eq!(record["event"], "publish");
    assert_eq!(record["fork_repo"], "mallory-unique/wizard");
    assert_eq!(record["branch"], "logged");
    assert_eq!(
        record["fork_url"],
        "https://github.com/mallory-unique/wizard"
    );
    assert!(
        record["install_one_liner"]
            .as_str()
            .is_some_and(|line| line.contains("WIZARD_REF=logged")),
        "{record}"
    );
}
