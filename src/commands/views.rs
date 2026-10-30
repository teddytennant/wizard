//! `/permissions` and `/hooks`: what the permission policy and the hook
//! engine hold right now, read from the same loaders a session uses.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::surface::SessionSnapshot;
use crate::config::Config;
use crate::hooks::{Handler, HookSpec};
use crate::permissions::{PermissionMode, Policy, Rule};

fn trust_line(root: &Path) -> &'static str {
    match crate::trust::status(root) {
        crate::trust::Status::Trusted => "trusted",
        crate::trust::Status::Denied => "denied",
        crate::trust::Status::Unknown => "no decision",
    }
}

/// How a mode reads in the report.
fn mode_note(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Default | PermissionMode::Auto => "no rule objects, every call runs",
        PermissionMode::AcceptEdits => "file edits run, other side effects need an allow rule",
        PermissionMode::Plan => "read-only until a plan is approved",
        PermissionMode::DontAsk => "only read-only calls and calls an allow rule names",
        PermissionMode::BypassPermissions => "nothing checked except deny and ask rules",
    }
}

/// Rules from the policy, grouped by where they were written.
fn rules_by_source(policy: &Policy) -> BTreeMap<String, Vec<(&'static str, &Rule)>> {
    let mut groups: BTreeMap<String, Vec<(&'static str, &Rule)>> = BTreeMap::new();
    for (action, rules) in [
        ("deny", &policy.deny),
        ("ask", &policy.ask),
        ("allow", &policy.allow),
    ] {
        for rule in rules {
            groups
                .entry(rule.source.clone())
                .or_default()
                .push((action, rule));
        }
    }
    groups
}

/// The policy the next session would run under, with the settings it came
/// from and any warnings they produced.
fn current_policy(root: &Path) -> (Policy, Vec<PathBuf>, Vec<String>) {
    let config = Config::load().unwrap_or_default();
    // The same loader a session uses, so a project's rules count once it is
    // trusted and not before.
    let (_, settings) = crate::hooks::load_specs(root);
    let policy = crate::permissions::build(&config, &settings, root);
    (policy, settings.sources, settings.warnings)
}

pub fn permissions(session: &SessionSnapshot, root: &Path, args: &str) -> Result<String, String> {
    let words: Vec<&str> = args.split_whitespace().collect();
    if let Some((action, rest)) = words.split_first()
        && matches!(*action, "allow" | "ask" | "deny" | "remove")
    {
        let rule = rest.join(" ");
        return edit_rule(action, &rule);
    }
    let (policy, sources, warnings) = current_policy(root);
    let plan = session.plan.plan || session.plan.omakase;
    let mut out = String::from("permissions\n");
    out.push_str(&format!(
        "  mode: {} ({})\n",
        policy.mode,
        mode_note(policy.mode)
    ));
    out.push_str(&format!(
        "  plan mode: {}\n  project trust ({}): {}\n",
        if plan { "on" } else { "off" },
        root.display(),
        trust_line(root)
    ));
    let groups = rules_by_source(&policy);
    if groups.is_empty() {
        out.push_str("\nno rules; every tool call runs\n");
    }
    for (source, rules) in &groups {
        out.push_str(&format!("\n{source}\n"));
        for (action, rule) in rules {
            out.push_str(&format!("  {action:<5} {}\n", rule.raw));
        }
    }
    if !sources.is_empty() {
        out.push_str("\nsettings read:\n");
        for source in &sources {
            out.push_str(&format!("  {}\n", source.display()));
        }
    }
    for warning in &warnings {
        out.push_str(&format!("warning: {warning}\n"));
    }
    out.push_str(
        "\n/permissions allow|ask|deny <rule> adds a rule to ~/.wizard/settings.json for the \
         next session; /permissions remove <rule> takes it out. Rules read like Claude Code's: \
         Bash(git status:*), Read(./secrets/**), mcp__server__tool.\n",
    );
    Ok(out)
}

fn user_settings_path() -> Result<PathBuf, String> {
    Config::wizard_dir()
        .map(|dir| dir.join("settings.json"))
        .map_err(|err| format!("{err:#}"))
}

fn read_settings(path: &Path) -> Result<Value, String> {
    match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw)
            .map_err(|err| format!("{} is not valid JSON ({err}); left alone", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(err) => Err(format!("{}: {err}", path.display())),
    }
}

fn edit_rule(action: &str, rule: &str) -> Result<String, String> {
    let rule = rule.trim();
    if rule.is_empty() {
        return Err(format!("usage: /permissions {action} <rule>"));
    }
    Rule::parse(rule, Path::new("."))?;
    let path = user_settings_path()?;
    let mut settings = read_settings(&path)?;
    let object = settings
        .as_object_mut()
        .ok_or_else(|| format!("{} is not a JSON object", path.display()))?;
    let permissions = object
        .entry("permissions")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or("\"permissions\" in settings.json is not an object")?;
    let message = if action == "remove" {
        let mut removed = 0;
        for list in ["allow", "ask", "deny"] {
            if let Some(items) = permissions.get_mut(list).and_then(Value::as_array_mut) {
                let before = items.len();
                items.retain(|item| item.as_str() != Some(rule));
                removed += before - items.len();
            }
        }
        if removed == 0 {
            return Err(format!("no rule `{rule}` in {}", path.display()));
        }
        format!("removed `{rule}` from {}", path.display())
    } else {
        let items = permissions
            .entry(action)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| format!("\"permissions.{action}\" is not a list"))?;
        if items.iter().any(|item| item.as_str() == Some(rule)) {
            return Ok(format!("`{rule}` is already in the {action} list"));
        }
        items.push(json!(rule));
        format!("{action} `{rule}` added to {}", path.display())
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text = serde_json::to_string_pretty(&settings).map_err(|err| err.to_string())?;
    std::fs::write(&path, format!("{text}\n"))
        .map_err(|err| format!("{}: {err}", path.display()))?;
    Ok(format!("{message}\nit applies from the next session"))
}

fn handler_label(handler: &Handler) -> &'static str {
    match handler {
        Handler::Command { .. } => "command",
        Handler::Http { .. } => "http",
        Handler::Prompt { .. } => "prompt",
        Handler::Agent { .. } => "agent",
        Handler::McpTool { .. } => "mcp_tool",
    }
}

fn hook_line(spec: &HookSpec) -> String {
    let mut line = format!(
        "  {:<14} {:<8} {}",
        spec.matcher.as_deref().unwrap_or("*"),
        handler_label(&spec.handler),
        spec.handler.describe()
    );
    let mut notes = Vec::new();
    if spec.is_async {
        notes.push("async".to_string());
    }
    if let Some(seconds) = spec.timeout_secs {
        notes.push(format!("{seconds}s"));
    }
    if let Some(condition) = &spec.condition {
        notes.push(format!("if {condition}"));
    }
    if !notes.is_empty() {
        line.push_str(&format!("  [{}]", notes.join(", ")));
    }
    line.push_str(&format!("  ({})", spec.source));
    line
}

pub fn hooks(root: &Path) -> String {
    let (specs, settings) = crate::hooks::load_specs(root);
    let mut out = String::from("hooks\n");
    if specs.is_empty() {
        out.push_str("  none configured\n");
    }
    let mut by_event: BTreeMap<String, Vec<&HookSpec>> = BTreeMap::new();
    for spec in &specs {
        by_event
            .entry(spec.event.claude_name().to_string())
            .or_default()
            .push(spec);
    }
    for (event, specs) in &by_event {
        out.push_str(&format!("\n{event}\n"));
        for spec in specs {
            out.push_str(&hook_line(spec));
            out.push('\n');
        }
    }
    if settings.disable_all_hooks {
        out.push_str("\ndisableAllHooks is set: Claude-format hooks are off\n");
    }
    out.push_str(
        "\nSources: ~/.wizard/hooks.toml, the project's .wizard/hooks.toml, the hooks key of \
         Claude-format settings files, and enabled plugins' hooks/hooks.json. Project hooks \
         wait for the project to be trusted.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_are_grouped_by_where_they_were_written() {
        let mut policy = Policy::new(PathBuf::from("/p"));
        policy.deny.push(
            Rule::parse("Bash(rm:*)", Path::new("/p"))
                .unwrap()
                .from_source("user settings"),
        );
        policy.allow.push(
            Rule::parse("Read", Path::new("/p"))
                .unwrap()
                .from_source("--allowedTools"),
        );
        policy.ask.push(
            Rule::parse("Bash(git push:*)", Path::new("/p"))
                .unwrap()
                .from_source("user settings"),
        );
        let groups = rules_by_source(&policy);
        assert_eq!(groups["user settings"].len(), 2);
        assert_eq!(groups["--allowedTools"][0].0, "allow");
    }

    #[test]
    fn a_hook_line_names_its_matcher_handler_and_source() {
        let spec = HookSpec {
            event: crate::hooks::HookEvent::PreToolUse,
            matcher: Some("Bash".into()),
            handler: Handler::Http {
                url: "https://example.test/h".into(),
                headers: Default::default(),
                allowed_env_vars: Vec::new(),
            },
            timeout_secs: Some(5),
            origin: crate::hooks::Origin::Claude,
            json: true,
            condition: Some("Bash(git *)".into()),
            is_async: true,
            block_on_failure: false,
            source: "user settings".into(),
        };
        let line = hook_line(&spec);
        assert!(line.contains("Bash"), "{line}");
        assert!(line.contains("http"), "{line}");
        assert!(line.contains("async, 5s, if Bash(git *)"), "{line}");
        assert!(line.ends_with("(user settings)"), "{line}");
    }

    #[test]
    fn adding_and_removing_a_rule_edits_the_user_settings_file() {
        // The test config dir is shared with the rest of the suite, so work on
        // the path directly.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        let mut settings = read_settings(&path).unwrap();
        assert_eq!(settings, json!({}));
        settings["permissions"] = json!({"deny": ["Bash(rm:*)"]});
        std::fs::write(&path, settings.to_string()).unwrap();
        let reread = read_settings(&path).unwrap();
        assert_eq!(reread["permissions"]["deny"][0], "Bash(rm:*)");
        assert!(
            read_settings(tmp.path()).is_err(),
            "a directory is not a settings file"
        );
    }

    #[test]
    fn a_rule_that_does_not_parse_is_refused_before_any_file_is_touched() {
        let err = edit_rule("allow", "Bash(x) junk").unwrap_err();
        assert!(err.contains("closing parenthesis"), "{err}");
        assert!(edit_rule("deny", "  ").unwrap_err().starts_with("usage"));
    }
}
