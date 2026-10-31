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

/// `ANTHROPIC_CUSTOM_HEADERS`: one `Name: value` per line.
pub fn custom_headers(get: Get) -> Vec<(String, String)> {
    let Some(raw) = get("ANTHROPIC_CUSTOM_HEADERS") else {
        return Vec::new();
    };
    raw.lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(name, val)| (name.trim().to_string(), val.trim().to_string()))
        .filter(|(name, _)| !name.is_empty())
        .collect()
}

/// The `anthropic-beta` header value: `--betas` (published as
/// `WIZARD_BETAS`, spaces or commas) and `ANTHROPIC_BETAS` (commas).
pub fn betas(get: Get) -> Option<String> {
    let mut all: Vec<String> = Vec::new();
    for name in ["WIZARD_BETAS", "ANTHROPIC_BETAS"] {
        for beta in value(get, name)
            .unwrap_or_default()
            .split([',', ' '])
            .map(str::trim)
            .filter(|b| !b.is_empty())
        {
            if !all.iter().any(|have| have == beta) {
                all.push(beta.to_string());
            }
        }
    }
    (!all.is_empty()).then(|| all.join(","))
}

/// A JSON object from an environment variable, or why it is not one.
fn json_object(get: Get, name: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    let raw = value(get, name)?;
    match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(serde_json::Value::Object(map)) => Some(map),
        _ => {
            tracing::warn!("{name} must be a JSON object; ignored");
            None
        }
    }
}

/// Add `CLAUDE_CODE_EXTRA_BODY` (top-level keys) and
/// `CLAUDE_CODE_EXTRA_METADATA` (keys of `metadata`) to an Anthropic request
/// body. Keys the request already has are kept.
pub fn extend_request_body(body: &mut serde_json::Value, get: Get) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    if let Some(extra) = json_object(get, "CLAUDE_CODE_EXTRA_BODY") {
        for (key, val) in extra {
            object.entry(key).or_insert(val);
        }
    }
    if let Some(extra) = json_object(get, "CLAUDE_CODE_EXTRA_METADATA") {
        let metadata = object
            .entry("metadata")
            .or_insert_with(|| serde_json::json!({}));
        if let Some(metadata) = metadata.as_object_mut() {
            for (key, val) in extra {
                metadata.entry(key).or_insert(val);
            }
        }
    }
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
    // `DISABLE_COMPACT` turns off /compact as well; `DISABLE_AUTO_COMPACT` only the automatic pass.
    if truthy(get, "DISABLE_COMPACT") || truthy(get, "DISABLE_AUTO_COMPACT") {
        config.auto_compact = false;
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

/// Longest `timeout` the model may give a shell command: `BASH_MAX_TIMEOUT_MS`,
/// rounded up to whole seconds, or ten minutes.
pub fn bash_max_timeout_secs(get: Get) -> u64 {
    value(get, "BASH_MAX_TIMEOUT_MS")
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map_or(600, |ms| ms.div_ceil(1000))
}

/// Characters of a successful command's output the model gets inline:
/// `bashOutputMaxChars` (the setting) or `BASH_MAX_OUTPUT_LENGTH`, clamped to
/// 4000 to 128000 as Claude Code clamps it. `None` leaves Wizard's own cap.
pub fn bash_output_chars(get: Get, setting: Option<u64>) -> Option<usize> {
    let asked = setting.or_else(|| value(get, "BASH_MAX_OUTPUT_LENGTH")?.parse().ok())?;
    Some(asked.clamp(4_000, 128_000) as usize)
}

fn millis(get: Get, name: &str, default_ms: u64) -> std::time::Duration {
    let ms = value(get, name)
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .unwrap_or(default_ms);
    std::time::Duration::from_millis(ms)
}

/// How long an MCP server has to start and answer `initialize`: `MCP_TIMEOUT`
/// in milliseconds, twenty seconds without it.
pub fn mcp_connect_timeout(get: Get) -> std::time::Duration {
    millis(get, "MCP_TIMEOUT", 20_000)
}

/// How long one MCP tool call may run: `MCP_TOOL_TIMEOUT` in milliseconds, two
/// minutes without it.
pub fn mcp_call_timeout(get: Get) -> std::time::Duration {
    millis(get, "MCP_TOOL_TIMEOUT", 120_000)
}

/// Tokens of an MCP result kept, from `MAX_MCP_OUTPUT_TOKENS`.
pub fn mcp_output_tokens(get: Get) -> Option<usize> {
    value(get, "MAX_MCP_OUTPUT_TOKENS")
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
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
    if truthy(get, "DISABLE_COLORS") {
        out.push(("NO_COLOR", "1".to_string()));
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
        Config {
            providers: vec![ProviderConfig {
                name: "claude".into(),
                kind: crate::config::ProviderKind::ANTHROPIC,
                ..ProviderConfig::default()
            }],
            active_provider: Some("claude".into()),
            ..Config::default()
        }
    }

    #[test]
    fn disable_colors_publishes_no_color() {
        let published = publish(&env(&[("DISABLE_COLORS", "1")]));
        assert!(published.contains(&("NO_COLOR", "1".to_string())));
        assert!(publish(&env(&[("DISABLE_COLORS", "0")])).is_empty());
    }

    #[test]
    fn custom_headers_betas_and_extra_body() {
        let get = env(&[
            ("ANTHROPIC_CUSTOM_HEADERS", "X-One: a b\nbad line\nX-Two:c"),
            ("WIZARD_BETAS", "b1 b2"),
            ("ANTHROPIC_BETAS", "b2,b3"),
            ("CLAUDE_CODE_EXTRA_BODY", r#"{"top_k": 5, "model": "no"}"#),
            ("CLAUDE_CODE_EXTRA_METADATA", r#"{"user_id": "u"}"#),
        ]);
        assert_eq!(
            custom_headers(&get),
            vec![
                ("X-One".to_string(), "a b".to_string()),
                ("X-Two".to_string(), "c".to_string())
            ]
        );
        assert_eq!(betas(&get).as_deref(), Some("b1,b2,b3"));
        let mut body = serde_json::json!({"model": "m"});
        extend_request_body(&mut body, &get);
        assert_eq!(body["top_k"], 5);
        assert_eq!(body["model"], "m", "the request's own keys win");
        assert_eq!(body["metadata"]["user_id"], "u");
        let none = env(&[("CLAUDE_CODE_EXTRA_BODY", "[1]")]);
        let mut body = serde_json::json!({"model": "m"});
        extend_request_body(&mut body, &none);
        assert_eq!(body, serde_json::json!({"model": "m"}));
        assert_eq!(betas(&env(&[])), None);
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
    fn the_shell_and_mcp_limits_read_claude_codes_variables() {
        let none = env(&[]);
        assert_eq!(bash_max_timeout_secs(&none), 600);
        assert_eq!(
            bash_max_timeout_secs(&env(&[("BASH_MAX_TIMEOUT_MS", "90001")])),
            91
        );
        assert_eq!(bash_output_chars(&none, None), None);
        let long = env(&[("BASH_MAX_OUTPUT_LENGTH", "999999")]);
        assert_eq!(bash_output_chars(&long, None), Some(128_000));
        assert_eq!(bash_output_chars(&long, Some(10)), Some(4_000));
        assert_eq!(mcp_connect_timeout(&none).as_secs(), 20);
        assert_eq!(
            mcp_connect_timeout(&env(&[("MCP_TIMEOUT", "5000")])).as_secs(),
            5
        );
        assert_eq!(mcp_call_timeout(&none).as_secs(), 120);
        assert_eq!(
            mcp_call_timeout(&env(&[("MCP_TOOL_TIMEOUT", "1500")])).as_millis(),
            1500
        );
        assert_eq!(mcp_output_tokens(&none), None);
        assert_eq!(
            mcp_output_tokens(&env(&[("MAX_MCP_OUTPUT_TOKENS", "50000")])),
            Some(50_000)
        );
    }

    #[test]
    fn the_compact_switches_turn_automatic_compaction_off() {
        let mut config = Config::default();
        assert!(config.auto_compact);
        apply(&mut config, &env(&[("DISABLE_AUTO_COMPACT", "1")]));
        assert!(!config.auto_compact);
        let mut config = Config::default();
        apply(&mut config, &env(&[("DISABLE_COMPACT", "true")]));
        assert!(!config.auto_compact);
        let mut config = Config::default();
        apply(&mut config, &env(&[("DISABLE_AUTO_COMPACT", "0")]));
        assert!(config.auto_compact);
    }

    #[test]
    fn the_autocompact_percentage_moves_the_80_percent_point() {
        let mut config = Config {
            max_context_tokens: 160_000,
            ..Config::default()
        };
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
