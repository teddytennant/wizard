//! Lifecycle hooks: user-supplied shell commands that observe and steer the
//! agent at fixed points of every mode (TUI genie, sovereign headless,
//! perpetual continuous, gateway). See `docs/hooks.md`.
//!
//! Hooks are declared in `~/.wizard/hooks.toml` and
//! `<project>/.wizard/hooks.toml` (project hooks run after global ones, and
//! only once the project is trusted, see [`crate::trust`]) and
//! receive a JSON payload on stdin. Exit 0 continues — depending on the
//! event, stdout may rewrite tool arguments or append extra context. Exit 2
//! blocks, with stderr as the reason. Any other exit code, a timeout, or a
//! spawn failure is a logged warning and the pipeline continues: a broken
//! hook can never wedge the agent.

pub mod backend;
pub mod engine;
pub mod event;
pub mod lifecycle;
pub mod matcher;
pub mod protocol;
pub mod spec;
pub mod toolmap;

pub use engine::{Before, Fire, HookBackend, HookEngine, Outcome};
pub use spec::{Handler, HookSpec, Origin};

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Deserialize;
use serde_json::Value;

use crate::config::claude::{self, ClaudeSettings};
use crate::config::{Config, Mode};

/// Wall-clock budget for one hook when `timeout_secs` is not set.
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Cap on the `tool_output` field in `post_tool_use` payloads.
const TOOL_OUTPUT_CAP_BYTES: usize = 32 * 1024;

/// `output` capped to [`TOOL_OUTPUT_CAP_BYTES`] on a char boundary, with a
/// marker when content was dropped.
fn cap_tool_output(output: &str) -> String {
    if output.len() <= TOOL_OUTPUT_CAP_BYTES {
        return output.to_string();
    }
    let mut cut = TOOL_OUTPUT_CAP_BYTES;
    while cut > 0 && !output.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n... [tool output truncated]", &output[..cut])
}

pub use event::HookEvent;

/// One `[[hooks]]` entry from a `hooks.toml` file.
#[derive(Debug, Clone, Deserialize)]
pub struct HookDef {
    /// Which lifecycle event this hook fires on.
    pub event: HookEvent,
    /// Optional glob over the tool name (`"execute"`, `"git_*"`). Only
    /// meaningful for the tool events; other events fire regardless.
    #[serde(default)]
    pub matcher: Option<String>,
    /// Shell command, run in the project root through the platform shell
    /// ([`crate::platform::shell`]: `sh -c` on Unix).
    pub command: String,
    /// Wall-clock budget; the hook is killed and ignored past it
    /// (default 60).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// On-disk shape of a `hooks.toml` file.
#[derive(Debug, Default, Deserialize)]
struct HooksFile {
    #[serde(default)]
    hooks: Vec<HookDef>,
}

/// Parse the contents of one `hooks.toml`.
pub fn parse(raw: &str) -> Result<Vec<HookDef>, toml::de::Error> {
    toml::from_str::<HooksFile>(raw).map(|file| file.hooks)
}

/// Load hook definitions for a project: global `~/.wizard/hooks.toml` first,
/// then `<project>/.wizard/hooks.toml` appended after it, the latter only
/// when the project is trusted ([`crate::trust`]).
///
/// This is the single funnel every surface goes through (TUI, sovereign,
/// gateway, GUI, fleet), so the trust gate lives here rather than at the four
/// call sites: a hook that is never loaded can never fire, on any surface,
/// for any event.
///
/// Never prompts, on any surface, ever. This runs again on every agent rebuild
/// (`/model`, a provider switch, `/fusion`, crash recovery), by which time the
/// TUI owns the terminal in raw mode behind the alternate screen and a blocking
/// stdin read would freeze the whole app, so an undecided project is refused
/// rather than asked about. The question is settled once, earlier, by the
/// surface that still owns its terminal (`crate::trust::preflight`); by the
/// time anything gets here the answer is on record and this only reads it.
pub fn load(project_root: &Path) -> Vec<HookDef> {
    let mut paths = Vec::new();
    match Config::wizard_dir() {
        Ok(dir) => paths.push(dir.join("hooks.toml")),
        Err(err) => tracing::warn!("could not resolve ~/.wizard for hooks: {err}"),
    }
    // The global file lives in the user's own ~/.wizard and is theirs by
    // construction; only the project file arrives with a `git clone`, so only
    // the project file is gated.
    let mut defs = load_files(&paths);
    defs.extend(load_project(
        project_root,
        crate::trust::env_trust(),
        &log_refusal,
    ));
    defs
}

/// Every hook for a project, from every source: `~/.wizard/hooks.toml`,
/// `<project>/.wizard/hooks.toml`, and the hooks in Claude-format settings
/// files (`~/.claude/settings.json`, `.claude/settings.json`,
/// `.claude/settings.local.json` and the `.wizard/settings*.json` twins).
/// The settings that were read come back with them, for the rules, `env`
/// and other keys.
///
/// This is the funnel production code uses; [`load`] is the same thing for
/// `hooks.toml` alone. Project-owned files go through the trust gate once,
/// here, and the bytes it approved are the bytes that are read.
pub fn load_specs(project_root: &Path) -> (Vec<HookSpec>, ClaudeSettings) {
    load_specs_with(project_root, crate::trust::env_trust(), &log_refusal)
}

fn load_specs_with(
    project_root: &Path,
    env_trusted: bool,
    report: Report<'_>,
) -> (Vec<HookSpec>, ClaudeSettings) {
    let mut specs = Vec::new();
    match Config::wizard_dir() {
        Ok(dir) => {
            let path = dir.join("hooks.toml");
            if let Ok(raw) = std::fs::read_to_string(&path) {
                match spec::parse_toml(&raw, &path.display().to_string()) {
                    Ok(found) => specs.extend(found),
                    Err(err) => tracing::warn!("skipping invalid {}: {err}", path.display()),
                }
            }
        }
        Err(err) => tracing::warn!("could not resolve ~/.wizard for hooks: {err}"),
    }
    let gate = crate::trust::gate_with_console_env(
        project_root,
        crate::trust::Console::Unavailable,
        env_trusted,
    );
    let settings = match &gate {
        crate::trust::Gate::Allowed(surface) => {
            if let Some(raw) = surface.contents_of(crate::trust::PROJECT_HOOKS_FILE) {
                let label = project_root.join(crate::trust::PROJECT_HOOKS_FILE);
                let label = label.display().to_string();
                match std::str::from_utf8(raw) {
                    Ok(text) => match spec::parse_toml(text, &label) {
                        Ok(found) => specs.extend(found),
                        Err(err) => tracing::warn!("skipping invalid {label}: {err}"),
                    },
                    Err(err) => tracing::warn!("skipping {label}: not valid UTF-8: {err}"),
                }
            }
            claude::load(project_root, claude::Trust::Approved(surface))
        }
        crate::trust::Gate::Refused(why) => {
            report(why);
            claude::load(project_root, claude::Trust::Refused)
        }
    };
    for warning in &settings.warnings {
        tracing::warn!("{warning}");
    }
    specs.extend(settings.hooks.iter().cloned());
    // Hooks of enabled Claude-format plugins. The user installed or named
    // the plugin, so they are theirs the way `~/.claude/settings.json` is;
    // `disableAllHooks` turns them off with the rest.
    if !settings.disable_all_hooks {
        specs.extend(crate::claude_dir::plugins::hook_specs(project_root));
    }
    (specs, settings)
}

/// The engine for a session: every hook source, the permission policy, and
/// the `env` from Claude-format settings handed to spawned commands.
pub fn engine_for(config: &Config, project_root: &Path, session_id: String) -> HookEngine {
    let (specs, settings) = load_specs(project_root);
    crate::platform::shell::set_session_env(
        settings
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    );
    let engine = HookEngine::from_specs(specs, project_root.to_path_buf(), session_id);
    engine.set_policy(crate::permissions::build(config, &settings, project_root));
    engine
}

static AMBIENT: Mutex<Option<std::sync::Weak<HookEngine>>> = Mutex::new(None);

/// Remember the session's engine so code with no handle on it (an
/// instructions loader, the `todo` tool, the fleet) can still fire events.
/// The last agent built wins; a gateway serving several projects at once
/// reports these events to the newest.
pub fn set_ambient(engine: &std::sync::Arc<HookEngine>) {
    if let Ok(mut slot) = AMBIENT.lock() {
        *slot = Some(std::sync::Arc::downgrade(engine));
    }
}

/// The engine [`set_ambient`] remembered, if its agent is still alive.
pub fn ambient() -> Option<std::sync::Arc<HookEngine>> {
    AMBIENT.lock().ok()?.as_ref()?.upgrade()
}

/// Fire an observational event on the ambient engine without waiting for it.
/// Does nothing when no engine is registered, no hook listens for `event`, or
/// there is no async runtime to run it on.
pub fn emit_detached(event: HookEvent, subject: Option<String>, extra: Value) {
    let Some(engine) = ambient() else { return };
    if !engine.listens(event) {
        return;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    handle.spawn(async move {
        engine
            .emit(event, subject.as_deref(), extra, Mode::default(), None)
            .await;
    });
}

/// Where a trust refusal goes from *here*. One seam, one destination: the log.
///
/// The refusal used to be written to stderr as well, on the theory that a log
/// file is nobody's idea of a notification. But [`load_project`] runs again on
/// every agent rebuild (`/model`, a provider switch, `/fusion`, crash
/// recovery), and under the TUI that put a multi-line message straight onto
/// the alternate screen with the terminal in raw mode, where a bare `\n`
/// staircases the text across the frame. `crate::logging` is the sink that
/// exists precisely because nothing in this process may write to the terminal
/// behind the TUI's back.
///
/// A log line is not a notification, so it is not the only thing that happens:
/// each surface says it once, itself, where its user is actually looking. The
/// TUI settles the question before it takes the terminal
/// (`crate::trust::preflight`) and puts any refusal in the transcript; the
/// headless runner prints it before the spinner starts; the gateway and the
/// GUI call `crate::trust::unattended_refusal` and put it in the operator's
/// journal and the task's own stream. This is the per-rebuild trace underneath
/// all of that.
fn log_refusal(why: &str) {
    tracing::warn!("{why}");
}

/// How a refusal is surfaced, injected so a test can count the reports without
/// depending on a global tracing subscriber.
type Report<'a> = &'a dyn Fn(&str);

/// `<project>/.wizard/hooks.toml`, behind the per-project trust gate.
///
/// Cloning a repository must not be enough to run its shell commands, and
/// `session_start` fires before the model has said a word, so an untrusted
/// project contributes no hooks at all.
///
/// The approved bytes come back from the gate and are parsed here rather than
/// read again: between the read the trust decision was made on and a second
/// read at load time sits a `git pull`, a branch switch, or Wizard's own edit
/// of the project it is working in, and what runs must be what was approved.
///
/// The console declaration is hard-coded to
/// [`crate::trust::Console::Unavailable`] and not a parameter, because there is
/// no caller for which it could be anything else: this is the funnel every
/// surface shares, including the mid-turn rebuilds that run underneath a live
/// TUI. `env_trusted` *is* a parameter so a test can say which side of the
/// opt-in it is testing instead of inheriting the shell's.
fn load_project(project_root: &Path, env_trusted: bool, report: Report<'_>) -> Vec<HookDef> {
    match crate::trust::gate_with_console_env(
        project_root,
        crate::trust::Console::Unavailable,
        env_trusted,
    ) {
        crate::trust::Gate::Allowed(surface) => {
            match surface.contents_of(crate::trust::PROJECT_HOOKS_FILE) {
                Some(raw) => parse_bytes(&project_root.join(crate::trust::PROJECT_HOOKS_FILE), raw),
                // No project hooks file at all: the common case.
                None => Vec::new(),
            }
        }
        crate::trust::Gate::Refused(why) => {
            report(&why);
            Vec::new()
        }
    }
}

/// Read and parse each path in order, concatenating the results. Missing
/// files mean no hooks; unreadable or invalid files are skipped with a
/// warning so a bad hook file can never prevent startup.
fn load_files(paths: &[PathBuf]) -> Vec<HookDef> {
    let mut defs = Vec::new();
    for path in paths {
        let raw = match std::fs::read(path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                tracing::warn!("could not read {}: {err}", path.display());
                continue;
            }
        };
        defs.append(&mut parse_bytes(path, &raw));
    }
    defs
}

/// Parse one hooks file's bytes. Invalid UTF-8 or invalid TOML costs that
/// file's hooks and nothing else: a bad hook file can never prevent startup.
fn parse_bytes(path: &Path, raw: &[u8]) -> Vec<HookDef> {
    let raw = match std::str::from_utf8(raw) {
        Ok(raw) => raw,
        Err(err) => {
            tracing::warn!("skipping {}: not valid UTF-8: {err}", path.display());
            return Vec::new();
        }
    };
    match parse(raw) {
        Ok(hooks) => hooks,
        Err(err) => {
            tracing::warn!("skipping invalid {}: {err}", path.display());
            Vec::new()
        }
    }
}

/// What one hook execution did, surfaced via `AgentEvent::HookFired`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookOutcome {
    /// `pre_tool_use` rewrote the tool arguments.
    UpdatedArgs,
    /// Stdout was appended as extra context.
    AppendedContext,
    /// Exit 2: the action was blocked for this reason.
    Blocked(String),
    /// The hook misbehaved (other exit code, timeout, spawn failure) and was
    /// ignored.
    Warning(String),
}

impl fmt::Display for HookOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HookOutcome::UpdatedArgs => write!(f, "updated args"),
            HookOutcome::AppendedContext => write!(f, "appended context"),
            HookOutcome::Blocked(reason) => write!(f, "blocked — {reason}"),
            HookOutcome::Warning(why) => write!(f, "warning — {why}"),
        }
    }
}

/// Verdict of the `pre_tool_use` chain for one tool call.
#[derive(Debug)]
pub enum PreToolUse {
    /// Proceed; `Some` holds rewritten arguments.
    Continue(Option<Value>),
    /// Veto the call; the reason feeds back to the model as a tool error.
    Block(String),
}

/// Verdict of the `user_prompt_submit` chain for one turn.
#[derive(Debug)]
pub enum PromptSubmit {
    /// Proceed; `Some` holds extra context to append to the message.
    Continue(Option<String>),
    /// End the turn before the model sees the prompt.
    Block(String),
}

/// Append `post_tool_use` hook context to a tool result body. Shared by the
/// dispatcher and the subagent loop so the framing stays identical.
pub fn append_context(content: &mut String, extra: &str) {
    if !content.is_empty() {
        content.push_str("\n\n");
    }
    content.push_str("[post_tool_use hook]\n");
    content.push_str(extra);
}

/// Parse a `pre_tool_use` hook's stdout as `{"updated_args": {...}}`.
/// Anything else (empty, non-JSON, missing key, non-object value) means
/// "leave the arguments alone".
fn parse_updated_args(stdout: &str) -> Option<Value> {
    let value: Value = serde_json::from_str(stdout.trim()).ok()?;
    let updated = value.get("updated_args")?;
    updated.is_object().then(|| updated.clone())
}

/// Warning text for an unexpected exit code.
fn exit_warning(code: i32, stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        format!("exit {code}")
    } else {
        format!("exit {code}: {trimmed}")
    }
}

#[cfg(test)]
mod engine_tests;

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use tokio::sync::mpsc;

    use super::*;
    use crate::agent::AgentEvent;

    fn def(event: HookEvent, matcher: Option<&str>, command: &str) -> HookDef {
        HookDef {
            event,
            matcher: matcher.map(str::to_string),
            command: command.to_string(),
            timeout_secs: None,
        }
    }

    fn engine(defs: Vec<HookDef>) -> HookEngine {
        HookEngine::new(defs, std::env::temp_dir(), "test-session".to_string())
    }

    #[test]
    fn parse_full_entry() {
        let hooks = parse(
            r#"
            [[hooks]]
            event = "pre_tool_use"
            matcher = "execute"
            command = "/path/to/script.sh"
            timeout_secs = 30
            "#,
        )
        .expect("valid hooks.toml");
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].event, HookEvent::PreToolUse);
        assert_eq!(hooks[0].matcher.as_deref(), Some("execute"));
        assert_eq!(hooks[0].command, "/path/to/script.sh");
        assert_eq!(hooks[0].timeout_secs, Some(30));
    }

    #[test]
    fn parse_defaults_matcher_and_timeout() {
        let hooks =
            parse("[[hooks]]\nevent = \"turn_end\"\ncommand = \"date\"").expect("valid hooks.toml");
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].event, HookEvent::TurnEnd);
        assert!(hooks[0].matcher.is_none());
        assert!(hooks[0].timeout_secs.is_none());
    }

    #[test]
    fn parse_every_event_name() {
        for (name, event) in [
            ("pre_tool_use", HookEvent::PreToolUse),
            ("post_tool_use", HookEvent::PostToolUse),
            ("user_prompt_submit", HookEvent::UserPromptSubmit),
            ("session_start", HookEvent::SessionStart),
            ("session_end", HookEvent::SessionEnd),
            ("turn_end", HookEvent::TurnEnd),
        ] {
            let raw = format!("[[hooks]]\nevent = \"{name}\"\ncommand = \"true\"");
            let hooks = parse(&raw).expect("valid event name");
            assert_eq!(hooks[0].event, event);
            assert_eq!(event.name(), name, "name() round-trips the serde name");
        }
    }

    #[test]
    fn parse_rejects_unknown_event() {
        assert!(parse("[[hooks]]\nevent = \"on_boot\"\ncommand = \"true\"").is_err());
    }

    #[test]
    fn parse_empty_and_missing_hooks_key() {
        assert!(parse("").expect("empty file parses").is_empty());
        assert!(parse("# just a comment\n").expect("parses").is_empty());
    }

    #[test]
    fn load_files_appends_project_after_global_and_skips_bad_files() {
        let dir = std::env::temp_dir().join(format!("wizard-hooks-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let global = dir.join("global.toml");
        let project = dir.join("project.toml");
        let broken = dir.join("broken.toml");
        let missing = dir.join("missing.toml");
        std::fs::write(
            &global,
            "[[hooks]]\nevent = \"turn_end\"\ncommand = \"global\"",
        )
        .unwrap();
        std::fs::write(
            &project,
            "[[hooks]]\nevent = \"turn_end\"\ncommand = \"project\"",
        )
        .unwrap();
        std::fs::write(&broken, "this is not toml [[[").unwrap();

        let defs = load_files(&[global, broken, missing, project]);
        let commands: Vec<&str> = defs.iter().map(|d| d.command.as_str()).collect();
        assert_eq!(commands, vec!["global", "project"], "order preserved");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn matcher_glob_filters_tool_events() {
        let engine = engine(vec![
            def(HookEvent::PreToolUse, Some("execute"), "exact"),
            def(HookEvent::PreToolUse, Some("git_*"), "glob"),
            def(HookEvent::PreToolUse, None, "all"),
            def(HookEvent::PostToolUse, Some("execute"), "post"),
        ]);
        assert_eq!(
            engine.matching_commands(HookEvent::PreToolUse, Some("execute")),
            vec!["exact", "all"]
        );
        assert_eq!(
            engine.matching_commands(HookEvent::PreToolUse, Some("git_status")),
            vec!["glob", "all"]
        );
        assert_eq!(
            engine.matching_commands(HookEvent::PreToolUse, Some("read_file")),
            vec!["all"]
        );
        assert_eq!(
            engine.matching_commands(HookEvent::PostToolUse, Some("execute")),
            vec!["post"]
        );
    }

    #[test]
    fn matcher_is_ignored_for_non_tool_events() {
        // A matcher on a non-tool event is meaningless and matches anyway.
        let engine = engine(vec![def(HookEvent::SessionStart, Some("execute"), "start")]);
        assert_eq!(
            engine.matching_commands(HookEvent::SessionStart, None),
            vec!["start"]
        );
    }

    #[test]
    fn invalid_matcher_glob_drops_the_hook() {
        let engine = engine(vec![
            def(HookEvent::PreToolUse, Some("a{"), "bad"),
            def(HookEvent::PreToolUse, None, "good"),
        ]);
        assert_eq!(
            engine.matching_commands(HookEvent::PreToolUse, Some("execute")),
            vec!["good"]
        );
    }

    #[test]
    fn updated_args_requires_a_json_object_under_the_key() {
        assert_eq!(
            parse_updated_args(r#"{"updated_args": {"path": "b.rs"}}"#),
            Some(json!({"path": "b.rs"}))
        );
        assert!(parse_updated_args("").is_none());
        assert!(parse_updated_args("all clear").is_none());
        assert!(parse_updated_args(r#"{"other": 1}"#).is_none());
        assert!(parse_updated_args(r#"{"updated_args": "nope"}"#).is_none());
    }

    #[test]
    fn cap_tool_output_truncates_on_char_boundaries() {
        assert_eq!(cap_tool_output("short"), "short");
        let long = "é".repeat(TOOL_OUTPUT_CAP_BYTES); // 2 bytes per char
        let capped = cap_tool_output(&long);
        assert!(capped.len() <= TOOL_OUTPUT_CAP_BYTES + 32);
        assert!(capped.ends_with("[tool output truncated]"));
    }

    /// A hook that never reads its stdin cannot outlive its `timeout_secs`.
    ///
    /// The payload here is bigger than a pipe buffer (64 KiB on Linux), which
    /// a `pre_tool_use` payload reaches the first time a `write_file` of any
    /// real size is hooked, because the tool's arguments travel in it. The
    /// write used to sit *before* the timeout, so past the buffer it blocked
    /// until the hook read — and this one never does. `timeout_secs` had not
    /// started, so nothing bounded it: the module contract says a broken hook
    /// can never wedge the agent, and a hook that just ignores stdin did.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_hook_that_never_reads_a_large_payload_still_hits_its_timeout() {
        let engine = HookEngine::new(
            vec![HookDef {
                event: HookEvent::PreToolUse,
                matcher: None,
                command: "sleep 30".to_string(),
                timeout_secs: Some(1),
            }],
            std::env::temp_dir(),
            "test-session".to_string(),
        );
        let args = json!({ "content": "x".repeat(512 * 1024) });

        let started = std::time::Instant::now();
        let payload = json!({ "tool_name": "write_file", "args": args });
        let result = engine.execute(&engine.hooks[0], &payload).await;
        let elapsed = started.elapsed();

        match result {
            engine::RunResult::Failed(why) => assert!(why.contains("timed out"), "{why}"),
            engine::RunResult::Exited { code, .. } => panic!("expected a timeout, got exit {code}"),
        }
        assert!(
            elapsed < Duration::from_secs(10),
            "the timeout did not bound the run: {elapsed:?}"
        );
    }

    // A `cat` hook echoes the stdin payload back as appended context, so
    // these tests observe exactly what a hook receives.

    /// A project root with a `session_start` hook that would `touch` the
    /// returned sentinel path. Whether the sentinel exists afterwards is the
    /// only honest test of "did the hook execute".
    fn project_with_session_start_hook() -> (PathBuf, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("wizard-hooks-trust-{}", uuid::Uuid::new_v4()));
        let sentinel = root.join("fired");
        let hooks = root.join(".wizard").join("hooks.toml");
        std::fs::create_dir_all(hooks.parent().expect("has parent")).unwrap();
        std::fs::write(
            &hooks,
            format!(
                "[[hooks]]\nevent = \"session_start\"\ncommand = \"touch {}\"\n",
                sentinel.display()
            ),
        )
        .unwrap();
        (root, sentinel)
    }

    /// [`load`] with the environment opt-in pinned off, which is what every
    /// surface sees on a machine that has not exported `WIZARD_TRUST_PROJECT`.
    /// `cargo test` inherits the developer's (or the CI runner's) environment,
    /// and `docs/hooks.md` recommends exporting the variable on unattended
    /// machines, so a test about an *undecided* project has to say which
    /// answer it is testing rather than inherit one.
    fn load_undecided(root: &Path) -> Vec<HookDef> {
        load_project(root, false, &log_refusal)
    }

    #[tokio::test]
    async fn an_untrusted_projects_session_start_hook_never_runs() {
        let (root, sentinel) = project_with_session_start_hook();
        crate::trust::record(&root, crate::trust::Decision::Deny).expect("record the refusal");

        // `load`, the production entry point: a recorded refusal outranks the
        // environment opt-in, so this holds whatever the suite was started
        // with.
        let defs = load(&root);
        assert!(
            defs.is_empty(),
            "an untrusted project contributes no hooks: {defs:?}"
        );
        let engine = HookEngine::new(defs, root.clone(), "test-session".to_string());
        engine.session_start(Mode::default(), None).await;
        assert!(
            !sentinel.exists(),
            "the hook of an untrusted project must not execute"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_trusted_projects_session_start_hook_still_runs() {
        let (root, sentinel) = project_with_session_start_hook();
        crate::trust::record(&root, crate::trust::Decision::Trust).expect("record the approval");

        let defs = load(&root);
        assert_eq!(defs.len(), 1, "the project hook is loaded: {defs:?}");
        let engine = HookEngine::new(defs, root.clone(), "test-session".to_string());
        engine.session_start(Mode::default(), None).await;
        assert!(
            sentinel.exists(),
            "a trusted project's hook fires as before"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn approving_a_project_at_the_prompt_lets_its_session_start_hook_run() {
        // The other half of the gate, and the one that was unreachable while
        // no caller declared a console: a project the user says yes to runs its
        // hooks. The answer travels the real path: the gate reads the surface,
        // records the decision, and writes the store that `load` reads back
        // through `status`, with only the human scripted.
        let (root, sentinel) = project_with_session_start_hook();
        assert!(
            matches!(
                crate::trust::answer_for_test(&root, true),
                crate::trust::Gate::Allowed(_)
            ),
            "answering yes allows"
        );
        assert_eq!(crate::trust::status(&root), crate::trust::Status::Trusted);

        let defs = load(&root);
        assert_eq!(defs.len(), 1, "the approved project's hook loads: {defs:?}");
        let engine = HookEngine::new(defs, root.clone(), "test-session".to_string());
        engine.session_start(Mode::default(), None).await;
        assert!(
            sentinel.exists(),
            "an approved project's session_start hook actually executes"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn refusing_a_project_at_the_prompt_keeps_its_session_start_hook_off() {
        // The mirror of the test above, through the same path: a no is
        // recorded, and it is a decision rather than a re-ask.
        let (root, sentinel) = project_with_session_start_hook();
        assert!(matches!(
            crate::trust::answer_for_test(&root, false),
            crate::trust::Gate::Refused(_)
        ));
        assert_eq!(crate::trust::status(&root), crate::trust::Status::Denied);

        let defs = load(&root);
        assert!(defs.is_empty(), "a refused project loads nothing: {defs:?}");
        let engine = HookEngine::new(defs, root.clone(), "test-session".to_string());
        engine.session_start(Mode::default(), None).await;
        assert!(!sentinel.exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_refused_project_contributes_no_hooks_on_any_rebuild() {
        let (root, sentinel) = project_with_session_start_hook();
        crate::trust::record(&root, crate::trust::Decision::Deny).expect("record the refusal");

        // `load` runs again on every agent rebuild (`/model`, a provider
        // switch, `/fusion`, crash recovery), and each of those must be silent
        // about everything except the one report. Reaching the end of this
        // loop at all is half the assertion: a gate that prompted here would
        // sit on stdin forever.
        let reported = std::cell::RefCell::new(Vec::new());
        let report = |why: &str| reported.borrow_mut().push(why.to_string());
        for _ in 0..3 {
            let defs = load_project(&root, false, &report);
            assert!(
                defs.is_empty(),
                "a refused project contributes no hooks, ever: {defs:?}"
            );
        }

        let reported = reported.into_inner();
        assert_eq!(reported.len(), 3, "reported once per load: {reported:?}");
        assert!(
            reported
                .iter()
                .all(|why| why.contains("not running project hooks")),
            "{reported:?}"
        );
        assert!(!sentinel.exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_refusal_never_reaches_the_terminal() {
        // Structural, deliberately. The defect this guards against is a print
        // that staircases across the TUI's alternate screen on every agent
        // rebuild, and nothing a unit test can observe in-process tells "wrote
        // to fd 2" from "wrote to the log" without hijacking fd 2 for every
        // other test in the binary. So assert on this module's own source: it
        // prints nowhere, and the reporter it hands `load_project` is the log.
        let source = include_str!("mod.rs");
        // Everything above this module's own tests. Tests may print; the
        // module may not, and this assertion has to be able to name what it
        // is looking for without becoming the match itself.
        let (production, _) = source
            .split_once("#[cfg(test)]\nmod tests {")
            .expect("this module ends with its test module");
        // The needles are assembled rather than written out for the same
        // reason. The two cover all four print macros: the `e`-prefixed
        // spellings end in the same characters as the plain ones.
        for needle in [concat!("print", "ln!"), concat!("print", "!")] {
            assert!(
                !production.contains(needle),
                "a hook refusal must never go to the terminal: found {needle}"
            );
        }
        assert_eq!(
            production.matches("&log_refusal").count(),
            2,
            "one reporter, wired at the two entry points (load, load_specs)"
        );
    }

    #[tokio::test]
    async fn an_undecided_project_is_refused_when_there_is_nobody_to_ask() {
        // No console declared, which is the position of a sovereign run, the
        // gateway, the GUI server, CI, and every agent rebuild: default
        // untrusted, and nothing asked.
        let (root, sentinel) = project_with_session_start_hook();
        let defs = load_undecided(&root);
        assert!(defs.is_empty(), "no decision on record means no hooks");
        let engine = HookEngine::new(defs, root.clone(), "test-session".to_string());
        engine.session_start(Mode::default(), None).await;
        assert!(!sentinel.exists());
        assert_eq!(
            crate::trust::status(&root),
            crate::trust::Status::Unknown,
            "the unattended refusal is not recorded as the user's answer"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn editing_the_hooks_file_revokes_the_old_approval() {
        let (root, sentinel) = project_with_session_start_hook();
        crate::trust::record(&root, crate::trust::Decision::Trust).expect("record the approval");
        assert_eq!(load(&root).len(), 1, "approved as it stands");

        // Same path, edited command: the approval covered the old content
        // only. It still touches the sentinel, so a gate that let the edit
        // through would be caught below rather than merely unproven.
        std::fs::write(
            root.join(".wizard").join("hooks.toml"),
            format!(
                "[[hooks]]\nevent = \"session_start\"\ncommand = \"touch {} && echo pwned\"\n",
                sentinel.display()
            ),
        )
        .unwrap();
        let defs = load_undecided(&root);
        assert!(
            defs.is_empty(),
            "the edited file is a new question: {defs:?}"
        );
        let engine = HookEngine::new(defs, root.clone(), "test-session".to_string());
        engine.session_start(Mode::default(), None).await;
        assert!(!sentinel.exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Every append still reaches the model; only the *notice* is deduplicated.
    /// A `post_tool_use` hook fires on every tool call, so reporting each one
    /// puts a line in the transcript per tool call for a hook doing exactly
    /// what it was configured to do.
    #[tokio::test]
    async fn a_repeated_append_is_reported_once_per_session() {
        let engine = engine(vec![def(HookEvent::PostToolUse, None, "echo extra")]);
        let (tx, mut rx) = mpsc::channel(16);

        for _ in 0..3 {
            let extra = engine
                .post_tool_use_with_output(
                    "read_file",
                    &json!({}),
                    "",
                    false,
                    Mode::default(),
                    Some(&tx),
                )
                .await;
            assert_eq!(
                extra.as_deref(),
                Some("extra"),
                "the context itself is never suppressed"
            );
        }
        drop(tx);

        let mut fired = Vec::new();
        while let Some(event) = rx.recv().await {
            fired.push(event);
        }
        assert_eq!(fired.len(), 1, "three appends, one notice: {fired:?}");
        assert!(matches!(
            &fired[0],
            AgentEvent::HookFired { outcome, .. } if *outcome == HookOutcome::AppendedContext
        ));
    }

    /// `/clear` hands the user an empty transcript, so the note that a hook is
    /// feeding the model text is worth making again in the session after it.
    #[tokio::test]
    async fn clearing_the_session_lets_the_append_be_reported_again() {
        let engine = engine(vec![def(HookEvent::PostToolUse, None, "echo extra")]);
        let (tx, mut rx) = mpsc::channel(16);
        let fire = async |tx: &mpsc::Sender<AgentEvent>| {
            engine
                .post_tool_use_with_output(
                    "read_file",
                    &json!({}),
                    "",
                    false,
                    Mode::default(),
                    Some(tx),
                )
                .await;
        };

        fire(&tx).await;
        fire(&tx).await;
        engine.set_session_id("a-new-session".to_string());
        fire(&tx).await;
        drop(tx);

        let mut fired = 0;
        while rx.recv().await.is_some() {
            fired += 1;
        }
        assert_eq!(fired, 2, "one notice per session, not one in total");
    }

    /// A warning describes something that varies per call — a different exit
    /// code, a different reason — so suppressing the repeat would hide the
    /// occurrence that mattered. Only the append is filtered.
    #[tokio::test]
    async fn a_repeated_warning_is_reported_every_time() {
        let engine = engine(vec![def(HookEvent::PostToolUse, None, "exit 3")]);
        let (tx, mut rx) = mpsc::channel(16);

        for _ in 0..3 {
            engine
                .post_tool_use_with_output(
                    "read_file",
                    &json!({}),
                    "",
                    false,
                    Mode::default(),
                    Some(&tx),
                )
                .await;
        }
        drop(tx);

        let mut fired = 0;
        while rx.recv().await.is_some() {
            fired += 1;
        }
        assert_eq!(fired, 3);
    }

    #[tokio::test]
    async fn user_prompt_submit_with_prompt_puts_the_prompt_in_the_payload() {
        let engine = engine(vec![def(HookEvent::UserPromptSubmit, None, "cat")]);
        match engine
            .user_prompt_submit_with_prompt("deploy to prod", Mode::default(), None)
            .await
        {
            PromptSubmit::Continue(Some(payload)) => {
                assert!(
                    payload.contains(r#""event":"user_prompt_submit""#),
                    "{payload}"
                );
                assert!(
                    payload.contains(r#""prompt":"deploy to prod""#),
                    "{payload}"
                );
            }
            other => panic!("expected appended context, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn post_tool_use_with_output_puts_the_result_in_the_payload() {
        let engine = engine(vec![def(HookEvent::PostToolUse, None, "cat")]);
        let payload = engine
            .post_tool_use_with_output(
                "execute",
                &json!({ "command": "true" }),
                "exit code: 3",
                true,
                Mode::default(),
                None,
            )
            .await
            .expect("cat echoes the payload");
        assert!(
            payload.contains(r#""tool_output":"exit code: 3""#),
            "{payload}"
        );
        assert!(payload.contains(r#""is_error":true"#), "{payload}");
        assert!(payload.contains(r#""tool_name":"execute""#), "{payload}");
    }

    #[tokio::test]
    async fn a_hook_runs_under_the_shell_the_model_is_told_about() {
        // `$0` is the name the running shell was invoked with, so this is the
        // hook executor reporting which interpreter it actually landed in.
        // `crate::agent::prompts` tells the model the same answer through
        // `platform::shell::name()`; a hook that runs under a different shell
        // than the one the prompt names is a command line written for the
        // wrong language.
        let engine = engine(vec![def(HookEvent::SessionStart, None, "printf %s \"$0\"")]);
        let context = engine
            .session_start(Mode::default(), None)
            .await
            .expect("the hook prints its shell's name");
        assert_eq!(context.trim(), crate::platform::shell::name());
    }

    #[test]
    fn the_hook_executor_spawns_through_the_platform_shell() {
        // Structural, like `the_refusal_never_reaches_the_terminal` above, and
        // for the same class of reason: on Unix a hand-written `sh -c` and
        // `platform::shell::tokio_command` are the same two strings, so no
        // in-process observation can tell them apart today. What they are not
        // is the same *decision*: the second one changes with the platform and
        // with what the system prompt claims, and the first one silently does
        // not. This module was the last hand-written `sh -c` in the tree.
        let source = include_str!("engine.rs");
        let (production, _) = source
            .split_once("#[cfg(test)]\nmod tests {")
            .expect("this module ends with its test module");
        assert!(
            !production.contains(concat!("Command::new(", "\"sh\")")),
            "hooks must not hand-write the shell; use platform::shell"
        );
        assert_eq!(
            production
                .matches("crate::platform::shell::tokio_command(")
                .count(),
            1,
            "one spawn site, through the platform shell"
        );
    }
}
