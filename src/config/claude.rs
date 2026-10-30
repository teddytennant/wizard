//! Claude Code `settings.json` files, read for the keys that mean something
//! to Wizard.
//!
//! Layers, lowest precedence first:
//!
//! 1. `~/.claude/settings.json` (or `$CLAUDE_CONFIG_DIR/settings.json`)
//! 2. `~/.wizard/settings.json`
//! 3. `<project>/.claude/settings.json`
//! 4. `<project>/.wizard/settings.json`
//! 5. `<project>/.claude/settings.local.json`
//! 6. `<project>/.wizard/settings.local.json`
//! 7. `/etc/claude-code/managed-settings.json` and `/etc/wizard/managed-settings.json`
//!
//! Lists (`permissions.allow`, `ask`, `deny`, hooks) concatenate across
//! layers; scalars and `env` entries are won by the later layer. A project's
//! files hand Wizard code to run (`hooks`, `env`, `statusLine`, helper
//! commands), so they sit behind the same trust gate as `.wizard/hooks.toml`;
//! see [`crate::trust`]. Without trust a project still contributes its `deny`
//! and `ask` rules, which can only narrow what Wizard does.
//!
//! `model` is honored only from the Wizard-named files: a Claude model id
//! means nothing to a Grok or local provider, so a shared `.claude/` file
//! must not pick Wizard's model.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::hooks::spec::{self, HookSpec};
use crate::permissions::{PermissionMode, Rule};
use crate::trust::Surface;

/// Keys that make a project settings file executable. A file with none of
/// them is only data and is not part of the trust surface.
pub const EXECUTABLE_KEYS: [&str; 7] = [
    "hooks",
    "env",
    "statusLine",
    "apiKeyHelper",
    "awsAuthRefresh",
    "awsCredentialExport",
    "otelHeadersHelper",
];

/// True when `bytes` is a settings file carrying any [`EXECUTABLE_KEYS`].
pub fn is_executable(bytes: &[u8]) -> bool {
    serde_json::from_slice::<Value>(bytes)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .is_some_and(|map| EXECUTABLE_KEYS.iter().any(|key| map.contains_key(*key)))
}

/// Project-relative settings files, lowest precedence first.
pub const PROJECT_FILES: [&str; 4] = [
    ".claude/settings.json",
    ".wizard/settings.json",
    ".claude/settings.local.json",
    ".wizard/settings.local.json",
];

/// Everything read from the layers that Wizard acts on.
#[derive(Debug, Default)]
pub struct ClaudeSettings {
    pub env: BTreeMap<String, String>,
    pub model: Option<String>,
    pub default_mode: Option<PermissionMode>,
    pub allow: Vec<Rule>,
    pub ask: Vec<Rule>,
    pub deny: Vec<Rule>,
    pub hooks: Vec<HookSpec>,
    pub disable_all_hooks: bool,
    pub status_line: Option<Value>,
    pub output_style: Option<String>,
    /// Files that were read, lowest precedence first.
    pub sources: Vec<PathBuf>,
    /// Things a user should hear about (a rule that did not parse, a layer
    /// that was not JSON).
    pub warnings: Vec<String>,
}

/// Whether a project's executable files may be used.
#[derive(Clone, Copy)]
pub enum Trust<'a> {
    /// The trust gate approved this exact surface.
    Approved(&'a Surface),
    /// Refused or undecided: take only what cannot run code.
    Refused,
}

/// One file to read.
struct Layer {
    path: PathBuf,
    label: String,
    /// Project-relative name, for fetching approved bytes from the surface.
    project_rel: Option<&'static str>,
    honors_model: bool,
}

/// Where the user-level Claude directory is.
pub fn claude_dir() -> Option<PathBuf> {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => dirs::home_dir().map(|home| home.join(".claude")),
    }
}

fn layers(project_root: &Path, user_dirs: &[(PathBuf, bool)], managed: &[PathBuf]) -> Vec<Layer> {
    let mut out = Vec::new();
    for (dir, honors_model) in user_dirs {
        out.push(Layer {
            path: dir.join("settings.json"),
            label: format!("user settings ({})", dir.join("settings.json").display()),
            project_rel: None,
            honors_model: *honors_model,
        });
    }
    for rel in PROJECT_FILES {
        let local = rel.contains(".local.");
        out.push(Layer {
            path: project_root.join(rel),
            label: format!(
                "{} ({rel})",
                if local {
                    "local settings"
                } else {
                    "project settings"
                }
            ),
            project_rel: Some(rel),
            honors_model: false,
        });
    }
    for path in managed {
        out.push(Layer {
            path: path.clone(),
            label: format!("managed settings ({})", path.display()),
            project_rel: None,
            honors_model: false,
        });
    }
    out
}

/// Load the layers for `project_root` with the real user and system paths.
pub fn load(project_root: &Path, trust: Trust<'_>) -> ClaudeSettings {
    let mut user_dirs = Vec::new();
    if let Some(dir) = claude_dir() {
        user_dirs.push((dir, false));
    }
    if let Ok(dir) = super::Config::wizard_dir() {
        user_dirs.push((dir, true));
    }
    let managed = vec![
        PathBuf::from("/etc/claude-code/managed-settings.json"),
        PathBuf::from("/etc/wizard/managed-settings.json"),
    ];
    load_from(project_root, &user_dirs, &managed, trust)
}

/// [`load`] with the user directories and managed files given, for tests.
pub fn load_from(
    project_root: &Path,
    user_dirs: &[(PathBuf, bool)],
    managed: &[PathBuf],
    trust: Trust<'_>,
) -> ClaudeSettings {
    let mut settings = ClaudeSettings::default();
    for layer in layers(project_root, user_dirs, managed) {
        let approved = match (trust, layer.project_rel) {
            (Trust::Approved(surface), Some(rel)) => surface.contents_of(rel).map(<[u8]>::to_vec),
            _ => None,
        };
        let bytes = match approved.or_else(|| std::fs::read(&layer.path).ok()) {
            Some(bytes) => bytes,
            None => continue,
        };
        let value: Value = match serde_json::from_slice(&bytes) {
            Ok(Value::Object(map)) => Value::Object(map),
            _ => {
                settings.warnings.push(format!(
                    "{}: not a JSON object, skipped",
                    layer.path.display()
                ));
                continue;
            }
        };
        let untrusted_project = layer.project_rel.is_some() && matches!(trust, Trust::Refused);
        settings.sources.push(layer.path.clone());
        merge_layer(
            &mut settings,
            &value,
            &layer,
            project_root,
            untrusted_project,
        );
    }
    if settings.disable_all_hooks {
        settings.hooks.clear();
    }
    settings
}

fn merge_layer(
    out: &mut ClaudeSettings,
    value: &Value,
    layer: &Layer,
    root: &Path,
    untrusted_project: bool,
) {
    if !untrusted_project {
        if let Some(env) = value.get("env").and_then(Value::as_object) {
            for (key, val) in env {
                if let Some(text) = scalar_text(val) {
                    out.env.insert(key.clone(), text);
                }
            }
        }
        if let Some(hooks) = value.get("hooks") {
            out.hooks
                .extend(spec::from_claude_settings(hooks, &layer.label));
        }
        if let Some(line) = value.get("statusLine") {
            out.status_line = Some(line.clone());
        }
    }
    if layer.honors_model
        && let Some(model) = value.get("model").and_then(Value::as_str)
    {
        out.model = Some(model.to_string());
    }
    if let Some(style) = value.get("outputStyle").and_then(Value::as_str) {
        out.output_style = Some(style.to_string());
    }
    if value.get("disableAllHooks").and_then(Value::as_bool) == Some(true) {
        out.disable_all_hooks = true;
    }
    let Some(perms) = value.get("permissions") else {
        return;
    };
    let mut read = |key: &str, into: fn(&mut ClaudeSettings) -> &mut Vec<Rule>| {
        for raw in perms
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(raw) = raw.as_str() else { continue };
            match Rule::parse(raw, root) {
                Ok(rule) => into(out).push(rule.from_source(&layer.label)),
                Err(why) => out.warnings.push(format!("{}: {why}", layer.label)),
            }
        }
    };
    // Narrowing rules always apply. An untrusted repository cannot widen
    // what Wizard may do.
    read("deny", |s| &mut s.deny);
    read("ask", |s| &mut s.ask);
    if !untrusted_project {
        read("allow", |s| &mut s.allow);
    }
    if let Some(mode) = perms.get("defaultMode").and_then(Value::as_str) {
        match mode.parse::<PermissionMode>() {
            // An untrusted project may make Wizard stricter, never looser.
            Ok(mode)
                if !untrusted_project
                    || matches!(mode, PermissionMode::Plan | PermissionMode::DontAsk) =>
            {
                out.default_mode = Some(mode);
            }
            Ok(_) => {}
            Err(why) => out.warnings.push(format!("{}: {why}", layer.label)),
        }
    }
}

fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dirs {
        user: PathBuf,
        wizard: PathBuf,
        project: PathBuf,
        _guard: tempfile::TempDir,
    }

    fn dirs() -> Dirs {
        let guard = tempfile::tempdir().unwrap();
        let base = guard.path().to_path_buf();
        let d = Dirs {
            user: base.join("claude"),
            wizard: base.join("wizard"),
            project: base.join("proj"),
            _guard: guard,
        };
        for dir in [
            &d.user,
            &d.wizard,
            &d.project.join(".claude"),
            &d.project.join(".wizard"),
        ] {
            std::fs::create_dir_all(dir).unwrap();
        }
        d
    }

    fn write(path: PathBuf, json: &str) {
        std::fs::write(path, json).unwrap();
    }

    fn run(d: &Dirs, trust: Trust<'_>) -> ClaudeSettings {
        load_from(
            &d.project,
            &[(d.user.clone(), false), (d.wizard.clone(), true)],
            &[],
            trust,
        )
    }

    #[test]
    fn lists_concatenate_and_scalars_take_the_later_layer() {
        let d = dirs();
        write(
            d.user.join("settings.json"),
            r#"{"env":{"A":"user","B":"user"},"permissions":{"deny":["Read(.env)"],"allow":["Bash(ls:*)"],"defaultMode":"acceptEdits"},"outputStyle":"terse"}"#,
        );
        write(
            d.project.join(".claude/settings.local.json"),
            r#"{"env":{"B":"local"},"permissions":{"deny":["Bash(rm:*)"],"defaultMode":"dontAsk"},"outputStyle":"verbose"}"#,
        );
        let s = run(&d, Trust::Approved(&Surface::default()));
        assert_eq!(s.env["A"], "user");
        assert_eq!(s.env["B"], "local");
        assert_eq!(s.deny.len(), 2);
        assert_eq!(s.allow.len(), 1);
        assert_eq!(s.default_mode, Some(PermissionMode::DontAsk));
        assert_eq!(s.output_style.as_deref(), Some("verbose"));
    }

    #[test]
    fn precedence_runs_user_project_local() {
        let d = dirs();
        write(d.user.join("settings.json"), r#"{"env":{"X":"1"}}"#);
        write(
            d.project.join(".claude/settings.json"),
            r#"{"env":{"X":"2"}}"#,
        );
        write(
            d.project.join(".claude/settings.local.json"),
            r#"{"env":{"X":"3"}}"#,
        );
        let s = run(&d, Trust::Approved(&Surface::default()));
        assert_eq!(s.env["X"], "3");
    }

    #[test]
    fn model_comes_only_from_wizard_named_files() {
        let d = dirs();
        write(d.user.join("settings.json"), r#"{"model":"opus"}"#);
        write(
            d.project.join(".claude/settings.json"),
            r#"{"model":"sonnet"}"#,
        );
        let s = run(&d, Trust::Approved(&Surface::default()));
        assert_eq!(s.model, None);
        write(d.wizard.join("settings.json"), r#"{"model":"grok-4.3"}"#);
        let s = run(&d, Trust::Approved(&Surface::default()));
        assert_eq!(s.model.as_deref(), Some("grok-4.3"));
    }

    #[test]
    fn an_untrusted_project_keeps_deny_and_ask_and_loses_the_rest() {
        let d = dirs();
        write(
            d.project.join(".claude/settings.json"),
            r#"{"env":{"EVIL":"1"},"hooks":{"Stop":[{"hooks":[{"type":"command","command":"x"}]}]},
               "permissions":{"deny":["Bash(curl:*)"],"ask":["Bash(git push:*)"],"allow":["Bash"],"defaultMode":"bypassPermissions"}}"#,
        );
        let s = run(&d, Trust::Refused);
        assert!(s.env.is_empty());
        assert!(s.hooks.is_empty());
        assert!(s.allow.is_empty());
        assert_eq!(s.default_mode, None);
        assert_eq!(s.deny.len(), 1);
        assert_eq!(s.ask.len(), 1);
    }

    #[test]
    fn user_files_are_trusted_even_when_the_project_is_not() {
        let d = dirs();
        write(
            d.user.join("settings.json"),
            r#"{"env":{"OK":"1"},"hooks":{"Stop":[{"hooks":[{"type":"command","command":"x"}]}]}}"#,
        );
        let s = run(&d, Trust::Refused);
        assert_eq!(s.env["OK"], "1");
        assert_eq!(s.hooks.len(), 1);
    }

    #[test]
    fn disable_all_hooks_drops_claude_hooks() {
        let d = dirs();
        write(
            d.user.join("settings.json"),
            r#"{"disableAllHooks":true,"hooks":{"Stop":[{"hooks":[{"type":"command","command":"x"}]}]}}"#,
        );
        assert!(run(&d, Trust::Refused).hooks.is_empty());
    }

    #[test]
    fn bad_rules_and_bad_files_warn_and_do_not_stop_loading() {
        let d = dirs();
        write(d.user.join("settings.json"), "not json");
        write(
            d.wizard.join("settings.json"),
            r#"{"permissions":{"deny":["Bash(x) junk","Read(.env)"]}}"#,
        );
        let s = run(&d, Trust::Refused);
        assert_eq!(s.deny.len(), 1);
        assert_eq!(s.warnings.len(), 2);
    }

    #[test]
    fn only_files_with_executable_keys_are_executable() {
        assert!(is_executable(br#"{"hooks":{}}"#));
        assert!(is_executable(br#"{"env":{"A":"1"}}"#));
        assert!(!is_executable(br#"{"permissions":{"deny":["Bash"]}}"#));
        assert!(!is_executable(b"nope"));
    }
}
