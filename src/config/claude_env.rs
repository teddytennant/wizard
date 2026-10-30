//! Claude Code's environment variables, mapped onto what Wizard has.
//!
//! Only the ones with a natural Wizard meaning are read; the audit table in
//! `docs/claude-code-parity.md` names every other one and why it does
//! nothing. Everything here takes a `get` closure instead of reading the
//! process environment, so the mapping is testable without touching it.
//!
//! Precedence, highest first: the command-line flag, the variable, the
//! settings file, Wizard's own config. The variable never beats a flag.

use std::collections::BTreeMap;

use super::{Config, ReasoningEffort};

/// A source of environment variables.
pub type Get<'a> = &'a dyn Fn(&str) -> Option<String>;

fn value(get: Get, name: &str) -> Option<String> {
    get(name)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Claude Code's reading of a switch: set, non-empty, and not `0`/`false`.
pub fn truthy(get: Get, name: &str) -> bool {
    value(get, name).is_some_and(|v| {
        !matches!(
            v.to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        )
    })
}

/// `MAX_THINKING_TOKENS` as an effort level. Wizard has levels, not a token
/// budget, so this is a banding: nothing or a tiny budget is `low`, and each
/// doubling of the budget is a step up.
pub fn effort_from_thinking_budget(tokens: u64) -> ReasoningEffort {
    match tokens {
        0..=4_096 => ReasoningEffort::Low,
        4_097..=16_384 => ReasoningEffort::Medium,
        16_385..=32_768 => ReasoningEffort::High,
        _ => ReasoningEffort::Xhigh,
    }
}

/// The effort level the environment asks for: `CLAUDE_CODE_EFFORT_LEVEL`
/// (`low|medium|high|xhigh|max`), else a banding of `MAX_THINKING_TOKENS`.
pub fn effort(get: Get) -> Option<String> {
    if let Some(level) = value(get, "CLAUDE_CODE_EFFORT_LEVEL") {
        return Some(level);
    }
    let budget: u64 = value(get, "MAX_THINKING_TOKENS")?.parse().ok()?;
    Some(
        match effort_from_thinking_budget(budget) {
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
            ReasoningEffort::Xhigh => "xhigh",
        }
        .to_string(),
    )
}

/// `CLAUDE_CODE_MAX_TURNS`.
pub fn max_turns(get: Get) -> Option<u32> {
    value(get, "CLAUDE_CODE_MAX_TURNS")?
        .parse()
        .ok()
        .filter(|turns| *turns > 0)
}

/// `ANTHROPIC_DEFAULT_<TIER>_MODEL` and the old `ANTHROPIC_SMALL_FAST_MODEL`,
/// as the alias-to-model map `modelOverrides` also fills: `--model opus`
/// means whatever the variable names.
pub fn model_aliases(get: Get) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (alias, var) in [
        ("opus", "ANTHROPIC_DEFAULT_OPUS_MODEL"),
        ("sonnet", "ANTHROPIC_DEFAULT_SONNET_MODEL"),
        ("haiku", "ANTHROPIC_DEFAULT_HAIKU_MODEL"),
        ("fable", "ANTHROPIC_DEFAULT_FABLE_MODEL"),
    ] {
        if let Some(model) = value(get, var) {
            out.insert(alias.to_string(), model);
        }
    }
    if let Some(model) = value(get, "ANTHROPIC_SMALL_FAST_MODEL") {
        out.entry("haiku".to_string()).or_insert(model);
    }
    out
}

fn anthropic_kind(config: &Config) -> bool {
    config
        .active_provider
        .as_ref()
        .and_then(|name| config.providers.iter().find(|p| &p.name == name))
        .or(config.providers.first())
        .is_some_and(|p| p.kind == crate::config::ProviderKind::ANTHROPIC)
}

/// Apply the variables that set a plain config value. Run before the
/// command-line flags, which then overwrite whatever they name.
pub fn apply(config: &mut Config, get: Get) {
    if anthropic_kind(config) {
        let at = config
            .active_provider
            .as_ref()
            .and_then(|name| config.providers.iter().position(|p| &p.name == name))
            .unwrap_or(0);
        if let Some(provider) = config.providers.get_mut(at) {
            if let Some(url) = value(get, "ANTHROPIC_BASE_URL") {
                provider.base_url = url;
            }
            if let Some(model) = value(get, "ANTHROPIC_MODEL") {
                provider.model = model;
                provider.model_pinned = Some(true);
            }
        }
    }
    if let Some(ms) = value(get, "BASH_DEFAULT_TIMEOUT_MS").and_then(|v| v.parse::<u64>().ok())
        && ms > 0
    {
        config.shell.timeout_secs = ms.div_ceil(1000);
    }
    if let Some(shell) = value(get, "CLAUDE_CODE_SHELL") {
        config.shell.program = Some(shell);
    }
    if truthy(get, "DISABLE_AUTOUPDATER") || truthy(get, "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC")
    {
        config.update.auto = false;
        config.update.notify = false;
    }
    if let Some(window) =
        value(get, "CLAUDE_CODE_AUTO_COMPACT_WINDOW").and_then(|v| v.parse::<u32>().ok())
    {
        config.max_context_tokens = window;
    } else if let Some(pct) =
        value(get, "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE").and_then(|v| v.parse::<u64>().ok())
        && (1..=100).contains(&pct)
        && config.max_context_tokens > 0
    {
        // Compaction starts at 80% of the window; asking for `pct` percent
        // moves that point, so the window is scaled by `pct / 80`.
        let scaled = u64::from(config.max_context_tokens) * pct / 80;
        config.max_context_tokens = u32::try_from(scaled).unwrap_or(u32::MAX);
    }
}

/// The `WIZARD_*` variables the Claude ones stand for, as `(name, value)`
/// pairs to set when the Wizard one is not already set.
pub fn publish(get: Get) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    if truthy(get, "CLAUDE_CODE_SAFE_MODE") {
        out.push((crate::cli_claude::env::SAFE_MODE, "1".to_string()));
        out.push((crate::cli_claude::env::BARE, "1".to_string()));
    }
    if let Some(dirs) = value(get, "CLAUDE_CODE_PLUGIN_DIRS") {
        out.push((crate::cli_claude::env::PLUGIN_DIRS, dirs));
    }
    if let Some(tmp) = value(get, "CLAUDE_CODE_TMPDIR").or_else(|| value(get, "CLAUDE_TMPDIR")) {
        out.push(("TMPDIR", tmp));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderConfig;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, v)| (*v).to_string())
        }
    }

    fn anthropic() -> Config {
        let mut config = Config::default();
        config.providers = vec![ProviderConfig {
            name: "claude".into(),
            kind: crate::config::ProviderKind::ANTHROPIC,
            ..ProviderConfig::default()
        }];
        config.active_provider = Some("claude".into());
        config
    }

    #[test]
    fn anthropic_variables_apply_only_to_an_anthropic_provider() {
        let get = env(&[
            ("ANTHROPIC_MODEL", "claude-x"),
            ("ANTHROPIC_BASE_URL", "http://127.0.0.1:9"),
        ]);
        let mut config = anthropic();
        apply(&mut config, &get);
        assert_eq!(config.providers[0].model, "claude-x");
        assert_eq!(config.providers[0].base_url, "http://127.0.0.1:9");

        let mut other = Config::default();
        let before = other.providers.clone();
        apply(&mut other, &get);
        assert_eq!(other.providers.len(), before.len());
        assert!(other.providers.iter().all(|p| p.model != "claude-x"));
    }

    #[test]
    fn bash_and_compaction_variables_set_config() {
        let mut config = Config::default();
        apply(
            &mut config,
            &env(&[
                ("BASH_DEFAULT_TIMEOUT_MS", "45001"),
                ("CLAUDE_CODE_SHELL", "/bin/zsh"),
                ("DISABLE_AUTOUPDATER", "1"),
                ("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "100000"),
            ]),
        );
        assert_eq!(config.shell.timeout_secs, 46);
        assert_eq!(config.shell.program.as_deref(), Some("/bin/zsh"));
        assert!(!config.update.auto && !config.update.notify);
        assert_eq!(config.max_context_tokens, 100_000);
    }

    #[test]
    fn the_autocompact_percentage_moves_the_80_percent_point() {
        let mut config = Config::default();
        config.max_context_tokens = 160_000;
        apply(
            &mut config,
            &env(&[("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE", "40")]),
        );
        assert_eq!(config.max_context_tokens, 80_000);
    }

    #[test]
    fn a_false_switch_is_not_a_switch() {
        let mut config = Config::default();
        let was = config.update.notify;
        apply(&mut config, &env(&[("DISABLE_AUTOUPDATER", "0")]));
        assert_eq!(config.update.notify, was);
        assert!(!truthy(&env(&[("X", "false")]), "X"));
        assert!(truthy(&env(&[("X", "1")]), "X"));
    }

    #[test]
    fn effort_prefers_the_level_over_the_thinking_budget() {
        assert_eq!(
            effort(&env(&[
                ("CLAUDE_CODE_EFFORT_LEVEL", "high"),
                ("MAX_THINKING_TOKENS", "0")
            ])),
            Some("high".to_string())
        );
        assert_eq!(
            effort(&env(&[("MAX_THINKING_TOKENS", "0")])),
            Some("low".to_string())
        );
        assert_eq!(
            effort(&env(&[("MAX_THINKING_TOKENS", "10000")])),
            Some("medium".to_string())
        );
        assert_eq!(
            effort(&env(&[("MAX_THINKING_TOKENS", "63999")])),
            Some("xhigh".to_string())
        );
        assert_eq!(effort(&env(&[])), None);
    }

    #[test]
    fn turns_and_model_aliases() {
        assert_eq!(max_turns(&env(&[("CLAUDE_CODE_MAX_TURNS", "7")])), Some(7));
        assert_eq!(max_turns(&env(&[("CLAUDE_CODE_MAX_TURNS", "0")])), None);
        let aliases = model_aliases(&env(&[
            ("ANTHROPIC_DEFAULT_OPUS_MODEL", "big"),
            ("ANTHROPIC_SMALL_FAST_MODEL", "small"),
        ]));
        assert_eq!(aliases["opus"], "big");
        assert_eq!(aliases["haiku"], "small");
    }

    #[test]
    fn claude_names_publish_wizard_ones() {
        let published = publish(&env(&[
            ("CLAUDE_CODE_SAFE_MODE", "1"),
            ("CLAUDE_CODE_PLUGIN_DIRS", "/a:/b"),
            ("CLAUDE_CODE_TMPDIR", "/t"),
        ]));
        let names: Vec<_> = published.iter().map(|(k, _)| *k).collect();
        assert!(names.contains(&"WIZARD_SAFE_MODE"));
        assert!(names.contains(&"WIZARD_BARE"));
        assert!(names.contains(&"WIZARD_PLUGIN_DIRS"));
        assert!(names.contains(&"TMPDIR"));
    }
}
