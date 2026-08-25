//! First-run onboarding: a small full-screen wizard that asks for a provider,
//! model, optional messaging gateway, and mode, then writes
//! `~/.wizard/config.toml`.
//!
//! The module is split into two halves:
//! - **Pure logic** ([`Answers`], [`Answers::into_config`], [`parse_chat_ids`],
//!   and the option tables) — fully unit-tested without a terminal.
//! - **TUI** ([`run`] and the private `select` / `text_input` event loops) —
//!   ratatui + crossterm rendering in the existing aesthetic (white accent,
//!   dim rounded borders, transparent background).
//!
//! Keeping the answer → [`Config`] mapping pure means the interesting behavior
//! is testable; the TUI layer is a thin shell over it.

use std::io::Stdout;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::config::{
    Config, Credentials, GatewayConfig, GatewayKind, Mode, ProviderConfig, ProviderKind,
};
use crate::hardware::{self, GgufModel};
use crate::import_claude::{self, ImportSelection};
use crate::skin::Skin;
use crate::theme::{self, Token};

// The wizard paints with the same semantic tokens as the main TUI, so
// `NO_COLOR`, `WIZARD_COLOR` and `WIZARD_THEME` mean here what they mean
// everywhere else. It used to carry its own white/gray/darkgray constants,
// which made first-run setup the one screen that ignored all three: a machine
// with no config is exactly where a user who sets `NO_COLOR=1` meets Wizard
// first, and the wizard painted colors at them anyway. Under the default
// theme the three tokens below are the same white/gray/darkgray, so nothing
// looks different unless the user asked for it.

/// The one accent: titles, the selection marker, the input caret.
fn accent() -> Style {
    theme::style(Token::Accent)
}

/// Dim chrome: borders, hints, footers.
fn dim() -> Style {
    theme::style(Token::Faint)
}

/// Secondary text: subtitles and option details.
fn text_dim() -> Style {
    theme::style(Token::Muted)
}

// ---------------------------------------------------------------------------
// Pure logic (unit-tested)
// ---------------------------------------------------------------------------

/// The collected answers from the wizard. Converting this into a [`Config`]
/// ([`Answers::into_config`]) is pure and unit-tested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answers {
    /// Provider id stored in [`ProviderConfig::name`] (e.g. `"local"`).
    pub provider_name: String,
    /// Backend kind for the single configured provider.
    pub kind: ProviderKind,
    /// Base URL for the provider.
    pub base_url: String,
    /// Model tag.
    pub model: String,
    /// Env var that overrides the stored API key (cloud providers only).
    pub api_key_env: Option<String>,
    /// A pasted provider API key, stored under [`Self::provider_name`] in
    /// `~/.wizard/credentials.toml` (0600) by [`run_blocking`] after the pure
    /// config mapping, exactly like [`Self::web_search_api_key`]. It never
    /// reaches `config.toml`.
    pub provider_api_key: Option<String>,
    /// Path to the GGUF model file (llama.cpp only) — lets Wizard spawn
    /// `llama-server` itself.
    pub gguf_path: Option<String>,
    /// Messaging gateway to configure.
    pub gateway_kind: GatewayKind,
    /// Env var holding the gateway bot token (Telegram only).
    pub gateway_token_env: Option<String>,
    /// Allowed inbound chat IDs (Telegram only). The list is closed: an empty
    /// list allows **nobody**, which is the shipped default (see
    /// [`crate::gateway::is_authorized`]). Leaving this empty ships a gateway
    /// that refuses every message, not one that answers everyone.
    pub gateway_allowed_chat_ids: Vec<i64>,
    /// Personality mode.
    pub mode: Mode,
    /// Which coding agent's terminal chrome the TUI wears. Cosmetic: it
    /// changes glyphs, framing and wording, never the commands or the model.
    pub skin: Skin,
    /// `web_search` backend id (`"duckduckgo"`, `"brave"`, `"tavily"`,
    /// `"exa"`, `"serper"`, or `"xai"`).
    pub web_search_backend: String,
    /// A pasted API key for the chosen web-search backend, stored under the
    /// backend name in `~/.wizard/credentials.toml` by [`run_blocking`] after
    /// the pure config mapping (so [`Answers::into_config`] stays pure).
    pub web_search_api_key: Option<String>,
    /// Pasted Telegram bot token (Telegram gateway only). Stored under
    /// `telegram` in `~/.wizard/credentials.toml` by [`run_blocking`] after the
    /// pure config mapping — same pattern as [`Self::web_search_api_key`].
    pub gateway_bot_token: Option<String>,
    /// Artifacts to import from an existing Claude Code install, if any. The
    /// actual import (file writes + spinner verbs) runs in [`run_blocking`]
    /// after [`Answers::into_config`], so this is consumed there rather than in
    /// the pure config mapping.
    pub claude_import: Option<ImportSelection>,
}

impl Answers {
    /// Build a [`Config`] from the answers: one configured provider (set
    /// active), the chosen mode, the `[gateway]` section, and — for an Ollama
    /// choice — the legacy `model` / `ollama_host` fields mirrored for
    /// back-compat with pre-`providers` config readers.
    pub fn into_config(self) -> Config {
        let mut config = Config::default();

        let provider = ProviderConfig {
            name: self.provider_name.clone(),
            kind: self.kind.clone(),
            base_url: self.base_url.clone(),
            model: self.model.clone(),
            api_key_env: self.api_key_env.clone(),
            gguf_path: self.gguf_path.clone(),
            usd_per_mtok_in: None,
            usd_per_mtok_out: None,
        };

        // Mirror an Ollama choice into the legacy fields so config files remain
        // readable by code paths that predate the providers table.
        if self.kind == ProviderKind::OLLAMA {
            config.model = self.model.clone();
            config.ollama_host = self.base_url.clone();
        }

        // Mirror a llama.cpp choice into the top-level fields so the same
        // local provider is synthesized if the providers table ever empties
        // (e.g. `/provider remove`).
        if self.kind == ProviderKind::LLAMACPP {
            config.llamacpp_host = self.base_url.clone();
            config.gguf_path = self.gguf_path.clone();
        }

        config.providers = vec![provider];
        config.active_provider = Some(self.provider_name);
        config.mode = self.mode;
        // Written even when it is the default, because it was answered: a key
        // that is present means "this was chosen", and `/ui` rewrites the same
        // key when it is chosen again.
        config.ui.skin = Some(self.skin.key().to_string());
        config.web.search_backend = self.web_search_backend;
        config.gateway = GatewayConfig {
            kind: self.gateway_kind,
            token_env: self.gateway_token_env,
            allowed_chat_ids: self.gateway_allowed_chat_ids,
        };
        config
    }
}

/// Persist the secrets [`Answers`] carries and [`Answers::into_config`]
/// deliberately drops. `store` is the credential writer
/// ([`crate::credentials::store`] in production, which writes 0600).
///
/// The *names* it is called with are the contract, because each secret is read
/// back from somewhere else entirely: the provider key under
/// `provider_name`, which is what [`crate::config::ProviderConfig`] resolves
/// against; the web-search key under the backend name the `web_search` tool
/// resolves at call time; the bot token under
/// [`crate::credentials::GATEWAY_TOKEN`], which is what the gateway
/// reads. A typo in any of those stores the secret where nothing looks for it
/// and leaves a setup that looks finished and 401s on the first turn, which is
/// exactly the failure asking for the key was meant to remove. Injecting the
/// writer is what lets a test pin the names without touching the shared
/// credentials file.
///
/// A write failure is reported and does not abort onboarding: the config is
/// still worth saving, and the summary then reports the key as missing.
fn store_pasted_secrets(answers: &Answers, mut store: impl FnMut(&str, &str) -> Result<()>) {
    let mut persist = |name: &str, secret: Option<&str>, label: &str| {
        let Some(secret) = secret.map(str::trim).filter(|secret| !secret.is_empty()) else {
            return;
        };
        if let Err(err) = store(name, secret) {
            eprintln!("warning: could not save the {label}: {err:#}");
        }
    };

    persist(
        &answers.provider_name,
        answers.provider_api_key.as_deref(),
        &format!("{} API key", answers.provider_name),
    );
    persist(
        &answers.web_search_backend,
        answers.web_search_api_key.as_deref(),
        &format!("{} API key", answers.web_search_backend),
    );
    persist(
        crate::credentials::GATEWAY_TOKEN,
        answers.gateway_bot_token.as_deref(),
        "Telegram bot token",
    );
}

/// Parse a comma-separated list of numeric chat IDs. Whitespace and empty
/// entries are ignored; an empty input yields an empty list, which the gateway
/// reads as "allow nobody" (see [`crate::gateway::is_authorized`]) and warns
/// about in the summary. A non-numeric entry is an error naming the offending
/// token.
pub fn parse_chat_ids(input: &str) -> Result<Vec<i64>, String> {
    let mut ids = Vec::new();
    for token in input.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let id: i64 = token
            .parse()
            .map_err(|_| format!("'{token}' is not a valid numeric chat id"))?;
        ids.push(id);
    }
    Ok(ids)
}

/// OpenAI model options offered in the picker (first is the default).
const OPENAI_MODELS: &[&str] = &[
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.3-codex",
];
/// Anthropic model options offered in the picker (first is the default — the
/// latest Claude).
const ANTHROPIC_MODELS: &[&str] = &[
    "claude-fable-5",
    "claude-opus-4-8",
    "claude-sonnet-5",
    "claude-haiku-4-5",
];
/// xAI (Grok) model options offered in the picker (first is the default).
const XAI_MODELS: &[&str] = &[
    "grok-4.6",
    "grok-4.5",
    "grok-4.3",
    "grok-4.20-0309-reasoning",
    "grok-build-0.1",
];
/// Ollama tier options offered alongside the hardware-suggested default. Must
/// list every tag [`hardware::suggest_ollama_model`] can return, including the
/// 4B tier: a machine below 8 GB is suggested `qwen3.5:4b`, and leaving it out
/// of the picker meant the one model such a machine can actually load was
/// unreachable the moment the user changed the default (the GGUF picker offers
/// its whole tier table, so the two pickers disagreed).
const OLLAMA_TIERS: &[&str] = &["qwen3.6:35b", "qwen3.6:27b", "qwen3.5:9b", "qwen3.5:4b"];

/// Default base URL for a local llama.cpp `llama-server`.
const LLAMACPP_BASE_URL: &str = crate::config::DEFAULT_LLAMACPP_HOST;
/// Default base URL for a local Ollama server.
const OLLAMA_BASE_URL: &str = "http://127.0.0.1:11434";
/// Default base URL for the OpenAI API.
const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
/// Default base URL for the Anthropic API.
const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
/// Default base URL for the xAI API.
const XAI_BASE_URL: &str = crate::llm::xai_oauth::DEFAULT_BASE_URL;
/// Default base URL for the OpenRouter API.
const OPENROUTER_BASE_URL: &str = crate::llm::registry::defaults::OPENROUTER_BASE_URL;
/// Default env var name for the OpenAI key.
const OPENAI_KEY_ENV: &str = "OPENAI_API_KEY";
/// Default env var name for the Anthropic key.
const ANTHROPIC_KEY_ENV: &str = "ANTHROPIC_API_KEY";
/// Default env var name for the xAI key.
const XAI_KEY_ENV: &str = crate::llm::xai_oauth::DEFAULT_KEY_ENV;
/// Default env var name for the OpenRouter key.
const OPENROUTER_KEY_ENV: &str = crate::llm::registry::defaults::OPENROUTER_KEY_ENV;
/// Default OpenRouter model (the Auto Router).
const OPENROUTER_MODEL: &str = crate::llm::registry::defaults::OPENROUTER_MODEL;
/// Default env var name for the Cloudflare API token.
const CLOUDFLARE_KEY_ENV: &str = crate::llm::registry::defaults::CLOUDFLARE_KEY_ENV;
/// Default Cloudflare Workers AI model (GLM 5.2).
const CLOUDFLARE_MODEL: &str = crate::llm::registry::defaults::CLOUDFLARE_MODEL;

// ---------------------------------------------------------------------------
// TUI entry point
// ---------------------------------------------------------------------------

/// Run the onboarding wizard. Returns `Ok(Some(config))` once the user
/// finishes (the config has already been saved to `~/.wizard/config.toml` and
/// a plaintext summary printed), or `Ok(None)` if the user cancelled
/// (Esc / Ctrl-C). Terminal setup/teardown is restored on every exit path,
/// including errors.
///
/// The interactive loop is synchronous (blocking crossterm reads); it runs on
/// a blocking thread so it never stalls the async runtime.
pub async fn run() -> Result<Option<Config>> {
    tokio::task::spawn_blocking(run_blocking)
        .await
        .context("onboarding task panicked")?
}

/// Synchronous core of [`run`].
fn run_blocking() -> Result<Option<Config>> {
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
    let outcome = collect_answers(&mut terminal);
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

type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Drive the sequence of steps. Returns `Ok(None)` as soon as any step is
/// cancelled.
fn collect_answers(terminal: &mut Tui) -> Result<Option<Answers>> {
    // Step 1 — provider, xAI first. "Local" is one pick: no further model
    // questions — Wizard self-configures llama.cpp (or an Ollama install that
    // already has a model) and downloads a hardware-sized GGUF on first run.
    // The BYOM local flavors sit alongside the cloud providers for people who
    // want to bring their own model and pick the pieces themselves.
    let provider_options = [
        Opt::new("xAI account sign-in", "grok-4.6 via OAuth, no API key"),
        Opt::new("xAI (Grok), API key", "grok-4.6 via XAI_API_KEY"),
        Opt::new(
            "Local",
            "one pick — llama.cpp & Ollama set up for you, model sized to this machine; \
             private, no API key",
        ),
        Opt::new("OpenRouter", "hundreds of models via OPENROUTER_API_KEY"),
        Opt::new(
            "Cloudflare Workers AI",
            "GLM 5.2 via CLOUDFLARE_API_TOKEN (+ account id)",
        ),
        Opt::new("OpenAI / OpenAI-compatible", "gpt-5.6 family and friends"),
        Opt::new("Anthropic (Claude)", "claude-fable-5"),
        Opt::new(
            "More cloud providers",
            "Gemini, DeepSeek, Groq, Mistral, Kimi, GLM, …",
        ),
        Opt::new("Custom OpenAI-compatible endpoint", "any base URL"),
        Opt::new(
            "BYOM — llama.cpp",
            "bring your own model: any GGUF, your server URL",
        ),
        Opt::new(
            "BYOM — Ollama",
            "bring your own model: any Ollama tag, pulled on first run",
        ),
    ];
    let provider = match select(
        terminal,
        "Provider",
        "Where should Wizard send its requests?",
        &provider_options,
        0,
    )? {
        Some(index) => index,
        None => return Ok(None),
    };

    // Step 2 — model (+ key env / base url, depending on provider). The
    // one-click local pick asks nothing further.
    let collected = match provider {
        0 => match collect_xai_oauth(terminal)? {
            Some(c) => c,
            None => return Ok(None),
        },
        1 => match collect_xai(terminal)? {
            Some(c) => c,
            None => return Ok(None),
        },
        2 => match collect_local_auto(terminal)? {
            Some(c) => c,
            None => return Ok(None),
        },
        3 => match collect_openrouter(terminal)? {
            Some(c) => c,
            None => return Ok(None),
        },
        4 => match collect_cloudflare(terminal)? {
            Some(c) => c,
            None => return Ok(None),
        },
        5 => match collect_openai(terminal)? {
            Some(c) => c,
            None => return Ok(None),
        },
        6 => match collect_anthropic(terminal)? {
            Some(c) => c,
            None => return Ok(None),
        },
        7 => match collect_compat_menu(terminal)? {
            Some(c) => c,
            None => return Ok(None),
        },
        8 => match collect_custom(terminal)? {
            Some(c) => c,
            None => return Ok(None),
        },
        9 => match collect_llamacpp(terminal)? {
            Some(c) => c,
            None => return Ok(None),
        },
        _ => match collect_ollama(terminal)? {
            Some(c) => c,
            None => return Ok(None),
        },
    };

    // Step 3 — messaging gateway.
    let gateway_options = [
        Opt::new("None — terminal only", "recommended"),
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

    let (gateway_kind, gateway_token_env, gateway_allowed_chat_ids, gateway_bot_token) = if gateway
        == 1
    {
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
                             Run `wizard gateway setup` afterwards — it has you message the \
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
        (
            GatewayKind::Telegram,
            Some(token_env),
            allowed,
            gateway_bot_token,
        )
    } else {
        (GatewayKind::None, None, Vec::new(), None)
    };

    // Step 4 — mode.
    let mode_options = [
        Opt::new(
            "Genie — interactive",
            "bypass permissions; acts without asking (recommended)",
        ),
        Opt::new("Sovereign — autonomous", "autonomous; works continuously"),
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
        "Which terminal UI should Wizard wear? (looks only — same commands either way; \
         change it any time with /ui)",
        &skin_options,
        0,
    )? {
        Some(index) => Skin::ALL[index],
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
        gateway_kind,
        gateway_token_env,
        gateway_allowed_chat_ids,
        mode,
        skin,
        web_search_backend,
        web_search_api_key,
        gateway_bot_token,
        claude_import,
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
        notice(
            terminal,
            "No key entered — using DuckDuckGo for web search.",
        )?;
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
        "Found ~/.claude — bring over any of these?",
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

/// Per-provider answers gathered in step 2.
struct ProviderAnswers {
    provider_name: String,
    kind: ProviderKind,
    base_url: String,
    model: String,
    api_key_env: Option<String>,
    /// The key the user pasted, if any. Stored in credentials.toml (0600) by
    /// [`run_blocking`], never written to config.toml.
    api_key: Option<String>,
    gguf_path: Option<String>,
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

/// `~/.wizard/models/` — where `install.sh` downloads GGUF files.
fn models_dir() -> PathBuf {
    Config::wizard_dir()
        .map(|dir| dir.join("models"))
        .unwrap_or_else(|_| PathBuf::from("~/.wizard/models"))
}

/// List `*.gguf` files in `dir`, sorted by name. Empty when the directory is
/// missing or unreadable.
fn existing_ggufs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("gguf"))
        })
        .collect();
    files.sort();
    files
}

/// Model tag for a GGUF path: the filename without the `.gguf` extension
/// (e.g. `/x/Qwen3.6-27B-Q4_K_M.gguf` → `Qwen3.6-27B-Q4_K_M`).
fn gguf_model_tag(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .unwrap_or("default")
        .to_string()
}

/// What the one-click "Local" pick resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalPlan {
    /// llama.cpp serving `gguf_path`; when the file is missing, Wizard
    /// downloads it (and installs llama-server) on first run.
    LlamaCpp { gguf_path: String },
    /// An existing Ollama install that already has `model` pulled.
    Ollama { model: String },
}

/// Decide the one-click local plan. Pure, so it is unit-tested:
/// 1. A GGUF already in `~/.wizard/models` wins — nothing to download
///    (preferring the hardware-suggested tier, else the first by name).
/// 2. Otherwise an Ollama install with at least one model pulled is reused
///    (the hardware-suggested tag when pulled, else the first listed).
/// 3. Otherwise llama.cpp with the suggested tier: Wizard downloads the GGUF
///    and installs llama-server itself on first run.
pub fn plan_local_auto(
    existing: &[PathBuf],
    models_dir: &Path,
    ollama_models: &[String],
    suggested: &GgufModel,
    suggested_tag: &str,
) -> LocalPlan {
    if !existing.is_empty() {
        let chosen = existing
            .iter()
            .find(|path| path.file_name().is_some_and(|name| name == suggested.file))
            .unwrap_or(&existing[0]);
        return LocalPlan::LlamaCpp {
            gguf_path: chosen.display().to_string(),
        };
    }
    if !ollama_models.is_empty() {
        let model = ollama_models
            .iter()
            .find(|model| model.as_str() == suggested_tag)
            .unwrap_or(&ollama_models[0]);
        return LocalPlan::Ollama {
            model: model.clone(),
        };
    }
    LocalPlan::LlamaCpp {
        gguf_path: models_dir.join(suggested.file).display().to_string(),
    }
}

/// Model tags an installed Ollama already has pulled (empty when Ollama is
/// absent, its server is down, or the listing fails).
fn installed_ollama_models() -> Vec<String> {
    if !crate::platform::host::on_path("ollama") {
        return Vec::new();
    }
    let Ok(output) = std::process::Command::new("ollama").arg("list").output() else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .skip(1) // header row
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_string)
        .collect()
}

/// The one-click "Local" pick: no questions. Resolve the plan from what is
/// already on this machine and the hardware suggestion.
///
/// The pick asks nothing, which is the point, but it must not therefore *say*
/// nothing: on a machine below the smallest tier's requirement the hardware
/// suggestion carries a warning that local inference will not work here, and
/// this path used to throw the explanation away. The user then paid for a
/// multi-GB download and met the same verdict from the preflight afterwards.
/// The other local paths show the explanation as the picker subtitle; this one
/// shows it as a notice, and only when it is a warning, so the one-click pick
/// stays one click on every machine that can actually run a model.
///
/// `Ok(None)` when the notice is cancelled (Esc), like every other step.
fn collect_local_auto(terminal: &mut Tui) -> Result<Option<ProviderAnswers>> {
    let (suggested, explanation) = hardware::suggest_gguf();
    let (suggested_tag, _) = hardware::suggest_model();
    if hardware::suggestion_is_a_warning(&explanation) {
        notice(terminal, &explanation)?;
    }
    let dir = models_dir();
    let existing = existing_ggufs(&dir);
    let answers = match plan_local_auto(
        &existing,
        &dir,
        &installed_ollama_models(),
        suggested,
        &suggested_tag,
    ) {
        LocalPlan::LlamaCpp { gguf_path } => ProviderAnswers {
            provider_name: "local".to_string(),
            kind: ProviderKind::LLAMACPP,
            base_url: LLAMACPP_BASE_URL.to_string(),
            model: gguf_model_tag(&gguf_path),
            api_key_env: None,
            api_key: None,
            gguf_path: Some(gguf_path),
        },
        LocalPlan::Ollama { model } => ProviderAnswers {
            provider_name: "local".to_string(),
            kind: ProviderKind::OLLAMA,
            base_url: OLLAMA_BASE_URL.to_string(),
            model,
            api_key_env: None,
            api_key: None,
            gguf_path: None,
        },
    };
    Ok(Some(answers))
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
            format!("{} — recommended for this machine", tier.name)
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
            "already pulled — recommended for this machine"
        } else {
            "already pulled"
        };
        rows.push((tag.clone(), detail.to_string()));
    }
    if !installed.iter().any(|tag| tag == suggested) {
        rows.push((
            suggested.to_string(),
            "recommended for this machine — pulled on first run".to_string(),
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
            "GLM 5.2 — most capable (default)".to_string(),
        ),
        (
            "@cf/zai-org/glm-4.7-flash".to_string(),
            "GLM 4.7 Flash — cheaper, faster".to_string(),
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
        "All OpenAI-compatible — pick one.",
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

// ---------------------------------------------------------------------------
// Summary
// ---------------------------------------------------------------------------

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
            println!("     Run `wizard gateway setup` — it has you message the bot, reports");
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
        println!("  ⚠  The gateway is a long-running process — messages get no reply");
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
    println!("  • change settings: run /settings anytime inside Wizard");
    println!();
}

// ---------------------------------------------------------------------------
// Terminal lifecycle (mirrors src/app.rs)
// ---------------------------------------------------------------------------

fn setup_terminal() -> Result<Tui> {
    crossterm::terminal::enable_raw_mode().context("enabling raw mode")?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)
        .context("entering alternate screen")?;
    Terminal::new(CrosstermBackend::new(stdout)).context("creating terminal")
}

/// Restore the terminal if (and only if) raw mode is active. Safe on any exit
/// path; idempotent.
fn restore_terminal_best_effort() {
    if crossterm::terminal::is_raw_mode_enabled().unwrap_or(false) {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen);
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

// ---------------------------------------------------------------------------
// Step widgets: select list, text input, transient notice
// ---------------------------------------------------------------------------

/// One selectable row.
struct Opt {
    label: String,
    detail: String,
}

impl Opt {
    fn new(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            detail: detail.into(),
        }
    }
}

/// True when `key` is Esc or Ctrl-C — the universal cancel chord.
fn is_cancel(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Esc)
        || (key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')))
}

/// Render a vertical list of options; navigate with ↑/↓, confirm with Enter.
/// Returns the selected index, or `None` on Esc/Ctrl-C.
fn select(
    terminal: &mut Tui,
    title: &str,
    subtitle: &str,
    options: &[Opt],
    default: usize,
) -> Result<Option<usize>> {
    let mut selected = default.min(options.len().saturating_sub(1));
    loop {
        terminal.draw(|frame| draw_select(frame, title, subtitle, options, selected))?;
        let Some(key) = next_key()? else { continue };
        if is_cancel(&key) {
            return Ok(None);
        }
        match key.code {
            KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') => {
                selected = if selected == 0 {
                    options.len().saturating_sub(1)
                } else {
                    selected - 1
                };
            }
            KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => {
                selected = if selected + 1 >= options.len() {
                    0
                } else {
                    selected + 1
                };
            }
            KeyCode::Enter => return Ok(Some(selected)),
            _ => {}
        }
    }
}

/// Render a checklist of options; ↑/↓ move, Space toggles the current row,
/// Enter confirms. Returns the per-row checked state, or `None` on Esc/Ctrl-C.
/// All rows start unchecked.
fn multi_select(
    terminal: &mut Tui,
    title: &str,
    subtitle: &str,
    options: &[Opt],
) -> Result<Option<Vec<bool>>> {
    let mut checked = vec![false; options.len()];
    let mut selected = 0usize;
    loop {
        terminal
            .draw(|frame| draw_multi_select(frame, title, subtitle, options, &checked, selected))?;
        let Some(key) = next_key()? else { continue };
        if is_cancel(&key) {
            return Ok(None);
        }
        match key.code {
            KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') => {
                selected = if selected == 0 {
                    options.len().saturating_sub(1)
                } else {
                    selected - 1
                };
            }
            KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => {
                selected = if selected + 1 >= options.len() {
                    0
                } else {
                    selected + 1
                };
            }
            KeyCode::Char(' ') => {
                if let Some(slot) = checked.get_mut(selected) {
                    *slot = !*slot;
                }
            }
            KeyCode::Enter => return Ok(Some(checked)),
            _ => {}
        }
    }
}

/// Free-text input step. Enter accepts (empty submits the default); Esc/Ctrl-C
/// cancels. Returns the entered (or default) value.
fn text_input(
    terminal: &mut Tui,
    title: &str,
    subtitle: &str,
    default: &str,
) -> Result<Option<String>> {
    let mut buffer = String::new();
    loop {
        terminal.draw(|frame| draw_input(frame, title, subtitle, &buffer, default))?;
        let Some(key) = next_key()? else { continue };
        if is_cancel(&key) {
            return Ok(None);
        }
        match key.code {
            KeyCode::Enter => {
                let value = if buffer.trim().is_empty() {
                    default.to_string()
                } else {
                    buffer.trim().to_string()
                };
                return Ok(Some(value));
            }
            KeyCode::Backspace => {
                buffer.pop();
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                buffer.push(c);
            }
            _ => {}
        }
    }
}

/// Show a transient message until the user presses a key (used for validation
/// errors). Always returns once a key is read.
fn notice(terminal: &mut Tui, message: &str) -> Result<()> {
    loop {
        terminal.draw(|frame| draw_notice(frame, message))?;
        if let Some(key) = next_key()?
            && (is_cancel(&key) || matches!(key.code, KeyCode::Enter | KeyCode::Char(_)))
        {
            return Ok(());
        }
    }
}

/// Block until the next key *press* (ignoring releases), polling so the draw
/// loop stays responsive. `None` means "nothing yet, redraw".
fn next_key() -> Result<Option<KeyEvent>> {
    if event::poll(Duration::from_millis(150)).context("polling terminal events")?
        && let Event::Key(key) = event::read().context("reading terminal event")?
        && key.kind != KeyEventKind::Release
    {
        return Ok(Some(key));
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Compose the outer frame (header + bordered body + footer) and return the
/// inner content area for the step to fill.
fn frame_body(frame: &mut ratatui::Frame, title: &str, subtitle: &str, footer: &str) -> Rect {
    let area = frame.area();
    let [header, body, foot] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(area);

    let header_lines = Text::from(vec![
        Line::from(Span::styled(
            format!("  {title}"),
            accent().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(format!("  {subtitle}"), text_dim())),
    ]);
    frame.render_widget(Paragraph::new(header_lines), header);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(theme::border_type())
        .border_style(dim())
        .title(Span::styled(" wizard setup ", dim()));
    let inner = block.inner(body);
    frame.render_widget(block, body);

    frame.render_widget(
        Paragraph::new(Span::styled(format!("  {footer}"), dim())),
        foot,
    );
    inner
}

fn draw_select(
    frame: &mut ratatui::Frame,
    title: &str,
    subtitle: &str,
    options: &[Opt],
    selected: usize,
) {
    let inner = frame_body(
        frame,
        title,
        subtitle,
        "↑/↓ move · enter select · esc cancel",
    );
    let mut lines = Vec::with_capacity(options.len());
    for (index, option) in options.iter().enumerate() {
        let active = index == selected;
        let marker = if active { "▸ " } else { "  " };
        let label_style = if active {
            accent().add_modifier(Modifier::BOLD)
        } else {
            text_dim()
        };
        let mut spans = vec![
            Span::styled(format!(" {marker}"), accent()),
            Span::styled(option.label.clone(), label_style),
        ];
        if !option.detail.is_empty() {
            spans.push(Span::styled(format!("   {}", option.detail), dim()));
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn draw_multi_select(
    frame: &mut ratatui::Frame,
    title: &str,
    subtitle: &str,
    options: &[Opt],
    checked: &[bool],
    selected: usize,
) {
    let inner = frame_body(
        frame,
        title,
        subtitle,
        "↑/↓ move · space toggle · enter confirm · esc skip",
    );
    let mut lines = Vec::with_capacity(options.len());
    for (index, option) in options.iter().enumerate() {
        let active = index == selected;
        let marker = if active { "▸ " } else { "  " };
        let box_ = if checked.get(index).copied().unwrap_or(false) {
            "[x]"
        } else {
            "[ ]"
        };
        let label_style = if active {
            accent().add_modifier(Modifier::BOLD)
        } else {
            text_dim()
        };
        let mut spans = vec![
            Span::styled(format!(" {marker}"), accent()),
            Span::styled(format!("{box_} "), accent()),
            Span::styled(option.label.clone(), label_style),
        ];
        if !option.detail.is_empty() {
            spans.push(Span::styled(format!("   {}", option.detail), dim()));
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn draw_input(
    frame: &mut ratatui::Frame,
    title: &str,
    subtitle: &str,
    buffer: &str,
    default: &str,
) {
    let inner = frame_body(frame, title, subtitle, "enter accept · esc cancel");
    let shown = if buffer.is_empty() {
        Span::styled(
            if default.is_empty() {
                "  (type a value)".to_string()
            } else {
                format!("  {default}")
            },
            dim(),
        )
    } else {
        Span::styled(format!("  {buffer}"), accent())
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(" ▸ ", accent()),
        shown,
        Span::styled("▏", accent()),
    ])];
    if !default.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("   default: {default}"),
            dim(),
        )));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn draw_notice(frame: &mut ratatui::Frame, message: &str) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(theme::border_type())
        .border_style(theme::style(Token::Warning))
        .title(Span::styled(
            " notice ",
            theme::style(Token::Warning).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(format!("  {message}"), text_dim())),
            Line::from(""),
            Line::from(Span::styled("  press any key to continue", dim())),
        ])
        .alignment(Alignment::Left),
        inner,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_answers() -> Answers {
        Answers {
            provider_name: "local".to_string(),
            kind: ProviderKind::OLLAMA,
            base_url: OLLAMA_BASE_URL.to_string(),
            model: "qwen3.6:27b".to_string(),
            api_key_env: None,
            provider_api_key: None,
            gguf_path: None,
            gateway_kind: GatewayKind::None,
            gateway_token_env: None,
            gateway_allowed_chat_ids: Vec::new(),
            mode: Mode::Genie,
            skin: Skin::Wizard,
            web_search_backend: "duckduckgo".to_string(),
            web_search_api_key: None,
            gateway_bot_token: None,
            claude_import: None,
        }
    }

    #[test]
    fn ollama_answers_mirror_legacy_fields() {
        let answers = Answers {
            base_url: "http://10.0.0.5:11434".to_string(),
            model: "qwen3.5:9b".to_string(),
            ..base_answers()
        };
        let config = answers.into_config();
        assert_eq!(config.providers.len(), 1);
        assert_eq!(config.active_provider.as_deref(), Some("local"));
        assert_eq!(config.active().kind, ProviderKind::OLLAMA);
        assert_eq!(config.active().model, "qwen3.5:9b");
        // Legacy fields mirror the Ollama choice for back-compat.
        assert_eq!(config.model, "qwen3.5:9b");
        assert_eq!(config.ollama_host, "http://10.0.0.5:11434");
        assert_eq!(config.gateway.kind, GatewayKind::None);
        assert_eq!(config.mode, Mode::Genie);
    }

    /// Adversarial: first-run setup is where a user who exports `NO_COLOR=1`
    /// meets Wizard, and it is the one screen that used to ignore it. The
    /// wizard carried its own white/gray/darkgray constants, so neither
    /// `NO_COLOR`, `WIZARD_COLOR` nor `WIZARD_THEME` reached it while the TUI
    /// it hands off to honoured all three.
    #[test]
    fn the_wizard_paints_with_the_active_theme_not_a_palette_of_its_own() {
        use ratatui::style::Color;
        use std::sync::Arc;

        use crate::theme::ColorDepth;

        // A terminal that wants no color at all: every one of the wizard's
        // three styles has to come back uncolored.
        {
            let _pin = theme::pin(Arc::new(theme::minimal().with_depth(ColorDepth::Mono)));
            for (name, style) in [
                ("accent", accent()),
                ("dim", dim()),
                ("text_dim", text_dim()),
            ] {
                assert_eq!(style.fg, Some(Color::Reset), "{name} kept a color");
            }
            assert_eq!(theme::style(Token::Warning).fg, Some(Color::Reset));
        }

        // And under the default theme they are the palette the wizard used to
        // hard-code, so honouring the theme changed nothing for the user who
        // set none of those variables.
        let _pin = theme::pin(theme::minimal());
        assert_eq!(accent().fg, Some(Color::White));
        assert_eq!(dim().fg, Some(Color::DarkGray));
        assert_eq!(text_dim().fg, Some(Color::Gray));
    }

    #[test]
    fn llamacpp_answers_carry_gguf_and_skip_legacy_ollama_fields() {
        let answers = Answers {
            kind: ProviderKind::LLAMACPP,
            base_url: "http://127.0.0.1:9090".to_string(),
            model: "Qwen3.6-27B-Q4_K_M".to_string(),
            gguf_path: Some("/home/u/.wizard/models/Qwen3.6-27B-Q4_K_M.gguf".to_string()),
            ..base_answers()
        };
        let defaults = Config::default();
        let config = answers.into_config();
        assert_eq!(config.active().kind, ProviderKind::LLAMACPP);
        assert_eq!(config.active().model, "Qwen3.6-27B-Q4_K_M");
        assert_eq!(
            config.active().gguf_path.as_deref(),
            Some("/home/u/.wizard/models/Qwen3.6-27B-Q4_K_M.gguf")
        );
        // Top-level llamacpp fields mirror the choice…
        assert_eq!(config.llamacpp_host, "http://127.0.0.1:9090");
        assert_eq!(config.gguf_path, config.active().gguf_path);
        // …while the legacy Ollama fields stay at their defaults.
        assert_eq!(config.model, defaults.model);
        assert_eq!(config.ollama_host, defaults.ollama_host);
    }

    #[test]
    fn gguf_model_tag_strips_directory_and_extension() {
        assert_eq!(
            gguf_model_tag("/home/u/.wizard/models/Qwen3.5-9B-Q4_K_M.gguf"),
            "Qwen3.5-9B-Q4_K_M"
        );
        assert_eq!(gguf_model_tag("model.gguf"), "model");
        assert_eq!(gguf_model_tag(""), "default");
    }

    #[test]
    fn existing_ggufs_lists_only_gguf_files_sorted() {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in ["b.gguf", "a.GGUF", "notes.txt"] {
            std::fs::write(dir.path().join(name), b"x").expect("write");
        }
        std::fs::create_dir(dir.path().join("sub.gguf")).expect("mkdir");
        let found = existing_ggufs(dir.path());
        let names: Vec<_> = found
            .iter()
            .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
            .collect();
        assert_eq!(names, vec!["a.GGUF", "b.gguf"]);
        // Missing directory → empty, not an error.
        assert!(existing_ggufs(&dir.path().join("missing")).is_empty());
    }

    #[test]
    fn cloud_answers_do_not_touch_legacy_ollama_fields() {
        let answers = Answers {
            provider_name: "claude".to_string(),
            kind: ProviderKind::ANTHROPIC,
            base_url: ANTHROPIC_BASE_URL.to_string(),
            model: "claude-fable-5".to_string(),
            api_key_env: Some(ANTHROPIC_KEY_ENV.to_string()),
            mode: Mode::Sovereign,
            ..base_answers()
        };
        let defaults = Config::default();
        let config = answers.into_config();
        assert_eq!(config.active().name, "claude");
        assert_eq!(config.active().kind, ProviderKind::ANTHROPIC);
        assert_eq!(
            config.active().api_key_env.as_deref(),
            Some(ANTHROPIC_KEY_ENV)
        );
        // Legacy fields untouched (still defaults) since this isn't an Ollama choice.
        assert_eq!(config.model, defaults.model);
        assert_eq!(config.ollama_host, defaults.ollama_host);
        assert_eq!(config.mode, Mode::Sovereign);
    }

    #[test]
    fn xai_answers_build_the_expected_providers() {
        // API-key flavor.
        let answers = Answers {
            provider_name: "xai".to_string(),
            kind: ProviderKind::XAI,
            base_url: XAI_BASE_URL.to_string(),
            model: "grok-4.3".to_string(),
            api_key_env: Some(XAI_KEY_ENV.to_string()),
            ..base_answers()
        };
        let config = answers.into_config();
        assert_eq!(config.active().name, "xai");
        assert_eq!(config.active().kind, ProviderKind::XAI);
        assert_eq!(config.active().base_url, "https://api.x.ai/v1");
        assert_eq!(config.active().api_key_env.as_deref(), Some("XAI_API_KEY"));

        // OAuth flavor: no API key env; credentials come from the token file.
        let answers = Answers {
            provider_name: "xai".to_string(),
            kind: ProviderKind::XAI_OAUTH,
            base_url: XAI_BASE_URL.to_string(),
            model: "grok-4.3".to_string(),
            api_key_env: None,
            ..base_answers()
        };
        let config = answers.into_config();
        assert_eq!(config.active().kind, ProviderKind::XAI_OAUTH);
        assert!(config.active().api_key_env.is_none());
        // Legacy Ollama fields stay untouched for cloud choices.
        let defaults = Config::default();
        assert_eq!(config.model, defaults.model);
        assert_eq!(config.ollama_host, defaults.ollama_host);
    }

    #[test]
    fn openrouter_answers_build_the_expected_provider() {
        let answers = Answers {
            provider_name: "openrouter".to_string(),
            kind: ProviderKind::OPENROUTER,
            base_url: OPENROUTER_BASE_URL.to_string(),
            model: OPENROUTER_MODEL.to_string(),
            api_key_env: Some(OPENROUTER_KEY_ENV.to_string()),
            ..base_answers()
        };
        let config = answers.into_config();
        assert_eq!(config.active().name, "openrouter");
        assert_eq!(config.active().kind, ProviderKind::OPENROUTER);
        assert_eq!(config.active().base_url, "https://openrouter.ai/api/v1");
        assert_eq!(config.active().model, "openrouter/auto");
        assert_eq!(
            config.active().api_key_env.as_deref(),
            Some("OPENROUTER_API_KEY")
        );
        // Legacy Ollama fields stay untouched for cloud choices.
        let defaults = Config::default();
        assert_eq!(config.model, defaults.model);
        assert_eq!(config.ollama_host, defaults.ollama_host);
    }

    #[test]
    fn cloudflare_answers_build_the_expected_provider() {
        let answers = Answers {
            provider_name: "cloudflare".to_string(),
            kind: ProviderKind::CLOUDFLARE,
            base_url: crate::llm::registry::defaults::cloudflare_base_url("acc123"),
            model: CLOUDFLARE_MODEL.to_string(),
            api_key_env: Some(CLOUDFLARE_KEY_ENV.to_string()),
            ..base_answers()
        };
        let config = answers.into_config();
        assert_eq!(config.active().name, "cloudflare");
        assert_eq!(config.active().kind, ProviderKind::CLOUDFLARE);
        assert_eq!(
            config.active().base_url,
            "https://api.cloudflare.com/client/v4/accounts/acc123/ai/v1"
        );
        assert_eq!(config.active().model, "@cf/zai-org/glm-5.2");
        assert_eq!(
            config.active().api_key_env.as_deref(),
            Some("CLOUDFLARE_API_TOKEN")
        );
    }

    #[test]
    fn telegram_gateway_persists_into_config() {
        let answers = Answers {
            gateway_kind: GatewayKind::Telegram,
            gateway_token_env: Some("MY_TOKEN".to_string()),
            gateway_allowed_chat_ids: vec![1, 2, 3],
            // Token is stored via credentials::store in run_blocking, not in
            // config — into_config stays pure.
            gateway_bot_token: Some("123456:ABC-test-token".to_string()),
            ..base_answers()
        };
        let config = answers.into_config();
        assert_eq!(config.gateway.kind, GatewayKind::Telegram);
        assert_eq!(config.gateway.token_env.as_deref(), Some("MY_TOKEN"));
        assert_eq!(config.gateway.allowed_chat_ids, vec![1, 2, 3]);
        // Bot token must never land in config.toml.
        let toml = toml::to_string(&config).expect("serialize");
        assert!(
            !toml.contains("123456:ABC-test-token"),
            "token must not appear in config: {toml}"
        );
    }

    /// Record every `(name, secret)` [`store_pasted_secrets`] writes, instead
    /// of the real credential store: the property under test is *which name*
    /// each secret lands under, and asserting it against a recorder keeps the
    /// test off the process-wide `credentials.toml` that the rest of the suite
    /// writes concurrently.
    fn recording_store(
        recorded: &mut Vec<(String, String)>,
    ) -> impl FnMut(&str, &str) -> Result<()> {
        move |name: &str, secret: &str| {
            recorded.push((name.to_string(), secret.to_string()));
            Ok(())
        }
    }

    /// Adversarial: the provider key onboarding now asks for is a secret, and
    /// `config.toml` is a plain 0644-ish file people paste into issues. The
    /// key belongs in credentials.toml (0600) and nowhere else; only the name
    /// of the overriding env var is config.
    ///
    /// Both halves are asserted here on purpose. `into_config` has no path by
    /// which the key could reach `ProviderConfig`, so the "not in config" half
    /// alone cannot fail however the storage code behaves; the half that can
    /// fail is that the key is handed to the credential store under the same
    /// name `ProviderConfig::resolved_key` reads back.
    #[test]
    fn pasted_provider_key_never_reaches_config() {
        let answers = Answers {
            provider_name: "openai".to_string(),
            kind: ProviderKind::OPENAI,
            base_url: OPENAI_BASE_URL.to_string(),
            model: OPENAI_MODELS[0].to_string(),
            api_key_env: Some(OPENAI_KEY_ENV.to_string()),
            provider_api_key: Some("  sk-pasted-during-onboarding\n".to_string()),
            ..base_answers()
        };

        // The storage step run_blocking performs, with the writer captured.
        let mut recorded: Vec<(String, String)> = Vec::new();
        store_pasted_secrets(&answers, recording_store(&mut recorded));
        assert_eq!(
            recorded,
            vec![(
                "openai".to_string(),
                // Trimmed: a key pasted with surrounding whitespace still works.
                "sk-pasted-during-onboarding".to_string()
            )],
            "the key must be stored under the provider's own name"
        );

        let config = answers.into_config();
        // The name it was stored under is the name the provider resolves by.
        assert_eq!(config.active().name, recorded[0].0);
        assert_eq!(config.active().api_key_env.as_deref(), Some(OPENAI_KEY_ENV));
        let toml = toml::to_string(&config).expect("serialize");
        assert!(
            !toml.contains("sk-pasted-during-onboarding"),
            "a pasted provider key must not appear in config: {toml}"
        );
    }

    /// The web-search key goes under the backend name the `web_search` tool
    /// resolves at call time, and the bot token under the exact key the
    /// gateway reads. A typo in either name stores a live secret where nothing
    /// looks for it, and the failure only shows up as a 401 (or a gateway that
    /// says the token is not set) much later.
    #[test]
    fn pasted_secrets_are_stored_under_the_names_that_read_them_back() {
        let answers = Answers {
            provider_name: "openai".to_string(),
            provider_api_key: Some("sk-provider".to_string()),
            web_search_backend: "brave".to_string(),
            web_search_api_key: Some("brv-secret-key".to_string()),
            gateway_kind: GatewayKind::Telegram,
            gateway_bot_token: Some("123456:ABC-test-token".to_string()),
            ..base_answers()
        };

        let mut recorded: Vec<(String, String)> = Vec::new();
        store_pasted_secrets(&answers, recording_store(&mut recorded));
        assert_eq!(
            recorded,
            vec![
                ("openai".to_string(), "sk-provider".to_string()),
                ("brave".to_string(), "brv-secret-key".to_string()),
                (
                    crate::credentials::GATEWAY_TOKEN.to_string(),
                    "123456:ABC-test-token".to_string()
                ),
            ]
        );
    }

    /// Nothing is written for an answer that was left blank: a store call with
    /// an empty value would shadow a key stored by an earlier run.
    #[test]
    fn blank_answers_store_nothing() {
        let answers = Answers {
            provider_api_key: Some("   ".to_string()),
            web_search_api_key: None,
            gateway_bot_token: Some(String::new()),
            ..base_answers()
        };
        let mut recorded: Vec<(String, String)> = Vec::new();
        store_pasted_secrets(&answers, recording_store(&mut recorded));
        assert!(recorded.is_empty(), "{recorded:?}");
    }

    /// A credential-store failure is reported, not fatal: the config is still
    /// worth saving, and every secret is still attempted.
    #[test]
    fn a_failing_credential_store_does_not_abort_onboarding() {
        let answers = Answers {
            provider_name: "openai".to_string(),
            provider_api_key: Some("sk-provider".to_string()),
            web_search_api_key: Some("brv-secret-key".to_string()),
            gateway_bot_token: Some("123456:ABC".to_string()),
            ..base_answers()
        };
        let mut attempts = 0;
        store_pasted_secrets(&answers, |_, _| {
            attempts += 1;
            Err(anyhow::anyhow!("read-only filesystem"))
        });
        assert_eq!(attempts, 3, "one attempt per pasted secret");
    }

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

    #[test]
    fn onboarding_config_survives_a_toml_round_trip() {
        let answers = Answers {
            kind: ProviderKind::LLAMACPP,
            base_url: "http://127.0.0.1:8080".to_string(),
            model: "Qwen3.6-27B-Q4_K_M".to_string(),
            gguf_path: Some("/m/Qwen3.6-27B-Q4_K_M.gguf".to_string()),
            gateway_kind: GatewayKind::Telegram,
            gateway_token_env: Some("TELEGRAM_BOT_TOKEN".to_string()),
            gateway_allowed_chat_ids: vec![-100123, 42],
            mode: Mode::Sovereign,
            web_search_backend: "tavily".to_string(),
            ..base_answers()
        };
        // Serialize/deserialize exactly as Config::save / Config::load do.
        let raw = toml::to_string_pretty(&answers.into_config()).expect("serialize");
        let reloaded: Config = toml::from_str(&raw).expect("parse");
        assert_eq!(reloaded.active().name, "local");
        assert_eq!(reloaded.active().kind, ProviderKind::LLAMACPP);
        assert_eq!(reloaded.active().model, "Qwen3.6-27B-Q4_K_M");
        assert_eq!(
            reloaded.active().gguf_path.as_deref(),
            Some("/m/Qwen3.6-27B-Q4_K_M.gguf")
        );
        assert_eq!(reloaded.gateway.kind, GatewayKind::Telegram);
        assert_eq!(
            reloaded.gateway.token_env.as_deref(),
            Some("TELEGRAM_BOT_TOKEN")
        );
        assert_eq!(reloaded.gateway.allowed_chat_ids, vec![-100123, 42]);
        assert_eq!(reloaded.mode, Mode::Sovereign);
        assert_eq!(reloaded.web.search_backend, "tavily");
    }

    #[test]
    fn is_cancel_matches_esc_and_ctrl_c_only() {
        assert!(is_cancel(&KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(is_cancel(&KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        )));
        assert!(!is_cancel(&KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::NONE
        )));
        assert!(!is_cancel(&KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL
        )));
    }

    #[test]
    fn parse_chat_ids_handles_empty_and_whitespace() {
        assert_eq!(parse_chat_ids("").unwrap(), Vec::<i64>::new());
        assert_eq!(parse_chat_ids("   ").unwrap(), Vec::<i64>::new());
        assert_eq!(parse_chat_ids(" , , ").unwrap(), Vec::<i64>::new());
    }

    #[test]
    fn parse_chat_ids_parses_numbers_including_negative() {
        assert_eq!(parse_chat_ids("42").unwrap(), vec![42]);
        assert_eq!(
            parse_chat_ids("42, -100123 , 7").unwrap(),
            vec![42, -100123, 7]
        );
    }

    #[test]
    fn parse_chat_ids_rejects_non_numeric() {
        let err = parse_chat_ids("42, abc").expect_err("non-numeric must error");
        assert!(
            err.contains("abc"),
            "error should name the bad token: {err}"
        );
    }

    fn tier(file: &'static str) -> GgufModel {
        GgufModel {
            name: "Test",
            file,
            url: "https://example.com/x.gguf",
            approx_gb: 1,
        }
    }

    #[test]
    fn local_plan_prefers_a_downloaded_gguf() {
        let dir = Path::new("/m");
        let existing = vec![PathBuf::from("/m/a.gguf"), PathBuf::from("/m/big.gguf")];
        // The suggested tier is on disk: it wins over the first-by-name file.
        let plan = plan_local_auto(
            &existing,
            dir,
            &["qwen3.5:9b".to_string()],
            &tier("big.gguf"),
            "qwen3.5:9b",
        );
        assert_eq!(
            plan,
            LocalPlan::LlamaCpp {
                gguf_path: "/m/big.gguf".to_string()
            }
        );
        // Suggested tier not on disk: first existing GGUF wins, still no
        // download and still ahead of any Ollama install.
        let plan = plan_local_auto(
            &existing,
            dir,
            &["qwen3.5:9b".to_string()],
            &tier("other.gguf"),
            "qwen3.5:9b",
        );
        assert_eq!(
            plan,
            LocalPlan::LlamaCpp {
                gguf_path: "/m/a.gguf".to_string()
            }
        );
    }

    #[test]
    fn local_plan_reuses_an_ollama_install_with_models() {
        let dir = Path::new("/m");
        let pulled = vec!["llama3:8b".to_string(), "qwen3.5:9b".to_string()];
        // The hardware-suggested tag is pulled: use it.
        let plan = plan_local_auto(&[], dir, &pulled, &tier("x.gguf"), "qwen3.5:9b");
        assert_eq!(
            plan,
            LocalPlan::Ollama {
                model: "qwen3.5:9b".to_string()
            }
        );
        // Suggested tag not pulled: first listed model.
        let plan = plan_local_auto(&[], dir, &pulled, &tier("x.gguf"), "qwen3.6:27b");
        assert_eq!(
            plan,
            LocalPlan::Ollama {
                model: "llama3:8b".to_string()
            }
        );
    }

    #[test]
    fn local_plan_falls_back_to_a_fresh_llamacpp_download() {
        let plan = plan_local_auto(&[], Path::new("/m"), &[], &tier("big.gguf"), "qwen3.5:9b");
        assert_eq!(
            plan,
            LocalPlan::LlamaCpp {
                gguf_path: "/m/big.gguf".to_string()
            }
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
