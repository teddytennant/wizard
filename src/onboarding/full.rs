//! The full wizard: every question `wizard --onboard`, `wizard setup` and the
//! `/setup` menu ask, and the plaintext summary it prints.

use std::path::Path;

use anyhow::{Context, Result};

use crate::config::{Config, Credentials, GatewayConfig, GatewayKind, Mode, ProviderKind};
use crate::hardware;
use crate::import_claude::{self, ImportSelection};
use crate::skin::Skin;
use crate::theme;

use super::widgets::{
    Opt, Tui, multi_select, notice, restore_terminal_best_effort, select, setup_terminal,
    text_input,
};
use super::*;

/// Run the full wizard: every question, then the config is saved and a
/// plaintext summary printed. `Ok(None)` if the user cancelled (Esc /
/// Ctrl-C). Terminal setup/teardown is restored on every exit path,
/// including errors.
///
/// The interactive loop is synchronous (blocking crossterm reads); it runs on
/// a blocking thread so it never stalls the async runtime.
pub async fn run_full() -> Result<Option<Config>> {
    tokio::task::spawn_blocking(|| run_full_blocking(true))
        .await
        .context("onboarding task panicked")?
}

/// Synchronous core of [`run_full`]. The TUI's `/setup` row calls it with
/// the terminal suspended and `print` off, since the screen is cleared on
/// return.
pub fn run_full_blocking(print: bool) -> Result<Option<Config>> {
    // Install the skin and theme before anything is drawn: there is no config
    // yet on a first run, so this is `WIZARD_SKIN` / `WIZARD_THEME` plus the
    // terminal's colour depth (`NO_COLOR`, `WIZARD_COLOR`, `TERM`). A name
    // that will not load is reported after the terminal is restored, not
    // swallowed.
    //
    // Onboarding itself always draws in its own plain style rather than the
    // chosen skin's: the interface question is one of the questions it asks,
    // and a wizard that restyled itself halfway through would make the answer
    // look like it had already taken effect for the whole session.
    let skin_warning = crate::skin::init(None);
    let theme_warning = theme::init(crate::skin::active().companion_theme());
    let mut terminal = setup_terminal()?;
    let outcome = collect_full_answers(&mut terminal);
    restore_terminal_best_effort();
    for warning in [skin_warning, theme_warning].into_iter().flatten() {
        eprintln!("warning: {warning}");
    }

    let answers = match outcome {
        Ok(Some(answers)) => answers,
        Ok(None) => return Ok(None),
        Err(err) => return Err(err),
    };

    let import = answers.claude_import;
    // The pasted secrets go to credentials.toml (0600), never into config.
    store_pasted_secrets(&answers, crate::credentials::store);
    let mut config = answers.into_config();

    // Perform the Claude Code import (MCP/commands file writes + spinner verbs)
    // before saving, so the verbs land in the same config write.
    let import_summary = match import {
        Some(selection) => match import_claude::run_import(&selection) {
            Ok(outcome) => {
                if !outcome.spinner_verbs.is_empty() {
                    config.ui.spinner_verbs = outcome.spinner_verbs.clone();
                }
                Some(outcome.summary())
            }
            Err(err) => Some(format!("import failed: {err:#}")),
        },
        None => None,
    };

    config.save().context("saving config from onboarding")?;
    if !print {
        return Ok(Some(config));
    }
    print_summary(&config);
    if let Some(summary) = import_summary
        && !summary.is_empty()
    {
        println!("Imported from Claude Code:");
        for line in summary.lines() {
            println!("  • {line}");
        }
        println!();
    }
    Ok(Some(config))
}

/// The gateway questions alone, from the TUI's `/setup` menu: collect, store
/// the token, write the `[gateway]` section into the existing config.
/// `Ok(None)` on Esc.
pub fn run_gateway_setup_blocking() -> Result<Option<Config>> {
    let mut terminal = setup_terminal()?;
    let outcome = collect_gateway(&mut terminal);
    restore_terminal_best_effort();
    let Some(gateway) = outcome? else {
        return Ok(None);
    };
    let mut config = Config::load().context("loading config for gateway setup")?;
    if let Some(token) = gateway
        .bot_token
        .as_deref()
        .filter(|token| !token.is_empty())
        && let Err(err) = crate::credentials::store(crate::credentials::GATEWAY_TOKEN, token)
    {
        eprintln!("warning: could not save the Telegram bot token: {err:#}");
    }
    config.gateway = GatewayConfig {
        kind: gateway.kind,
        token_env: gateway.token_env,
        allowed_chat_ids: gateway.allowed_chat_ids,
    };
    config.save().context("saving config from gateway setup")?;
    Ok(Some(config))
}

/// The provider menu, in display order, before `installed` narrows it.
///
/// The order is the order: xAI first, then the one-click local pick, then the
/// keyed clouds, then the bring-your-own rows. Filtering preserves it.
///
/// What is derived here is *availability*, and only that. The labels and the
/// details beside them stay written out, because a descriptor answers "what is
/// this backend called" and this menu answers "which of these should a person
/// pick, and why" — "one pick, model sized to this machine" is a claim about
/// what the *next three steps* will do, and no backend knows that about
/// itself. `docs/plugins.md` already permits core to hold the text a user
/// would otherwise type; this is the same boundary one step further out.
fn provider_choices(installed: &[ProviderKind]) -> Vec<ProviderChoice> {
    let all = vec![
        ProviderChoice {
            label: "xAI account sign-in",
            detail: "grok-4.6 via OAuth, no API key".to_string(),
            kinds: vec![ProviderKind::XAI_OAUTH],
            collect: collect_xai_oauth,
        },
        ProviderChoice {
            label: "xAI (Grok), API key",
            detail: "grok-4.6 via XAI_API_KEY".to_string(),
            kinds: vec![ProviderKind::XAI],
            collect: collect_xai,
        },
        ProviderChoice {
            label: "Local",
            detail: "one pick: llama.cpp & Ollama set up for you, model sized to this \
                     machine; private, no API key"
                .to_string(),
            kinds: vec![ProviderKind::LLAMACPP, ProviderKind::OLLAMA],
            collect: collect_local_auto,
        },
        ProviderChoice {
            label: "OpenRouter",
            detail: "hundreds of models via OPENROUTER_API_KEY".to_string(),
            kinds: vec![ProviderKind::OPENROUTER],
            collect: collect_openrouter,
        },
        ProviderChoice {
            label: "Cloudflare Workers AI",
            detail: "GLM 5.2 via CLOUDFLARE_API_TOKEN (+ account id)".to_string(),
            kinds: vec![ProviderKind::CLOUDFLARE],
            collect: collect_cloudflare,
        },
        ProviderChoice {
            label: "OpenAI / OpenAI-compatible",
            detail: "gpt-5.6 family and friends".to_string(),
            kinds: vec![ProviderKind::OPENAI],
            collect: collect_openai,
        },
        ProviderChoice {
            label: "Anthropic (Claude)",
            detail: "claude-fable-5".to_string(),
            kinds: vec![ProviderKind::ANTHROPIC],
            collect: collect_anthropic,
        },
        // Gemini, DeepSeek, Groq and the rest are `compat.rs` presets, which
        // are `kind = "openai"` with a base URL: they need the OpenAI plugin
        // and nothing else.
        ProviderChoice {
            label: "More cloud providers",
            detail: "Gemini, DeepSeek, Groq, Mistral, Kimi, GLM, …".to_string(),
            kinds: vec![ProviderKind::OPENAI],
            collect: collect_compat_menu,
        },
        ProviderChoice {
            label: "Custom OpenAI-compatible endpoint",
            detail: "any base URL".to_string(),
            kinds: vec![ProviderKind::OPENAI],
            collect: collect_custom,
        },
        ProviderChoice {
            label: "BYOM: llama.cpp",
            detail: "bring your own model: any GGUF, your server URL".to_string(),
            kinds: vec![ProviderKind::LLAMACPP],
            collect: collect_llamacpp,
        },
        ProviderChoice {
            label: "BYOM: Ollama",
            detail: "bring your own model: any Ollama tag, pulled on first run".to_string(),
            kinds: vec![ProviderKind::OLLAMA],
            collect: collect_ollama,
        },
    ];
    all.into_iter()
        .filter(|choice| choice.kinds.iter().any(|kind| installed.contains(kind)))
        .collect()
}

/// Drive the full sequence of steps. Returns `Ok(None)` as soon as any step
/// is cancelled.
fn collect_full_answers(terminal: &mut Tui) -> Result<Option<Answers>> {
    // Step 1 — provider, xAI first, and only the ones this build can reach.
    let choices = provider_choices(&crate::llm::registry::kinds());
    if choices.is_empty() {
        anyhow::bail!(
            "this build has no provider backends compiled in, so there is nothing to \
             onboard to.\n\
             \n\
             Every backend is a plugin behind a cargo feature (`provider-xai`, \
             `provider-openai`, `provider-llamacpp`, …) and all of them are on by \
             default. Rebuild with the ones you want, or install a stock release \
             binary, which has all of them. See docs/plugins.md."
        );
    }
    let options: Vec<Opt> = choices
        .iter()
        .map(|choice| Opt::new(choice.label, choice.detail.clone()))
        .collect();
    let provider = match select(
        terminal,
        "Provider",
        "Where should Wizard send its requests?",
        &options,
        0,
    )? {
        Some(index) => index,
        None => return Ok(None),
    };

    // Step 2 — model (+ key env / base url, depending on provider). The
    // one-click local pick asks nothing further.
    let collected = match (choices[provider].collect)(terminal)? {
        Some(collected) => collected,
        None => return Ok(None),
    };

    // Step 3 — messaging gateway.
    let gateway = match collect_gateway(terminal)? {
        Some(gateway) => gateway,
        None => return Ok(None),
    };

    // Step 4 — mode.
    let mode_options = [
        Opt::new(
            "Genie, interactive",
            "bypass permissions; acts without asking (recommended)",
        ),
        Opt::new("Sovereign, autonomous", "autonomous; works continuously"),
    ];
    let mode = match select(
        terminal,
        "Mode",
        "How should Wizard behave by default?",
        &mode_options,
        0,
    )? {
        Some(0) => Mode::Genie,
        Some(_) => Mode::Sovereign,
        None => return Ok(None),
    };

    // Step 5 — the interface. Purely how the terminal looks: the same Wizard
    // commands, onboarding, providers and keys under every one of them, which
    // is what the detail line has to say, because "Claude Code" in a list of
    // options otherwise reads as a choice of *agent*.
    let skin_options: Vec<Opt> = Skin::ALL
        .iter()
        .map(|skin| Opt::new(skin.label(), skin.description()))
        .collect();
    let skin = match select(
        terminal,
        "Interface",
        "Which terminal UI should Wizard wear? (looks only, same commands either way; \
         change it any time with /ui)",
        &skin_options,
        0,
    )? {
        Some(index) => Some(Skin::ALL[index]),
        None => return Ok(None),
    };

    // Step 6 — web search backend (used by the `web_search` tool). DuckDuckGo
    // needs no key; the keyed backends prompt for one; xAI reuses an existing
    // sign-in when present so the user is not asked to authenticate twice.
    let (web_search_backend, web_search_api_key) = match collect_web_search(terminal)? {
        Some(pair) => pair,
        None => return Ok(None),
    };

    // Step 7 — optional: import artifacts from an existing Claude Code install.
    // Only shown when `~/.claude` exists. Esc here skips the import (the rest of
    // the config is already complete) rather than aborting onboarding.
    let claude_import = if import_claude::claude_home().is_some() {
        collect_claude_import(terminal)?
    } else {
        None
    };

    Ok(Some(Answers {
        provider_name: collected.provider_name,
        kind: collected.kind,
        base_url: collected.base_url,
        model: collected.model,
        api_key_env: collected.api_key_env,
        provider_api_key: collected.api_key,
        gguf_path: collected.gguf_path,
        gateway_kind: gateway.kind,
        gateway_token_env: gateway.token_env,
        gateway_allowed_chat_ids: gateway.allowed_chat_ids,
        mode,
        skin,
        web_search_backend,
        web_search_api_key,
        gateway_bot_token: gateway.bot_token,
        claude_import,
    }))
}

/// The gateway step's answers.
struct GatewayAnswers {
    kind: GatewayKind,
    token_env: Option<String>,
    allowed_chat_ids: Vec<i64>,
    bot_token: Option<String>,
}

/// The messaging-gateway step: none, or Telegram with its token, env var and
/// allow-list. `Ok(None)` on cancel.
fn collect_gateway(terminal: &mut Tui) -> Result<Option<GatewayAnswers>> {
    let gateway_options = [
        Opt::new("None, terminal only", "recommended"),
        Opt::new("Telegram", "chat with Wizard from a bot"),
    ];
    let gateway = match select(
        terminal,
        "Messaging gateway",
        "Expose Wizard over a chat platform?",
        &gateway_options,
        0,
    )? {
        Some(index) => index,
        None => return Ok(None),
    };

    if gateway != 1 {
        return Ok(Some(GatewayAnswers {
            kind: GatewayKind::None,
            token_env: None,
            allowed_chat_ids: Vec::new(),
            bot_token: None,
        }));
    }
    // Paste the bot token itself (stored in credentials.toml, 0600).
    // Leave empty only if the user prefers an env var (next prompt).
    let bot_token = match text_input(
        terminal,
        "Telegram bot token",
        "Paste the token from @BotFather. Stored in ~/.wizard/credentials.toml (0600). Leave empty to use an env var instead.",
        "",
    )? {
        Some(value) => value,
        None => return Ok(None),
    };
    let bot_token = bot_token.trim().to_string();
    let gateway_bot_token = (!bot_token.is_empty()).then_some(bot_token);

    // Optional env-var fallback name (used when no credential is stored).
    let token_env = match text_input(
        terminal,
        "Telegram bot token env var (optional fallback)",
        "Used only when no token is stored in credentials.toml.",
        GatewayConfig::DEFAULT_TOKEN_ENV,
    )? {
        Some(value) => value,
        None => return Ok(None),
    };
    // Allowed chat IDs: re-prompt on a parse error rather than discarding
    // the answer. The list is a closed allow-list (see
    // `gateway::is_authorized`), so an empty answer is not "allow all",
    // it is "allow nobody". Say so before and after the prompt, because
    // from the outside the bot then looks broken rather than locked.
    let allowed = loop {
        let raw = match text_input(
            terminal,
            "Allowed chat IDs",
            "Comma-separated numeric chat IDs. Only these chats can drive the agent; \
             an empty list refuses every message.",
            "",
        )? {
            Some(value) => value,
            None => return Ok(None),
        };
        match parse_chat_ids(&raw) {
            Ok(ids) => {
                if ids.is_empty() {
                    notice(
                        terminal,
                        "No chat IDs entered: the gateway will refuse every message. \
                         Run `wizard gateway setup` afterwards: it has you message the \
                         bot, reports your chat id, and adds it for you.",
                    )?;
                }
                break ids;
            }
            Err(message) => {
                notice(terminal, &message)?;
            }
        }
    };
    Ok(Some(GatewayAnswers {
        kind: GatewayKind::Telegram,
        token_env: Some(token_env),
        allowed_chat_ids: allowed,
        bot_token: gateway_bot_token,
    }))
}

/// The web-search backends offered in onboarding: `(label, detail, id)`. The id
/// is written to `[web] search_backend` and used as the credentials key name.
const WEB_SEARCH_OPTIONS: &[(&str, &str, &str)] = &[
    (
        "DuckDuckGo",
        "free · no API key (recommended)",
        "duckduckgo",
    ),
    ("Brave Search", "API key · brave.com/search/api", "brave"),
    ("Tavily", "API key · tavily.com", "tavily"),
    ("Exa", "API key · exa.ai", "exa"),
    ("Serper (Google)", "API key · serper.dev", "serper"),
    ("xAI (Grok)", "your xAI sign-in, or an API key", "xai"),
];

/// Pick the `web_search` backend and, for keyed backends, collect the API key.
/// Returns `(backend_id, Option<api_key>)`, or `None` if the user cancels.
fn collect_web_search(terminal: &mut Tui) -> Result<Option<(String, Option<String>)>> {
    let options: Vec<Opt> = WEB_SEARCH_OPTIONS
        .iter()
        .map(|(label, detail, _)| Opt::new(*label, *detail))
        .collect();
    let index = match select(
        terminal,
        "Web search",
        "Which backend should the web_search tool use?",
        &options,
        0,
    )? {
        Some(index) => index,
        None => return Ok(None),
    };
    let (label, _, id) = WEB_SEARCH_OPTIONS[index];

    // DuckDuckGo: no key.
    if id == "duckduckgo" {
        return Ok(Some(("duckduckgo".to_string(), None)));
    }

    // xAI: reuse an existing sign-in; otherwise let them paste a key or defer.
    if id == "xai" {
        let signed_in = crate::llm::xai_oauth::token_path()
            .map(|path| path.exists())
            .unwrap_or(false);
        if signed_in {
            notice(terminal, "Using your existing xAI sign-in for web search.")?;
            return Ok(Some(("xai".to_string(), None)));
        }
        let key = match text_input(
            terminal,
            "xAI API key (optional)",
            "Paste an xAI API key, or leave empty to sign in later with /login xai.",
            "",
        )? {
            Some(value) => value,
            None => return Ok(None),
        };
        let key = key.trim();
        return Ok(Some((
            "xai".to_string(),
            (!key.is_empty()).then(|| key.to_string()),
        )));
    }

    // Keyed backends (brave/tavily/exa/serper): paste a key, or fall back.
    let key = match text_input(
        terminal,
        &format!("{label} API key"),
        "Paste your API key. Stored locally in ~/.wizard/credentials.toml (0600).",
        "",
    )? {
        Some(value) => value,
        None => return Ok(None),
    };
    let key = key.trim();
    if key.is_empty() {
        notice(terminal, "No key entered: using DuckDuckGo for web search.")?;
        return Ok(Some(("duckduckgo".to_string(), None)));
    }
    Ok(Some((id.to_string(), Some(key.to_string()))))
}

/// Optional final step: offer to import artifacts from an existing Claude Code
/// install (`~/.claude`). Returns the chosen selection, or `None` to skip (Esc,
/// or nothing toggled).
fn collect_claude_import(terminal: &mut Tui) -> Result<Option<ImportSelection>> {
    let (mcp, commands, verbs) = import_claude::counts();
    let options = [
        Opt::new(
            format!("MCP servers ({mcp})"),
            "merge into ~/.wizard/mcp.toml",
        ),
        Opt::new(
            format!("Custom commands ({commands})"),
            "copy into ~/.wizard/commands/",
        ),
        Opt::new(
            format!("Spinner verbs ({verbs})"),
            "adopt Claude Code's spinner verbs",
        ),
    ];
    let checked = match multi_select(
        terminal,
        "Import from Claude Code",
        "Found ~/.claude: bring over any of these?",
        &options,
    )? {
        Some(checked) => checked,
        None => return Ok(None), // skipped
    };
    let selection = ImportSelection {
        mcp: checked.first().copied().unwrap_or(false),
        commands: checked.get(1).copied().unwrap_or(false),
        verbs: checked.get(2).copied().unwrap_or(false),
    };
    Ok((!selection.is_empty()).then_some(selection))
}

/// Ask for a cloud provider's key the way the Telegram step asks for a bot
/// token: paste the secret first, then name the env var that overrides it.
/// Returns `(pasted_key, env_var_name)`, or `None` if the user cancels.
///
/// Asking only for the *name of an environment variable* is what this step
/// used to do, and it is why onboarding could print "Wizard is configured"
/// over a setup that 401s on the first turn: the user had exported nothing,
/// and the only complaint was a `tracing::warn!` nobody sees. The paste is the
/// primary answer now; the variable is the documented override.
fn collect_api_key(
    terminal: &mut Tui,
    label: &str,
    noun: &str,
    default_env: &str,
) -> Result<Option<(Option<String>, String)>> {
    let key = match text_input(
        terminal,
        &format!("{label} {noun}"),
        &format!(
            "Paste your {noun} here. Stored locally in ~/.wizard/credentials.toml \
             (mode 0600), never in config.toml. Leave empty to use an env var instead."
        ),
        "",
    )? {
        Some(value) => value,
        None => return Ok(None),
    };
    let key = key.trim().to_string();
    let key = (!key.is_empty()).then_some(key);

    let env = match text_input(
        terminal,
        &format!("{noun} env var (override)"),
        if key.is_some() {
            "Exporting this variable overrides the key you just pasted."
        } else {
            "Nothing pasted, so Wizard reads this variable; export it before the first turn."
        },
        default_env,
    )? {
        Some(value) => value,
        None => return Ok(None),
    };
    // Whether the pair actually adds up to a usable key is reported by
    // `print_summary`, which sees the environment too and can say so without
    // costing the user another keypress here.
    Ok(Some((key, env)))
}

/// Pick a model from `models` plus a "type a custom tag" row; on the custom
/// row, fall through to a free-text input defaulting to `custom_default`.
/// Returns `Ok(None)` on cancel.
fn pick_model(
    terminal: &mut Tui,
    subtitle: &str,
    models: &[(String, String)],
    custom_default: &str,
) -> Result<Option<String>> {
    let mut options: Vec<Opt> = models
        .iter()
        .map(|(value, detail)| Opt::new(value, detail))
        .collect();
    options.push(Opt::new("Type a custom tag…", ""));
    let custom_index = options.len() - 1;

    let selected = match select(terminal, "Model", subtitle, &options, 0)? {
        Some(index) => index,
        None => return Ok(None),
    };
    if selected == custom_index {
        match text_input(
            terminal,
            "Custom model tag",
            "Enter the exact model tag.",
            custom_default,
        )? {
            Some(model) => Ok(Some(model)),
            None => Ok(None),
        }
    } else {
        Ok(Some(models[selected].0.clone()))
    }
}

fn collect_llamacpp(terminal: &mut Tui) -> Result<Option<ProviderAnswers>> {
    let (suggested, explanation) = hardware::suggest_gguf();
    let dir = models_dir();
    let existing = existing_ggufs(&dir);

    // Each option stands for a GGUF path. GGUFs already on disk come first
    // (so a downloaded model is the default), followed by the tiers the
    // installer knows how to fetch.
    let mut options: Vec<Opt> = Vec::new();
    let mut paths: Vec<String> = Vec::new();
    for path in &existing {
        let label = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string();
        options.push(Opt::new(label, "found in ~/.wizard/models"));
        paths.push(path.display().to_string());
    }
    for tier in hardware::GGUF_TIERS {
        if existing
            .iter()
            .any(|path| path.file_name().is_some_and(|name| name == tier.file))
        {
            continue; // already listed as a downloaded model
        }
        let detail = if tier.file == suggested.file {
            format!("{}, recommended for this machine", tier.name)
        } else {
            tier.name.to_string()
        };
        options.push(Opt::new(tier.file, detail));
        paths.push(dir.join(tier.file).display().to_string());
    }
    let custom_index = options.len();
    options.push(Opt::new("Type a custom GGUF path…", ""));
    let default = if existing.is_empty() {
        paths
            .iter()
            .position(|path| {
                Path::new(path)
                    .file_name()
                    .is_some_and(|n| n == suggested.file)
            })
            .unwrap_or(0)
    } else {
        0
    };

    let selected = match select(terminal, "Model", &explanation, &options, default)? {
        Some(index) => index,
        None => return Ok(None),
    };
    let gguf_path = if selected == custom_index {
        // Re-prompt until a non-empty path is entered.
        loop {
            let path = match text_input(
                terminal,
                "GGUF path",
                "Absolute path to a .gguf model file.",
                "",
            )? {
                Some(value) => value,
                None => return Ok(None),
            };
            if path.trim().is_empty() {
                notice(terminal, "enter a path to a .gguf file")?;
            } else {
                break path;
            }
        }
    } else {
        paths[selected].clone()
    };

    let base_url = match text_input(
        terminal,
        "llama-server URL",
        "Where llama-server listens. Wizard starts it for you if it isn't running.",
        LLAMACPP_BASE_URL,
    )? {
        Some(value) => value.trim_end_matches('/').to_string(),
        None => return Ok(None),
    };

    Ok(Some(ProviderAnswers {
        provider_name: "local".to_string(),
        kind: ProviderKind::LLAMACPP,
        base_url,
        model: gguf_model_tag(&gguf_path),
        api_key_env: None,
        api_key: None,
        gguf_path: Some(gguf_path),
    }))
}

/// Model rows for the BYOM — Ollama picker: models already pulled first (a
/// model created with `ollama create` shows up here), then the
/// hardware-suggested tier and the remaining known tiers, which Wizard pulls
/// on first run when missing.
fn ollama_model_options(installed: &[String], suggested: &str) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = Vec::new();
    for tag in installed {
        let detail = if tag == suggested {
            "already pulled, recommended for this machine"
        } else {
            "already pulled"
        };
        rows.push((tag.clone(), detail.to_string()));
    }
    if !installed.iter().any(|tag| tag == suggested) {
        rows.push((
            suggested.to_string(),
            "recommended for this machine, pulled on first run".to_string(),
        ));
    }
    for tier in OLLAMA_TIERS {
        if *tier != suggested && !installed.iter().any(|tag| tag == tier) {
            rows.push(((*tier).to_string(), "pulled on first run".to_string()));
        }
    }
    rows
}

fn collect_ollama(terminal: &mut Tui) -> Result<Option<ProviderAnswers>> {
    let (suggested, explanation) = hardware::suggest_model();
    let models = ollama_model_options(&installed_ollama_models(), &suggested);
    let model = match pick_model(terminal, &explanation, &models, &suggested)? {
        Some(model) => model,
        None => return Ok(None),
    };
    Ok(Some(ProviderAnswers {
        provider_name: "local".to_string(),
        kind: ProviderKind::OLLAMA,
        base_url: OLLAMA_BASE_URL.to_string(),
        model,
        api_key_env: None,
        api_key: None,
        gguf_path: None,
    }))
}

fn collect_openai(terminal: &mut Tui) -> Result<Option<ProviderAnswers>> {
    let models: Vec<(String, String)> = OPENAI_MODELS
        .iter()
        .enumerate()
        .map(|(i, m)| {
            (
                (*m).to_string(),
                if i == 0 {
                    "default".to_string()
                } else {
                    String::new()
                },
            )
        })
        .collect();
    let model = match pick_model(
        terminal,
        "OpenAI-compatible model.",
        &models,
        OPENAI_MODELS[0],
    )? {
        Some(model) => model,
        None => return Ok(None),
    };
    let (api_key, api_key_env) =
        match collect_api_key(terminal, "OpenAI", "API key", OPENAI_KEY_ENV)? {
            Some(pair) => pair,
            None => return Ok(None),
        };
    Ok(Some(ProviderAnswers {
        provider_name: "openai".to_string(),
        kind: ProviderKind::OPENAI,
        base_url: OPENAI_BASE_URL.to_string(),
        model,
        api_key_env: Some(api_key_env),
        api_key,
        gguf_path: None,
    }))
}

fn collect_anthropic(terminal: &mut Tui) -> Result<Option<ProviderAnswers>> {
    let models: Vec<(String, String)> = ANTHROPIC_MODELS
        .iter()
        .enumerate()
        .map(|(i, m)| {
            (
                (*m).to_string(),
                if i == 0 {
                    "latest Claude (default)".to_string()
                } else {
                    String::new()
                },
            )
        })
        .collect();
    let model = match pick_model(
        terminal,
        "Anthropic Claude model.",
        &models,
        ANTHROPIC_MODELS[0],
    )? {
        Some(model) => model,
        None => return Ok(None),
    };
    let (api_key, api_key_env) =
        match collect_api_key(terminal, "Anthropic", "API key", ANTHROPIC_KEY_ENV)? {
            Some(pair) => pair,
            None => return Ok(None),
        };
    Ok(Some(ProviderAnswers {
        provider_name: "claude".to_string(),
        kind: ProviderKind::ANTHROPIC,
        base_url: ANTHROPIC_BASE_URL.to_string(),
        model,
        api_key_env: Some(api_key_env),
        api_key,
        gguf_path: None,
    }))
}

fn collect_openrouter(terminal: &mut Tui) -> Result<Option<ProviderAnswers>> {
    let models: Vec<(String, String)> = vec![(
        OPENROUTER_MODEL.to_string(),
        "Auto Router picks a model per prompt (default)".to_string(),
    )];
    let model = match pick_model(
        terminal,
        "OpenRouter model (any vendor/model tag from openrouter.ai/models).",
        &models,
        OPENROUTER_MODEL,
    )? {
        Some(model) => model,
        None => return Ok(None),
    };
    let (api_key, api_key_env) =
        match collect_api_key(terminal, "OpenRouter", "API key", OPENROUTER_KEY_ENV)? {
            Some(pair) => pair,
            None => return Ok(None),
        };
    Ok(Some(ProviderAnswers {
        provider_name: "openrouter".to_string(),
        kind: ProviderKind::OPENROUTER,
        base_url: OPENROUTER_BASE_URL.to_string(),
        model,
        api_key_env: Some(api_key_env),
        api_key,
        gguf_path: None,
    }))
}

fn collect_cloudflare(terminal: &mut Tui) -> Result<Option<ProviderAnswers>> {
    // The account id is folded into the endpoint URL (Workers AI is
    // account-scoped); the token is read from an env var at request time.
    let account_id = match text_input(
        terminal,
        "Cloudflare account ID",
        "Dashboard → Workers AI (or `wrangler whoami`). Folded into the endpoint URL.",
        "",
    )? {
        Some(value) => value,
        None => return Ok(None),
    };
    let models: Vec<(String, String)> = vec![
        (
            CLOUDFLARE_MODEL.to_string(),
            "GLM 5.2, most capable (default)".to_string(),
        ),
        (
            "@cf/zai-org/glm-4.7-flash".to_string(),
            "GLM 4.7 Flash: cheaper, faster".to_string(),
        ),
    ];
    let model = match pick_model(
        terminal,
        "Cloudflare Workers AI model (any @cf/... text-generation tag).",
        &models,
        CLOUDFLARE_MODEL,
    )? {
        Some(model) => model,
        None => return Ok(None),
    };
    let (api_key, api_key_env) =
        match collect_api_key(terminal, "Cloudflare", "API token", CLOUDFLARE_KEY_ENV)? {
            Some(pair) => pair,
            None => return Ok(None),
        };
    Ok(Some(ProviderAnswers {
        provider_name: "cloudflare".to_string(),
        kind: ProviderKind::CLOUDFLARE,
        base_url: crate::llm::registry::defaults::cloudflare_base_url(&account_id),
        model,
        api_key_env: Some(api_key_env),
        api_key,
        gguf_path: None,
    }))
}

fn collect_xai(terminal: &mut Tui) -> Result<Option<ProviderAnswers>> {
    let models: Vec<(String, String)> = XAI_MODELS
        .iter()
        .enumerate()
        .map(|(i, m)| {
            (
                (*m).to_string(),
                if i == 0 {
                    "default".to_string()
                } else {
                    String::new()
                },
            )
        })
        .collect();
    let model = match pick_model(terminal, "xAI Grok model.", &models, XAI_MODELS[0])? {
        Some(model) => model,
        None => return Ok(None),
    };
    let (api_key, api_key_env) = match collect_api_key(terminal, "xAI", "API key", XAI_KEY_ENV)? {
        Some(pair) => pair,
        None => return Ok(None),
    };
    Ok(Some(ProviderAnswers {
        provider_name: "xai".to_string(),
        kind: ProviderKind::XAI,
        base_url: XAI_BASE_URL.to_string(),
        model,
        api_key_env: Some(api_key_env),
        api_key,
        gguf_path: None,
    }))
}

fn collect_xai_oauth(terminal: &mut Tui) -> Result<Option<ProviderAnswers>> {
    let models: Vec<(String, String)> = XAI_MODELS
        .iter()
        .enumerate()
        .map(|(i, m)| {
            (
                (*m).to_string(),
                if i == 0 {
                    "default".to_string()
                } else {
                    String::new()
                },
            )
        })
        .collect();
    let model = match pick_model(
        terminal,
        "xAI Grok model (sign in with `wizard --login xai` after setup).",
        &models,
        XAI_MODELS[0],
    )? {
        Some(model) => model,
        None => return Ok(None),
    };
    Ok(Some(ProviderAnswers {
        provider_name: "xai".to_string(),
        kind: ProviderKind::XAI_OAUTH,
        base_url: XAI_BASE_URL.to_string(),
        model,
        api_key_env: None,
        api_key: None,
        gguf_path: None,
    }))
}

/// The "More cloud providers" submenu: every OpenAI-compatible preset from
/// [`crate::llm::compat::PRESETS`], then the usual model + key-env questions.
fn collect_compat_menu(terminal: &mut Tui) -> Result<Option<ProviderAnswers>> {
    let options: Vec<Opt> = crate::llm::compat::PRESETS
        .iter()
        .map(|preset| Opt::new(preset.label, preset.detail))
        .collect();
    let index = match select(
        terminal,
        "Provider",
        "All OpenAI-compatible: pick one.",
        &options,
        0,
    )? {
        Some(index) => index,
        None => return Ok(None),
    };
    collect_compat(terminal, &crate::llm::compat::PRESETS[index])
}

/// Model + key-env questions for one OpenAI-compatible preset.
fn collect_compat(
    terminal: &mut Tui,
    preset: &crate::llm::compat::CompatPreset,
) -> Result<Option<ProviderAnswers>> {
    let models: Vec<(String, String)> = preset
        .models
        .iter()
        .enumerate()
        .map(|(i, m)| {
            (
                (*m).to_string(),
                if i == 0 {
                    "default".to_string()
                } else {
                    String::new()
                },
            )
        })
        .collect();
    let model = match pick_model(
        terminal,
        &format!("{} model.", preset.label),
        &models,
        preset.default_model(),
    )? {
        Some(model) => model,
        None => return Ok(None),
    };
    let (api_key, api_key_env) =
        match collect_api_key(terminal, preset.label, "API key", preset.key_env)? {
            Some(pair) => pair,
            None => return Ok(None),
        };
    Ok(Some(ProviderAnswers {
        provider_name: preset.name.to_string(),
        kind: ProviderKind::OPENAI,
        base_url: preset.base_url.to_string(),
        model,
        api_key_env: Some(api_key_env),
        api_key,
        gguf_path: None,
    }))
}

fn collect_custom(terminal: &mut Tui) -> Result<Option<ProviderAnswers>> {
    let base_url = match text_input(
        terminal,
        "Base URL",
        "OpenAI-compatible endpoint (e.g. http://localhost:8000/v1).",
        OPENAI_BASE_URL,
    )? {
        Some(value) => value,
        None => return Ok(None),
    };
    let model = match text_input(terminal, "Model tag", "The model name to request.", "")? {
        Some(value) => value,
        None => return Ok(None),
    };
    // A custom endpoint may need no key at all (a local vLLM, say), so both
    // answers are allowed to be empty here.
    let (api_key, api_key_env) =
        match collect_api_key(terminal, "Custom endpoint", "API key", OPENAI_KEY_ENV)? {
            Some(pair) => pair,
            None => return Ok(None),
        };
    let api_key_env = if api_key_env.trim().is_empty() {
        None
    } else {
        Some(api_key_env)
    };
    Ok(Some(ProviderAnswers {
        provider_name: "custom".to_string(),
        kind: ProviderKind::OPENAI,
        base_url,
        model,
        api_key_env,
        api_key,
        gguf_path: None,
    }))
}

/// The API-key lines of the summary, given what is on disk (`stored`), which
/// env var the provider reads (`env`), and whether that variable currently
/// holds a non-blank value (`exported`).
///
/// Pure, because the thing it has to get right is a precedence rule that lives
/// somewhere else: [`crate::config::ProviderConfig::resolved_key`] reads the
/// **env var first** and only then the stored key. The (stored, exported) case
/// is the one this creates and the one that used to be reported backwards: a
/// user who exported `OPENAI_API_KEY` months ago (which is what onboarding
/// itself used to tell everyone to do), then re-ran `wizard --onboard` and
/// pasted a fresh key, was told the pasted key was in use while the first turn
/// went out with the stale export and 401'd. The summary has to name the key
/// that will actually be sent.
fn api_key_summary(stored: bool, env: Option<&str>, exported: bool) -> Vec<String> {
    const STORED: &str = "  • API key: stored in ~/.wizard/credentials.toml (mode 0600)";
    match (stored, env) {
        (true, Some(env)) if exported => vec![
            format!("  ⚠  API key: ${env} is exported, and it wins over the stored key."),
            format!("     Wizard will send ${env}. Run `unset {env}` (or re-export it)"),
            "     to use the key just stored in ~/.wizard/credentials.toml.".to_string(),
        ],
        (true, Some(env)) => vec![
            STORED.to_string(),
            format!("    (export {env}=... to override it for a run)"),
        ],
        (true, None) => vec![STORED.to_string()],
        (false, Some(env)) if exported => vec![format!("  • API key: read from ${env}")],
        (false, Some(env)) => vec![
            "  ⚠  no API key yet: requests will fail with 401.".to_string(),
            format!("     export {env}=...   (or paste one: /provider inside Wizard)"),
        ],
        (false, None) => {
            vec!["  • no API key configured (fine if the endpoint needs none)".to_string()]
        }
    }
}

/// Print a clean plaintext summary plus concrete next steps to stdout, after
/// the alternate screen has been left.
fn print_summary(config: &Config) {
    let provider = config.active();
    let path = Config::path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "~/.wizard/config.toml".to_string());

    println!();
    println!("✓ Wizard is configured.");
    println!();
    println!("  provider : {} ({})", provider.name, provider.kind);
    println!("  model    : {}", provider.model);
    println!("  endpoint : {}", provider.base_url);
    println!("  gateway  : {}", config.gateway.kind);
    println!("  mode     : {}", config.mode);
    println!("  config   : {path}");
    println!();
    println!("Next steps:");

    // The two backends with something on this machine to say a word about are
    // still named here, because the advice is about *their* artifacts — a GGUF
    // file, an `ollama pull` — and there is nothing on a descriptor that would
    // let a stranger's local backend produce it. Everything past them is
    // generated from the descriptor, so a new cloud provider gets the right
    // closing line without touching this function.
    let manages_server = provider
        .descriptor()
        .is_some_and(|descriptor| descriptor.manages_local_server());
    if manages_server {
        match provider.gguf_path.as_deref() {
            Some(path) if Path::new(path).exists() => {
                println!("  • llama-server starts automatically (model: {path})");
            }
            Some(path)
                if Path::new(path)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .and_then(hardware::gguf_tier_for_file)
                    .is_some() =>
            {
                println!("  • first run downloads the model and starts llama-server for you");
                println!("    (model: {path})");
            }
            Some(path) => {
                println!("  • download the model to: {path}");
                println!("    (Wizard then starts llama-server automatically)");
            }
            None => {
                println!(
                    "  • start the server: llama-server -m <model.gguf> --port {}",
                    crate::config::DEFAULT_LLAMACPP_PORT
                );
            }
        }
    } else if cfg!(feature = "provider-ollama") && provider.kind == ProviderKind::OLLAMA {
        // Gated because the tag comparison is the plugin's: `ollama list`
        // prints `llama3:latest` where a config says `llama3`, and one
        // canonicalizer for that is better than a second copy here. Without
        // the plugin there is no `kind = "ollama"` to advise about anyway, and
        // the generic credential advice below is the honest fallback.
        #[cfg(feature = "provider-ollama")]
        if crate::plugins::ollama::model_installed(&provider.model, &installed_ollama_models()) {
            println!("  • model already pulled: {}", provider.model);
        } else {
            println!(
                "  • first run pulls the model for you (model: {})",
                provider.model
            );
        }
    } else {
        match provider.credentials() {
            Credentials::ApiKey { .. } => {
                // Report the actual state rather than a generic instruction: a
                // summary that says "Wizard is configured" over a setup with no
                // key anywhere is how the first turn came to 401 in silence.
                let stored = crate::credentials::get(&provider.name)
                    .is_some_and(|key| !key.trim().is_empty());
                let env = provider.api_key_env.as_deref();
                let exported = env.is_some_and(|name| {
                    std::env::var(name).is_ok_and(|value| !value.trim().is_empty())
                });
                for line in api_key_summary(stored, env, exported) {
                    println!("{line}");
                }
            }
            Credentials::Account { login } => {
                let display = provider
                    .descriptor()
                    .map(|descriptor| descriptor.display_name().to_string())
                    .unwrap_or_else(|| provider.kind.to_string());
                println!("  • sign in to {display}:  wizard --login {login}");
            }
            // A local backend Wizard neither starts nor stocks has nothing to
            // set up, so the summary above is already the whole answer.
            Credentials::Local => {}
        }
    }

    if config.gateway.kind == GatewayKind::Telegram {
        let env = config.gateway.token_env();
        let token_stored = crate::credentials::get(crate::credentials::GATEWAY_TOKEN)
            .is_some_and(|t| !t.trim().is_empty());
        if token_stored {
            println!("  • Telegram bot token: stored in ~/.wizard/credentials.toml");
        } else {
            println!("  • store the bot token (credentials preferred over env):");
            println!("        # ~/.wizard/credentials.toml  (mode 0600)");
            println!("        [keys]");
            println!("        telegram = \"<token from @BotFather>\"");
            println!("    or: export {env}=...");
        }
        // The allow-list is closed: empty refuses everyone, which otherwise
        // presents as a bot that never answers.
        if config.gateway.allowed_chat_ids.is_empty() {
            println!("  ⚠  no allowed chat IDs: the gateway will refuse every message.");
            println!("     Run `wizard gateway setup`: it has you message the bot, reports");
            println!("     your chat id, and (with your say-so) writes it here:");
            println!();
            println!("        [gateway]");
            println!("        allowed_chat_ids = [<your chat id>]");
        } else {
            println!(
                "  • allowed chat IDs: {:?} (every other chat is refused)",
                config.gateway.allowed_chat_ids
            );
        }
        println!();
        println!("  ⚠  The gateway is a long-running process: messages get no reply");
        println!("     until it is running. Start it in the project you want it to");
        println!("     operate on:");
        println!();
        println!("        cd ~/your/project && wizard --gateway");
        println!();
        println!("     Keep it running (or install a user service so it survives logout):");
        println!();
        println!("        mkdir -p ~/.config/systemd/user");
        println!("        # copy contrib/wizard-gateway.service, set WorkingDirectory");
        println!("        # (or set Environment=WIZARD_GATEWAY_CWD=/path/to/project)");
        println!("        systemctl --user daemon-reload");
        println!("        systemctl --user enable --now wizard-gateway");
        println!("        journalctl --user -u wizard-gateway -f");
        println!();
        println!("     Full docs: docs/gateway.md");
    }

    println!("  • start Wizard:    wizard");
    println!("  • change settings: run /setup anytime inside Wizard");
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboarding::tests::{all_shipped_kinds, base_answers};

    /// The Ollama picker must offer every tag the hardware suggestion can
    /// produce. The 4B tier was missing, so an 8 GB laptop was suggested
    /// `qwen3.5:4b` and then could not pick it back after moving the cursor,
    /// while the GGUF picker (which lists `hardware::GGUF_TIERS` in full) had
    /// no such gap.
    #[test]
    fn ollama_tiers_offer_every_suggested_tag() {
        // Boundaries either side of every tier in `suggest_ollama_model`.
        for gb in [0, 1, 7, 8, 17, 18, 23, 24, 64, 512] {
            let suggested = hardware::suggest_ollama_model(gb);
            assert!(
                OLLAMA_TIERS.contains(&suggested),
                "{gb} GB suggests {suggested}, which the picker does not offer: {OLLAMA_TIERS:?}"
            );
        }
        assert!(OLLAMA_TIERS.contains(&"qwen3.5:4b"), "{OLLAMA_TIERS:?}");
    }

    /// Every tier stays offered even when the suggestion is one of them (no
    /// duplicate row), and the suggested tag is always present.
    #[test]
    fn ollama_model_options_list_each_tier_once() {
        let rows = ollama_model_options(&[], "qwen3.5:4b");
        let tags: Vec<&str> = rows.iter().map(|(tag, _)| tag.as_str()).collect();
        assert!(tags.contains(&"qwen3.5:4b"), "{tags:?}");
        for tier in OLLAMA_TIERS {
            assert_eq!(
                tags.iter().filter(|tag| *tag == tier).count(),
                1,
                "{tier} should appear exactly once: {tags:?}"
            );
        }
    }

    /// Adversarial: the summary must name the key the *next turn* will send,
    /// not the one most recently written. `ProviderConfig::resolved_key` reads
    /// the env var first, so a user who has `export OPENAI_API_KEY=<revoked>`
    /// in their shell rc (what onboarding used to tell everyone to do) and
    /// then pastes a fresh key was being told the pasted key was in use while
    /// the first request went out with the stale export and 401'd.
    #[test]
    fn the_summary_names_the_key_that_actually_wins() {
        let overridden = api_key_summary(true, Some("OPENAI_API_KEY"), true).join("\n");
        assert!(
            overridden.contains("OPENAI_API_KEY") && overridden.contains("wins"),
            "an exported key overrides the stored one and the summary must say so: {overridden}"
        );
        assert!(
            !overridden.contains("(export OPENAI_API_KEY=... to override it for a run)"),
            "must not offer to export what is already exported: {overridden}"
        );
        assert!(
            overridden.contains("unset OPENAI_API_KEY"),
            "the summary has to say how to get the pasted key back: {overridden}"
        );

        // Stored with the variable unset: the stored key is what is sent, and
        // exporting is the documented one-run override.
        let stored = api_key_summary(true, Some("OPENAI_API_KEY"), false).join("\n");
        assert!(stored.contains("credentials.toml"), "{stored}");
        assert!(stored.contains("export OPENAI_API_KEY=..."), "{stored}");
        assert!(!stored.contains("wins"), "{stored}");

        // Nothing stored, nothing exported: the state that 401s, called out.
        let neither = api_key_summary(false, Some("OPENAI_API_KEY"), false).join("\n");
        assert!(neither.contains("401"), "{neither}");

        // Nothing stored but exported: the env var is the key, no warning.
        let exported = api_key_summary(false, Some("OPENAI_API_KEY"), true).join("\n");
        assert_eq!(exported, "  • API key: read from $OPENAI_API_KEY");

        // No env var configured at all (a custom endpoint).
        assert!(
            api_key_summary(true, None, false)
                .join("\n")
                .contains("credentials.toml")
        );
        assert!(
            api_key_summary(false, None, false)
                .join("\n")
                .contains("no API key configured")
        );
    }

    #[test]
    fn web_search_choice_lands_in_config_but_the_key_does_not() {
        let answers = Answers {
            web_search_backend: "brave".to_string(),
            web_search_api_key: Some("brv-secret-key".to_string()),
            ..base_answers()
        };
        let config = answers.into_config();
        assert_eq!(config.web.search_backend, "brave");
        // The pasted key goes to credentials.toml, never config.toml.
        let toml = toml::to_string(&config).expect("serialize");
        assert!(
            !toml.contains("brv-secret-key"),
            "web-search key must not appear in config: {toml}"
        );
    }

    /// The menu offers a row exactly when this build can carry it out.
    ///
    /// This is the bug the table replaced a literal array to fix: a stripped
    /// build used to print "Anthropic (Claude)" and then fail at `build()`
    /// when it was picked. Asserted over feature sets rather than over the
    /// live registry, so the claim holds on every leg of
    /// `contrib/check-provider-plugins.sh` and not only on the one that is
    /// running.
    #[test]
    fn the_provider_menu_offers_only_what_this_build_installed() {
        // Nothing installed: nothing to offer, and onboarding says so rather
        // than drawing an empty picker.
        assert!(provider_choices(&[]).is_empty());

        // One backend: its rows and no others. Anthropic is the narrowest —
        // one kind, one row — so it is the sharpest version of the claim.
        let anthropic = provider_choices(&[ProviderKind::ANTHROPIC]);
        assert_eq!(anthropic.len(), 1);
        assert_eq!(anthropic[0].label, "Anthropic (Claude)");

        // The OpenAI kind carries three rows, because the compat presets and
        // the custom-endpoint row are both `kind = "openai"`.
        let openai: Vec<&str> = provider_choices(&[ProviderKind::OPENAI])
            .iter()
            .map(|choice| choice.label)
            .collect();
        assert_eq!(
            openai,
            [
                "OpenAI / OpenAI-compatible",
                "More cloud providers",
                "Custom OpenAI-compatible endpoint",
            ]
        );

        // "Local" needs either local backend, not both.
        for kind in [ProviderKind::LLAMACPP, ProviderKind::OLLAMA] {
            assert!(
                provider_choices(std::slice::from_ref(&kind))
                    .iter()
                    .any(|choice| choice.label == "Local"),
                "the one-click local row should survive on {kind} alone"
            );
        }

        // Filtering preserves the display order, which is the property an
        // index-dispatched menu had for free and a filtered one has to keep.
        let full = provider_choices(&crate::llm::registry::kinds());
        let labels: Vec<&str> = full.iter().map(|choice| choice.label).collect();
        let mut sorted = labels.clone();
        sorted.sort_by_key(|label| {
            provider_choices(&all_shipped_kinds())
                .iter()
                .position(|choice| choice.label == *label)
                .expect("every offered row is a row")
        });
        assert_eq!(labels, sorted);
    }

    /// A stock build's menu is the one it has always been: eleven rows, in
    /// the order they were written, xAI first.
    ///
    /// The point of the change was to stop offering what a build lacks, not
    /// to redesign the menu, and this is the half of that claim a filtered
    /// list could quietly break.
    #[test]
    #[cfg(all(
        feature = "provider-anthropic",
        feature = "provider-cloudflare",
        feature = "provider-llamacpp",
        feature = "provider-ollama",
        feature = "provider-openai",
        feature = "provider-xai",
    ))]
    fn a_stock_build_offers_the_menu_it_always_did() {
        let labels: Vec<&str> = provider_choices(&crate::llm::registry::kinds())
            .iter()
            .map(|choice| choice.label)
            .collect();
        assert_eq!(
            labels,
            [
                "xAI account sign-in",
                "xAI (Grok), API key",
                "Local",
                "OpenRouter",
                "Cloudflare Workers AI",
                "OpenAI / OpenAI-compatible",
                "Anthropic (Claude)",
                "More cloud providers",
                "Custom OpenAI-compatible endpoint",
                "BYOM: llama.cpp",
                "BYOM: Ollama",
            ]
        );
    }

    #[test]
    fn ollama_picker_lists_installed_models_first() {
        let installed = vec!["my-coder:latest".to_string(), "qwen3.5:9b".to_string()];
        let rows = ollama_model_options(&installed, "qwen3.6:27b");
        let tags: Vec<&str> = rows.iter().map(|(tag, _)| tag.as_str()).collect();
        assert_eq!(
            tags,
            vec![
                "my-coder:latest",
                "qwen3.5:9b",
                "qwen3.6:27b",
                "qwen3.6:35b",
                "qwen3.5:4b"
            ]
        );
        assert_eq!(rows[0].1, "already pulled");
        assert!(rows[2].1.contains("recommended"));
        assert!(rows[2].1.contains("pulled on first run"));
        assert_eq!(rows[3].1, "pulled on first run");
        // The 4B tier is offered even on a machine that was suggested a
        // bigger one: a user who knows their box is busy can pick down.
        assert_eq!(rows[4].1, "pulled on first run");
    }

    #[test]
    fn ollama_picker_marks_an_installed_suggestion_without_repeating_it() {
        let rows = ollama_model_options(&["qwen3.6:27b".to_string()], "qwen3.6:27b");
        assert_eq!(rows[0].0, "qwen3.6:27b");
        assert!(rows[0].1.contains("already pulled"));
        assert!(rows[0].1.contains("recommended"));
        assert_eq!(
            rows.iter().filter(|(tag, _)| tag == "qwen3.6:27b").count(),
            1,
            "the suggestion must not reappear as a download row"
        );
    }

    #[test]
    fn ollama_picker_with_nothing_installed_leads_with_the_suggestion() {
        let rows = ollama_model_options(&[], "qwen3.5:9b");
        assert_eq!(rows[0].0, "qwen3.5:9b");
        assert!(rows[0].1.contains("recommended"));
        assert!(rows.iter().all(|(_, detail)| detail.contains("first run")));
        // Every known tier is offered exactly once, including the 4B one an
        // 8 GB machine needs.
        let tags: Vec<&str> = rows.iter().map(|(tag, _)| tag.as_str()).collect();
        assert_eq!(
            tags,
            vec!["qwen3.5:9b", "qwen3.6:35b", "qwen3.6:27b", "qwen3.5:4b"]
        );
    }
}
