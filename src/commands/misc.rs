//! Smaller Claude Code commands: `/statusline`, `/autocompact`, and
//! the ones that name an Anthropic feature Wizard has no counterpart for. The
//! last kind answer with what to use here instead, so a script or a habit
//! gets a sentence rather than "unknown command".

use serde_json::{Value, json};

use crate::config::Config;

/// `(command, what to say)` for the commands with no Wizard counterpart.
pub const NOT_AVAILABLE: &[(&str, &str)] = &[
    (
        "color",
        "the prompt bar colour is not a setting here. Colours come from the interface skin: /ui lists them.",
    ),
    (
        "usage-credits",
        "there are no Anthropic credits to manage. /usage shows the limits of the xAI and ChatGPT sign-ins, /cost the session's tokens.",
    ),
    (
        "passes",
        "guest passes are an Anthropic subscription feature.",
    ),
    (
        "upgrade",
        "plans are a Claude subscription matter. `wizard update` updates Wizard itself.",
    ),
    (
        "rate-limit-options",
        "there are no Claude rate-limit tiers here. /usage shows the limits of your sign-ins.",
    ),
    (
        "heapdump",
        "Wizard is a Rust binary, so there is no JS heap to dump. /doctor and the log in ~/.wizard/logs are the diagnostics.",
    ),
    (
        "insights",
        "no session-insights report. `wizard usage` rolls up tokens and cost across sessions.",
    ),
    (
        "restart",
        "start Wizard again; /resume brings the conversation back.",
    ),
    (
        "autofix-pr",
        "autofix-pr watches a pull request through Anthropic's cloud. /loop 10m over `gh pr checks` is the local version.",
    ),
    (
        "chrome",
        "Claude in Chrome is a browser extension. The computer tool drives a browser or the desktop: `wizard computer setup`.",
    ),
    (
        "design",
        "Claude Design is a claude.ai product; there is no counterpart here.",
    ),
    (
        "design-sync",
        "Claude Design is a claude.ai product; there is no counterpart here.",
    ),
    (
        "design-login",
        "Claude Design is a claude.ai product; there is no counterpart here.",
    ),
    (
        "design-consent",
        "Claude Design is a claude.ai product; there is no counterpart here.",
    ),
    (
        "design-revoke",
        "Claude Design is a claude.ai product; there is no counterpart here.",
    ),
    (
        "install-github-app",
        "the Claude GitHub App is Anthropic's. For CI, run `wizard -p` in a workflow step; docs/headless.md has the flags.",
    ),
    (
        "install-slack-app",
        "the Claude Slack app is Anthropic's. `wizard --gateway` runs the same agent behind a Telegram bot.",
    ),
    (
        "powerup",
        "/help lists the commands, and docs/getting-started.md walks through them.",
    ),
    (
        "radio",
        "Claude FM is a Claude Code extra with no counterpart here.",
    ),
    ("stickers", "stickers are Anthropic merchandise."),
    (
        "tui",
        "there is one terminal UI. /ui switches skins and /view flips the compact transcript.",
    ),
    (
        "web-setup",
        "cloud sessions with a GitHub account are Anthropic's service.",
    ),
    (
        "batch",
        "no cloud batch of PR agents. `wizard fleet run -n N` splits a mission over parallel worktrees and merges them back.",
    ),
    (
        "claude-api",
        "the Claude API reference skill is not bundled. A skill of your own under .wizard/skills or .claude/skills loads like any other.",
    ),
    (
        "dataviz",
        "the chart-design skill is not bundled. A skill of your own under .wizard/skills or .claude/skills loads like any other.",
    ),
    (
        "deep-research",
        "the deep-research workflow is not built. /ultra fans a question out over lenses and judges the answers.",
    ),
    (
        "plugin-authoring",
        "Claude Code mods are not Wizard plugins. Wizard plugins are written as docs/plugins.md describes, with wizard-plugin.d.ts for the types.",
    ),
    (
        "auto-mode-setup",
        "auto mode is Anthropic's permission classifier. --permission-mode and /permissions set what Wizard asks about.",
    ),
];

pub fn not_available(command: &str) -> Option<&'static str> {
    NOT_AVAILABLE
        .iter()
        .find(|(name, _)| *name == command)
        .map(|(_, text)| *text)
}

fn user_settings() -> Result<(std::path::PathBuf, Value), String> {
    let path = Config::wizard_dir()
        .map(|dir| dir.join("settings.json"))
        .map_err(|err| format!("{err:#}"))?;
    let value = match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw)
            .map_err(|err| format!("{} is not valid JSON ({err}); left alone", path.display()))?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(err) => return Err(format!("{}: {err}", path.display())),
    };
    Ok((path, value))
}

fn write_settings(path: &std::path::Path, value: &Value) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text = serde_json::to_string_pretty(value).map_err(|err| err.to_string())?;
    std::fs::write(path, format!("{text}\n")).map_err(|err| format!("{}: {err}", path.display()))
}

/// `/statusline [command | off]`: the `statusLine` setting. Wizard stores and
/// reports it; the terminal UI draws the line.
pub fn statusline(args: &str, root: &std::path::Path) -> Result<String, String> {
    let args = args.trim();
    let (path, mut settings) = user_settings()?;
    let object = settings
        .as_object_mut()
        .ok_or_else(|| format!("{} is not a JSON object", path.display()))?;
    match args {
        "" => {
            let current = crate::config::claude::load(root, crate::config::claude::Trust::Refused)
                .status_line;
            Ok(match current {
                Some(line) => format!("statusLine: {line}"),
                None => "no statusLine is set. /statusline <command> runs that command for the line, /statusline off removes it".to_string(),
            })
        }
        "off" | "clear" | "none" => {
            object.remove("statusLine");
            write_settings(&path, &settings)?;
            Ok(format!("statusLine removed from {}", path.display()))
        }
        command => {
            object.insert(
                "statusLine".to_string(),
                json!({"type": "command", "command": command}),
            );
            write_settings(&path, &settings)?;
            Ok(format!(
                "statusLine set in {}; the terminal UI runs the command to draw the line",
                path.display()
            ))
        }
    }
}

/// `/fast [on|off]`: the Anthropic provider's speed tier.
pub fn fast(args: &str) -> Result<String, String> {
    let args = args.trim();
    let on = match args {
        "" => !crate::config::fast::enabled(),
        "on" | "true" | "1" => true,
        "off" | "false" | "0" => false,
        other => return Err(format!("/fast takes on or off, not '{other}'")),
    };
    let get = |name: &str| std::env::var(name).ok();
    if on && crate::config::claude_env::truthy(&get, "CLAUDE_CODE_DISABLE_FAST_MODE") {
        return Err("fast mode is turned off by CLAUDE_CODE_DISABLE_FAST_MODE".to_string());
    }
    crate::config::fast::set(on);
    let (path, mut settings) = user_settings()?;
    if let Some(object) = settings.as_object_mut() {
        object.insert("fastMode".to_string(), json!(on));
        write_settings(&path, &settings)?;
    }
    let config = Config::load().map_err(|err| format!("{err:#}"))?;
    let active = config.active();
    let state = if on { "on" } else { "off" };
    Ok(if active.kind == crate::config::ProviderKind::ANTHROPIC {
        if crate::config::fast::applies_to(&active.model) {
            format!(
                "fast mode {state}: requests to {} carry speed=fast",
                active.model
            )
        } else {
            format!(
                "fast mode {state}, but {} has no fast tier; only Opus models do",
                active.model
            )
        }
    } else {
        format!(
            "fast mode {state} (saved to fastMode). The active provider, {}, has no speed tier, so nothing changes until an anthropic provider is active.",
            active.name
        )
    })
}

/// `/sandbox`: what the `sandbox` settings turn into, since Wizard has no
/// process sandbox to switch on.
pub fn sandbox(root: &std::path::Path) -> String {
    let keys = crate::config::claude::load(root, crate::config::claude::Trust::Refused).policy;
    let paths = |label: &str, list: &[String]| {
        if list.is_empty() {
            String::new()
        } else {
            format!("  {label}: {}\n", list.join(", "))
        }
    };
    let rules = paths("no reading", &keys.sandbox_deny_read)
        + &paths("no writing", &keys.sandbox_deny_write)
        + &paths("no fetching from", &keys.sandbox_denied_domains);
    if rules.is_empty() {
        return "no sandbox rules. Wizard has no OS sandbox; with \"sandbox\": {\"enabled\": true} in settings, filesystem.denyRead, filesystem.denyWrite and network.deniedDomains become deny rules for the file tools and web_fetch (shell commands are not covered). /permissions shows the rest.".to_string();
    }
    format!(
        "sandbox settings, applied as deny rules to the file tools and web_fetch (shell commands are not covered)\n{}",
        rules.trim_end()
    )
}

/// `/privacy-settings`: where data leaves this machine.
pub fn privacy() -> String {
    let config = Config::load().unwrap_or_default();
    let active = config.active();
    let mut lines = vec![
        "Wizard sends no telemetry, analytics or error reports.".to_string(),
        format!(
            "model requests go to {} ({})",
            active.name,
            if active.base_url.is_empty() {
                "its default endpoint"
            } else {
                active.base_url.as_str()
            }
        ),
    ];
    let others: Vec<&str> = config
        .providers
        .iter()
        .filter(|provider| provider.name != active.name)
        .map(|provider| provider.name.as_str())
        .collect();
    if !others.is_empty() {
        lines.push(format!(
            "also configured, used only when selected: {}",
            others.join(", ")
        ));
    }
    lines.push(if config.update.notify || config.update.auto {
        format!(
            "update check: asks GitHub ({}) about new releases every {}h",
            config.update.repo, config.update.interval_hours
        )
    } else {
        "update check: off".to_string()
    });
    lines.push(
        "web_fetch and web_search reach the sites and search backend you or the model name; MCP http servers reach their own URLs.".to_string(),
    );
    lines.push("What a provider keeps is that provider's policy.".to_string());
    lines.join("\n")
}

/// `/autocompact [tokens|off]`: the context size compaction aims under.
/// Compaction starts at 80% of `max_context_tokens`; `0` uses the model's own
/// window.
pub fn autocompact(args: &str) -> Result<String, String> {
    let mut config = Config::load().map_err(|err| format!("{err:#}"))?;
    let args = args.trim();
    if args.is_empty() {
        return Ok(match config.max_context_tokens {
            0 => "compaction follows the model's own context window (80% of it). /autocompact <tokens> caps the window it counts, for example /autocompact 200k".to_string(),
            tokens => format!(
                "compaction starts at 80% of {tokens} tokens ({}). /autocompact 0 goes back to the model's window",
                tokens * 4 / 5
            ),
        });
    }
    let tokens = parse_tokens(args)?;
    config.max_context_tokens = tokens;
    config.save().map_err(|err| format!("{err:#}"))?;
    Ok(match tokens {
        0 => "compaction follows the model's own window again, from the next session".to_string(),
        tokens => format!(
            "compaction starts at {} tokens (80% of {tokens}), from the next session",
            tokens * 4 / 5
        ),
    })
}

fn parse_tokens(text: &str) -> Result<u32, String> {
    let text = text.trim().to_ascii_lowercase();
    if matches!(text.as_str(), "off" | "default") {
        return Ok(0);
    }
    let (digits, scale) = match text.strip_suffix('k') {
        Some(rest) => (rest, 1_000),
        None => match text.strip_suffix('m') {
            Some(rest) => (rest, 1_000_000),
            None => (text.as_str(), 1),
        },
    };
    digits
        .parse::<u32>()
        .ok()
        .and_then(|n| n.checked_mul(scale))
        .ok_or_else(|| format!("'{text}' is not a token count (try 200k, 1m, 0 or off)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_counts_take_k_and_m_suffixes() {
        assert_eq!(parse_tokens("200k"), Ok(200_000));
        assert_eq!(parse_tokens("1m"), Ok(1_000_000));
        assert_eq!(parse_tokens("5000"), Ok(5000));
        assert_eq!(parse_tokens("off"), Ok(0));
        assert!(parse_tokens("lots").is_err());
    }

    #[test]
    fn every_unavailable_command_says_what_to_use() {
        for (name, text) in NOT_AVAILABLE {
            assert!(text.len() > 20, "/{name} says too little");
            assert!(!text.contains('\u{2014}'), "/{name}: no em dashes");
        }
        assert!(not_available("stickers").is_some());
        assert!(not_available("voice").is_none(), "/voice is a real command");
        assert!(not_available("fast").is_none(), "/fast is a real command");
        assert!(not_available("nope").is_none());
    }
}
