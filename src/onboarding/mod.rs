//! First-run onboarding.
//!
//! A first run is one screen ([`run`]): how do you want to run Wizard, four
//! answers, then straight into the TUI with every other setting at its
//! default. The full wizard ([`run_full`], in [`full`]) still asks everything
//! (provider, model, gateway, mode, interface, web search, Claude import) and
//! writes `~/.wizard/config.toml`; `wizard --onboard`, `wizard setup` and the
//! `/setup` menu run it. [`widgets`] holds the screens both draw with.
//!
//! The answer to [`Config`] mapping ([`Answers::into_config`]) is pure and
//! unit-tested; the TUI layer is a thin shell over it.

mod full;
mod widgets;

pub use full::{run_full, run_full_blocking, run_gateway_setup_blocking};

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::{
    Config, Credentials, GatewayConfig, GatewayKind, Mode, ProviderConfig, ProviderKind,
};
use crate::hardware::{self, GgufModel};
use crate::import_claude::ImportSelection;
use crate::skin::Skin;
use crate::theme;

use widgets::{
    Interrupted, Opt, Tui, confirm, notice, restore_terminal_best_effort, secret_input, select,
    setup_terminal, text_input,
};

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
    /// the gateway's `is_authorized`). Leaving this empty ships a gateway
    /// that refuses every message, not one that answers everyone.
    pub gateway_allowed_chat_ids: Vec<i64>,
    /// Personality mode.
    pub mode: Mode,
    /// Which coding agent's terminal chrome the TUI wears. Cosmetic: it
    /// changes glyphs, framing and wording, never the commands or the model.
    /// `None` when the question was never asked (a first run), so the config
    /// keeps deferring to `WIZARD_SKIN` and the default.
    pub skin: Option<Skin>,
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
    /// A first run: the provider answered, everything else at its default.
    fn first_run(provider: ProviderAnswers) -> Self {
        Self {
            provider_name: provider.provider_name,
            kind: provider.kind,
            base_url: provider.base_url,
            model: provider.model,
            api_key_env: provider.api_key_env,
            provider_api_key: provider.api_key,
            gguf_path: provider.gguf_path,
            gateway_kind: GatewayKind::None,
            gateway_token_env: None,
            gateway_allowed_chat_ids: Vec::new(),
            mode: Mode::Genie,
            skin: None,
            web_search_backend: "duckduckgo".to_string(),
            web_search_api_key: None,
            gateway_bot_token: None,
            claude_import: None,
        }
    }

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
        config.ui.skin = self.skin.map(|skin| skin.key().to_string());
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
/// reads as "allow nobody" (see the gateway plugin's `is_authorized`) and warns
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

/// The one-screen first run. Returns the saved config and the card's
/// [`FirstRun`] line, or `Ok(None)` on Esc at the first screen or Ctrl-C
/// anywhere. A pasted key is checked with one request while the screen is
/// still up (a "checking with host…" subtitle, not a bare shell prompt); a
/// rejected one comes back to the key list with the reason and nothing on
/// disk. A browser sign-in, when the answer needs one, runs after the screen
/// is down and prints its URL to the plain terminal the way `wizard --login`
/// does.
pub async fn run() -> Result<Option<(Config, FirstRun)>> {
    let mut screen: Option<Screen> = None;
    let mut retry: Option<String> = None;
    let outcome = loop {
        let reason = retry.take();
        let taken = screen.take();
        let (mut opened, pick) = tokio::task::spawn_blocking(move || {
            let mut opened = match taken {
                Some(opened) => opened,
                None => {
                    local_plan();
                    match Screen::open() {
                        Ok(opened) => opened,
                        Err(err) => return Err(err),
                    }
                }
            };
            let pick = collect_first_run(&mut opened.terminal, reason);
            Ok((opened, pick))
        })
        .await
        .context("onboarding task panicked")??;
        let pick = match pick {
            Ok(Some(pick)) => pick,
            Ok(None) => break Ok(None),
            Err(err) => break Err(err),
        };
        match finish_first_run(&mut opened, pick).await {
            Ok(Finished::Rejected(reason)) => {
                retry = Some(reason);
                screen = Some(opened);
            }
            Ok(Finished::Saved {
                config,
                notice,
                login,
            }) => {
                opened.close();
                if let Some(login) = login {
                    sign_in(login).await?;
                }
                return Ok(Some((*config, FirstRun { notice })));
            }
            Err(err) => break Err(err),
        }
    };
    if let Some(opened) = screen {
        opened.close();
    }
    match outcome {
        Err(err) if err.is::<Interrupted>() => Ok(None),
        other => other,
    }
}

/// What the first run hands the TUI's card besides the config: one warning
/// when the credential check settled nothing (no answer in time, no key to
/// check), otherwise none.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FirstRun {
    pub notice: Option<String>,
}

/// The wizard's terminal: raw mode and the alternate screen, held across
/// the screens and the credential check so nothing shows between them.
struct Screen {
    terminal: Tui,
    /// A skin or theme name that would not load, printed once the terminal
    /// is back.
    warnings: Vec<String>,
}

impl Screen {
    fn open() -> Result<Self> {
        let skin_warning = crate::skin::init(None);
        let theme_warning = theme::init(crate::skin::active().companion_theme());
        let terminal = setup_terminal()?;
        Ok(Self {
            terminal,
            warnings: [skin_warning, theme_warning]
                .into_iter()
                .flatten()
                .collect(),
        })
    }

    fn close(self) {
        restore_terminal_best_effort();
        for warning in &self.warnings {
            eprintln!("warning: {warning}");
        }
    }
}

/// What a pass over the screens came to.
enum Finished {
    /// Saved, the key checked where there was one to check.
    Saved {
        config: Box<Config>,
        notice: Option<String>,
        login: Option<Login>,
    },
    /// A pasted key the provider refused; nothing saved. The text is the key
    /// list's subtitle.
    Rejected(String),
}

/// What the first screen resolved to.
struct FirstRunPick {
    answers: ProviderAnswers,
    login: Option<Login>,
    /// The paste screen's title, kept up while the key is checked.
    title: String,
}

/// A browser sign-in the first screen can ask for.
enum Login {
    Xai,
    #[cfg(feature = "provider-chatgpt")]
    ChatGpt,
}

/// One pass over the screens. `retry` carries a rejected key's reason and
/// starts at the key list; Esc there goes back to the first screen.
///
/// The first screen's rows, before `installed` narrows them. `local` is what
/// the local row would do on this machine, so its size is on the row.
fn first_run_choices(
    installed: &[ProviderKind],
    local: Option<&LocalPlan>,
) -> Vec<ProviderChoice<FirstRunPick>> {
    let keyed = key_providers(installed);
    let mut all = vec![ProviderChoice {
        label: "Sign in with xAI",
        detail: format!("{}, in the browser", XAI_MODELS[0]),
        kinds: vec![ProviderKind::XAI_OAUTH],
        collect: first_xai_oauth,
    }];
    #[cfg(feature = "provider-chatgpt")]
    all.push(ProviderChoice {
        label: "Sign in with ChatGPT",
        detail: format!(
            "{}, on your ChatGPT plan",
            crate::plugins::chatgpt::oauth::DEFAULT_MODEL
        ),
        kinds: vec![ProviderKind::CHATGPT_OAUTH],
        collect: first_chatgpt,
    });
    all.push(ProviderChoice {
        label: "Paste an API key",
        detail: key_row_detail(&keyed),
        kinds: keyed.into_iter().map(|row| row.kind).collect(),
        collect: first_api_key_row,
    });
    all.push(ProviderChoice {
        label: "Run a model locally",
        detail: local.map(local_row_detail).unwrap_or_default(),
        kinds: vec![ProviderKind::LLAMACPP, ProviderKind::OLLAMA],
        collect: first_local,
    });
    all.into_iter()
        .filter(|choice| choice.kinds.iter().any(|kind| installed.contains(kind)))
        .collect()
}

/// `Anthropic, OpenAI, xAI, Gemini and 11 more`, from whatever is installed.
fn key_row_detail(rows: &[KeyProvider]) -> String {
    let lead: Vec<&str> = ["claude", "openai", "xai", "gemini"]
        .iter()
        .filter_map(|name| rows.iter().find(|row| row.name == *name))
        .map(|row| row.short)
        .collect();
    let more = rows.len().saturating_sub(lead.len());
    match (lead.is_empty(), more) {
        (true, _) => format!("{} providers", rows.len()),
        (false, 0) => lead.join(", "),
        (false, more) => format!("{} and {more} more", lead.join(", ")),
    }
}

/// What the local row will do: `Qwen3.6 35B, about 20 GB download,
/// llama.cpp`, or the file or Ollama model already here.
fn local_row_detail(plan: &LocalPlan) -> String {
    match plan {
        LocalPlan::LlamaCpp { gguf_path } => {
            let tier = Path::new(gguf_path)
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(hardware::gguf_tier_for_file);
            match tier {
                Some(tier) if Path::new(gguf_path).exists() => {
                    format!("{}, already downloaded, llama.cpp", tier.name)
                }
                Some(tier) => format!(
                    "{}, about {} GB download, llama.cpp",
                    tier.name, tier.approx_gb
                ),
                None => format!("{}, llama.cpp", gguf_model_tag(gguf_path)),
            }
        }
        LocalPlan::Ollama { model } => format!("{model} via Ollama"),
    }
}

/// The local plan for this machine, or `None` when no local backend is in
/// the build. Probed once per process (`nvidia-smi`, `rocm-smi`, `ollama
/// list`), and [`run`] asks before the terminal goes raw: a probe that hangs
/// on a blank alternate screen is a hang where Ctrl-C does nothing.
fn local_plan() -> Option<LocalPlan> {
    static PLAN: std::sync::OnceLock<Option<LocalPlan>> = std::sync::OnceLock::new();
    PLAN.get_or_init(|| {
        let (suggested, _) = hardware::suggest_gguf();
        let (suggested_tag, _) = hardware::suggest_model();
        let dir = models_dir();
        plan_local_auto(
            &crate::llm::registry::kinds(),
            &existing_ggufs(&dir),
            &dir,
            &installed_ollama_models(),
            suggested,
            &suggested_tag,
        )
    })
    .clone()
}

fn collect_first_run(terminal: &mut Tui, retry: Option<String>) -> Result<Option<FirstRunPick>> {
    let installed = crate::llm::registry::kinds();
    let choices = first_run_choices(&installed, local_plan().as_ref());
    if choices.is_empty() {
        anyhow::bail!(
            "this build has no provider backends compiled in, so there is nothing to \
             onboard to. Every backend is a plugin behind a cargo feature and all of \
             them are on by default; rebuild with the ones you want, or install a \
             stock release binary. See docs/plugins.md."
        );
    }
    if let Some(reason) = retry
        && let Some(pick) = first_api_key(terminal, Some(&reason))?
    {
        return Ok(Some(pick));
    }
    let options: Vec<Opt> = choices
        .iter()
        .map(|choice| Opt::new(choice.label, choice.detail.clone()))
        .collect();
    let mut selected = 0;
    loop {
        let index = match select(
            terminal,
            "How do you want to run Wizard?",
            "Change anything later with /setup.",
            &options,
            selected,
        )? {
            Some(index) => index,
            None => return Ok(None),
        };
        selected = index;
        // A row's own Esc comes back here, not out of the wizard.
        if let Some(pick) = (choices[index].collect)(terminal)? {
            return Ok(Some(pick));
        }
    }
}

fn first_xai_oauth(_terminal: &mut Tui) -> Result<Option<FirstRunPick>> {
    Ok(Some(FirstRunPick {
        answers: ProviderAnswers {
            provider_name: "xai".to_string(),
            kind: ProviderKind::XAI_OAUTH,
            base_url: XAI_BASE_URL.to_string(),
            model: XAI_MODELS[0].to_string(),
            api_key_env: None,
            api_key: None,
            gguf_path: None,
        },
        login: Some(Login::Xai),
        title: String::new(),
    }))
}

#[cfg(feature = "provider-chatgpt")]
fn first_chatgpt(_terminal: &mut Tui) -> Result<Option<FirstRunPick>> {
    let provider = crate::plugins::chatgpt::oauth::provider_config();
    Ok(Some(FirstRunPick {
        answers: ProviderAnswers {
            provider_name: provider.name,
            kind: provider.kind,
            base_url: provider.base_url,
            model: provider.model,
            api_key_env: None,
            api_key: None,
            gguf_path: None,
        },
        login: Some(Login::ChatGpt),
        title: String::new(),
    }))
}

/// The local row: say what is about to be downloaded and ask, then resolve
/// the plan. `Ok(None)` (Esc) goes back to the first screen.
fn first_local(terminal: &mut Tui) -> Result<Option<FirstRunPick>> {
    if let Some(LocalPlan::LlamaCpp { gguf_path }) = local_plan()
        && !Path::new(&gguf_path).exists()
    {
        let tier = Path::new(&gguf_path)
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(hardware::gguf_tier_for_file);
        let size = tier.map_or(String::new(), |tier| {
            format!(" (about {} GB)", tier.approx_gb)
        });
        let question = format!(
            "Downloads {}{size} to {}. Enter to start, Esc to go back.",
            gguf_model_tag(&gguf_path),
            tilde(&models_dir())
        );
        if !confirm(terminal, &question)? {
            return Ok(None);
        }
    }
    Ok(collect_local_auto(terminal)?.map(|answers| FirstRunPick {
        answers,
        login: None,
        title: String::new(),
    }))
}

/// One keyed cloud provider on the first run's compact list.
struct KeyProvider {
    label: &'static str,
    /// The word the first screen's row uses for it.
    short: &'static str,
    name: &'static str,
    kind: ProviderKind,
    base_url: &'static str,
    model: &'static str,
    key_env: &'static str,
}

/// OpenRouter's first-run default. Not the Auto Router: it can hand a
/// tool-calling turn to a model without tool support.
const OPENROUTER_FIRST_RUN_MODEL: &str = "anthropic/claude-sonnet-5";

/// The compact list: the keyed clouds, then every OpenAI-compatible preset.
/// Cloudflare's base URL is a template; `first_api_key` fills the account in.
fn key_providers(installed: &[ProviderKind]) -> Vec<KeyProvider> {
    let mut rows = vec![
        KeyProvider {
            label: "xAI (Grok)",
            short: "xAI",
            name: "xai",
            kind: ProviderKind::XAI,
            base_url: XAI_BASE_URL,
            model: XAI_MODELS[0],
            key_env: XAI_KEY_ENV,
        },
        KeyProvider {
            label: "Anthropic (Claude)",
            short: "Anthropic",
            name: "claude",
            kind: ProviderKind::ANTHROPIC,
            base_url: ANTHROPIC_BASE_URL,
            model: ANTHROPIC_MODELS[0],
            key_env: ANTHROPIC_KEY_ENV,
        },
        KeyProvider {
            label: "OpenAI",
            short: "OpenAI",
            name: "openai",
            kind: ProviderKind::OPENAI,
            base_url: OPENAI_BASE_URL,
            model: OPENAI_MODELS[0],
            key_env: OPENAI_KEY_ENV,
        },
        KeyProvider {
            label: "OpenRouter",
            short: "OpenRouter",
            name: "openrouter",
            kind: ProviderKind::OPENROUTER,
            base_url: OPENROUTER_BASE_URL,
            model: OPENROUTER_FIRST_RUN_MODEL,
            key_env: OPENROUTER_KEY_ENV,
        },
        KeyProvider {
            label: "Cloudflare Workers AI",
            short: "Cloudflare",
            name: "cloudflare",
            kind: ProviderKind::CLOUDFLARE,
            base_url: "",
            model: CLOUDFLARE_MODEL,
            key_env: CLOUDFLARE_KEY_ENV,
        },
    ];
    for preset in crate::llm::compat::PRESETS {
        rows.push(KeyProvider {
            label: preset.label,
            short: match preset.name {
                "gemini" => "Gemini",
                _ => preset.label.split(' ').next().unwrap_or(preset.label),
            },
            name: preset.name,
            kind: ProviderKind::OPENAI,
            base_url: preset.base_url,
            model: preset.default_model(),
            key_env: preset.key_env,
        });
    }
    rows.retain(|row| installed.contains(&row.kind));
    rows
}

/// The exported value of `name`, when it has one that is not blank.
fn exported(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// The first row whose key variable is already exported: that provider is
/// preselected, and its paste screen offers to keep the variable.
fn preselected_key_provider(exported: &[Option<String>]) -> Option<usize> {
    exported.iter().position(Option::is_some)
}

/// `sk-…ab12`: enough of a key to recognise it, never enough to use it. A
/// value too short to show either end of is described by its length, so
/// `(5 characters)` rather than a lone `…` that reads as a rendering bug.
fn key_glimpse(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        return format!("{} characters", chars.len());
    }
    let head: String = chars[..3].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

/// The answers for one keyed provider. A pasted key wins over an exported
/// variable: the config names an empty variable, which turns the lookup off
/// (the backend's default variable included) so the stale value in the
/// shell cannot shadow the key just typed.
fn key_answers(
    row: &KeyProvider,
    base_url: String,
    pasted: Option<String>,
    env_set: bool,
) -> ProviderAnswers {
    let api_key_env = if pasted.is_some() && env_set {
        Some(String::new())
    } else {
        Some(row.key_env.to_string())
    };
    ProviderAnswers {
        provider_name: row.name.to_string(),
        kind: row.kind.clone(),
        base_url,
        model: row.model.to_string(),
        api_key_env,
        api_key: pasted,
        gguf_path: None,
    }
}

fn first_api_key_row(terminal: &mut Tui) -> Result<Option<FirstRunPick>> {
    first_api_key(terminal, None)
}

/// The last row of the key list: any server that speaks the OpenAI wire
/// format (vLLM, LM Studio, llama-server, a proxy), asked for its URL, its
/// model id and a key it may not need.
const COMPAT_ROW: &str = "Another OpenAI-compatible endpoint";

/// Where such a server usually listens.
const COMPAT_DEFAULT_URL: &str = "http://127.0.0.1:8080/v1";

/// The key list, then the paste. `reason` is a rejected key's verdict, shown
/// over the list. `Ok(None)` (Esc) goes back to the first screen.
fn first_api_key(terminal: &mut Tui, reason: Option<&str>) -> Result<Option<FirstRunPick>> {
    let rows = key_providers(&crate::llm::registry::kinds());
    let env: Vec<Option<String>> = rows.iter().map(|row| exported(row.key_env)).collect();
    let preselected = preselected_key_provider(&env);
    let mut options: Vec<Opt> = rows
        .iter()
        .zip(&env)
        .map(|(row, value)| {
            let detail = if value.is_some() {
                format!("{} · ${} is set", row.model, row.key_env)
            } else {
                format!("{} · ${}", row.model, row.key_env)
            };
            Opt::new(row.label, detail)
        })
        .collect();
    let compat_row = rows
        .iter()
        .any(|row| row.kind == ProviderKind::OPENAI)
        .then_some(options.len());
    if compat_row.is_some() {
        options.push(Opt::new(
            COMPAT_ROW,
            "vLLM, LM Studio, llama-server, any /v1",
        ));
    }
    let index = match select(
        terminal,
        "Which provider?",
        reason.unwrap_or(""),
        &options,
        preselected.unwrap_or(0),
    )? {
        Some(index) => index,
        None => return Ok(None),
    };
    if Some(index) == compat_row {
        return first_compat(terminal);
    }
    let row = &rows[index];
    let env_value = env[index].as_deref();

    let base_url = if row.kind == ProviderKind::CLOUDFLARE {
        let account_id = match text_input(
            terminal,
            "Cloudflare account ID",
            "Dashboard → Workers AI (or `wrangler whoami`).",
            "",
        )? {
            Some(value) => value,
            None => return Ok(None),
        };
        crate::llm::registry::defaults::cloudflare_base_url(&account_id)
    } else {
        row.base_url.to_string()
    };

    let subtitle = match env_value {
        Some(value) => format!(
            "${} is set ({}): enter keeps it, paste to replace it.",
            row.key_env,
            key_glimpse(value)
        ),
        None => format!(
            "Stored in ~/.wizard/credentials.toml (0600); empty uses ${}.",
            row.key_env
        ),
    };
    let title = format!("{} API key", row.label);
    let pasted = match secret_input(terminal, &title, &subtitle)? {
        Some(value) => Some(value.trim().to_string()).filter(|key| !key.is_empty()),
        None => return Ok(None),
    };

    Ok(Some(FirstRunPick {
        answers: key_answers(row, base_url, pasted, env_value.is_some()),
        login: None,
        title,
    }))
}

/// The compat row: base URL, model id, then a key the server may not want
/// (enter skips it). The same one-request check runs against that URL; a
/// 200 from `/models` passes, key or no key. Esc at any step goes back to
/// the list.
fn first_compat(terminal: &mut Tui) -> Result<Option<FirstRunPick>> {
    let base_url = match text_input(
        terminal,
        "Base URL",
        "The server's /v1 root.",
        COMPAT_DEFAULT_URL,
    )? {
        Some(value) => value.trim_end_matches('/').to_string(),
        None => return Ok(None),
    };
    let model = loop {
        match text_input(
            terminal,
            "Model id",
            "As the server lists it under /models.",
            "",
        )? {
            Some(value) if !value.trim().is_empty() => break value.trim().to_string(),
            Some(_) => continue,
            None => return Ok(None),
        }
    };
    let title = "Endpoint API key".to_string();
    let pasted = match secret_input(
        terminal,
        &title,
        "Enter to skip if the server takes none. Stored in ~/.wizard/credentials.toml (0600).",
    )? {
        Some(value) => Some(value.trim().to_string()).filter(|key| !key.is_empty()),
        None => return Ok(None),
    };
    Ok(Some(FirstRunPick {
        answers: ProviderAnswers {
            provider_name: "custom".to_string(),
            kind: ProviderKind::OPENAI,
            base_url,
            model,
            api_key_env: None,
            api_key: pasted,
            gguf_path: None,
        },
        login: None,
        title,
    }))
}

/// Check the credential, then save. The pasted key is staged in memory for
/// the check and written only once the provider has not refused it, so a
/// rejected key never reaches the disk and a write that fails is reported
/// as that. The config is saved before a sign-in, so an abandoned one leaves
/// `wizard --login` as the only step left.
async fn finish_first_run(screen: &mut Screen, pick: FirstRunPick) -> Result<Finished> {
    let FirstRunPick {
        answers,
        login,
        title,
    } = pick;
    let answers = Answers::first_run(answers);
    let name = answers.provider_name.clone();
    let pasted = answers.provider_api_key.clone();
    let config = answers.into_config();
    let active = config.active();
    let mut notice = None;
    let exported = pasted.is_none() && !active.api_key().is_empty();
    match credential_plan(
        &active.credentials(),
        active.key_env_name().as_deref(),
        pasted.is_some(),
        exported,
        login.is_some(),
    ) {
        Plan::Skip => {}
        Plan::NoKey(var) => {
            notice = Some(format!(
                "no key for {name}: export {var} and start again, or paste one with /setup"
            ));
        }
        Plan::Check => {
            if let Some(key) = &pasted {
                crate::credentials::stage(&name, key);
            }
            let shown = match (&pasted, active.key_env_name()) {
                (Some(key), _) => widgets::masked(key),
                (None, Some(var)) => format!("${var}"),
                (None, None) => "no key".to_string(),
            };
            let verdict = check_credential(screen, &config, &title, &shown).await;
            crate::credentials::unstage(&name);
            match verdict {
                Check::Ok => {}
                Check::Rejected(reason) => return Ok(Finished::Rejected(reason)),
                Check::Unreachable(reason) => notice = Some(reason),
            }
        }
    }
    if let Some(key) = &pasted {
        crate::credentials::store(&name, key).context("saving the API key")?;
    }
    config.save().context("saving config from onboarding")?;
    Ok(Finished::Saved {
        config: Box::new(config),
        notice,
        login,
    })
}

/// The browser sign-in, on the plain terminal.
async fn sign_in(login: Login) -> Result<()> {
    let paste = if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        crate::llm::oauth_callback::PasteChannel::Stdin
    } else {
        crate::llm::oauth_callback::PasteChannel::Disabled
    };
    let report = |line: &str| println!("{line}");
    let (name, outcome) = match login {
        Login::Xai => (
            "xai",
            crate::llm::xai_oauth::login(report, paste, false).await,
        ),
        #[cfg(feature = "provider-chatgpt")]
        Login::ChatGpt => (
            "chatgpt",
            crate::plugins::chatgpt::oauth::login(report, paste).await,
        ),
    };
    outcome.with_context(|| {
        format!(
            "sign-in failed; the config is saved, so run `wizard --login {name}` and \
             then `wizard`"
        )
    })
}

/// What the first run does about the credential before it saves.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    /// One request against the provider, with the pasted key, the exported
    /// variable, or (a server that wants none) no key at all.
    Check,
    /// Nothing to check: no key pasted and the named variable is not
    /// exported. Saved as is, with the card told which variable to set.
    NoKey(String),
    /// Not a keyed backend, or a sign-in decides it.
    Skip,
}

/// Decided from what the backend needs, never from which config field the
/// key happened to land in: a key pasted over an exported variable used to
/// clear `api_key_env`, and the check keyed off that field, so the one paste
/// most likely to be wrong was the one never checked.
fn credential_plan(
    credentials: &Credentials,
    key_env: Option<&str>,
    pasted: bool,
    exported: bool,
    login: bool,
) -> Plan {
    if login || !matches!(credentials, Credentials::ApiKey { .. }) {
        return Plan::Skip;
    }
    match (pasted || exported, key_env) {
        (true, _) | (false, None) => Plan::Check,
        (false, Some(var)) => Plan::NoKey(format!("${var}")),
    }
}

/// The verdict of one request against the provider just configured.
enum Check {
    Ok,
    /// The provider answered and refused the credential; the text is the
    /// key list's subtitle.
    Rejected(String),
    /// No verdict: a transport failure or no answer in time. Not worth
    /// holding the user at the key screen for; the card says so.
    Unreachable(String),
}

/// How long the first-run credential check waits for an answer.
const CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// How often the checking screen's spinner turns.
const CHECK_FRAME: std::time::Duration = std::time::Duration::from_millis(80);

/// One request to the active provider, so a bad paste or a stale shell
/// variable is caught here and not by the first turn. The paste screen stays
/// up meanwhile, saying which host it is waiting on.
async fn check_credential(screen: &mut Screen, config: &Config, title: &str, shown: &str) -> Check {
    let provider = config.active();
    let host = url_host(&provider.base_url);
    let client = match provider.build() {
        Ok(client) => client,
        Err(err) => return Check::Unreachable(format!("could not build the provider: {err:#}")),
    };
    let check = tokio::time::timeout(CHECK_TIMEOUT, client.health());
    tokio::pin!(check);
    let mut tick = 0u64;
    let started = std::time::Instant::now();
    let outcome = loop {
        let _ = screen
            .terminal
            .draw(|frame| widgets::draw_checking(frame, title, &host, shown, tick));
        tokio::select! {
            outcome = &mut check => break outcome,
            _ = tokio::time::sleep(CHECK_FRAME) => tick += 1,
        }
    };
    let verdict = match outcome {
        Ok(Ok(())) => Check::Ok,
        Ok(Err(err)) => rejection(&host, &err),
        Err(_) => Check::Unreachable(format!(
            "{host} did not answer in {} seconds; the key is saved unchecked",
            CHECK_TIMEOUT.as_secs()
        )),
    };
    tracing::info!(
        "first-run credential check against {host}: {} in {:.3}s",
        match &verdict {
            Check::Ok => "ok",
            Check::Rejected(_) => "rejected",
            Check::Unreachable(_) => "no verdict",
        },
        started.elapsed().as_secs_f64()
    );
    verdict
}

/// Plain words for a failed check: the status, never the response body.
fn rejection(host: &str, err: &anyhow::Error) -> Check {
    let status = err
        .downcast_ref::<crate::llm::ProviderError>()
        .and_then(|provider| provider.status);
    // Cloudflare answers 403 to a wrong account id as readily as to a bad
    // token, so its line names both.
    let what = if host.contains("cloudflare") {
        "that token or account id"
    } else {
        "that key"
    };
    match status {
        Some(code @ (401 | 403)) => Check::Rejected(format!(
            "{host} rejected {what} ({code}): paste another, or esc."
        )),
        Some(code) => Check::Unreachable(format!(
            "{host} answered HTTP {code}; the key is saved unchecked"
        )),
        None => Check::Unreachable(format!("cannot reach {host}; the key is saved unchecked")),
    }
}

/// `api.anthropic.com` out of `https://api.anthropic.com/v1`.
fn url_host(url: &str) -> String {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or(url)
        .to_string()
}

/// One row of the provider menu.
///
/// The row exists in the source; whether it is *offered* is a question for
/// [`crate::llm::registry`]. That split is the whole of what this type is for:
/// every backend is a plugin behind a cargo feature now, so a menu written out
/// as eleven literals offered whatever it was written with, and a build
/// without `provider-anthropic` still had a row that produced a config failing
/// at `build()` — an entry that should never have been on the screen.
struct ProviderChoice<T = ProviderAnswers> {
    label: &'static str,
    detail: String,
    /// The kinds this row can produce. Offered when *any* of them is
    /// registered, because "Local" resolves to llama.cpp or to Ollama
    /// depending on what is already on the machine and either one alone is
    /// enough to make the row work.
    kinds: Vec<ProviderKind>,
    /// The questions this row asks next. A function pointer rather than an
    /// index into a `match`, so a row that is filtered out cannot shift the
    /// meaning of the ones after it — which is what an index-dispatched menu
    /// does the first time it is filtered.
    collect: fn(&mut Tui) -> Result<Option<T>>,
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

/// `path` with the home directory written as `~`.
fn tilde(path: &Path) -> String {
    let shown = path.display().to_string();
    match dirs::home_dir().map(|home| home.display().to_string()) {
        Some(home) if shown.starts_with(&home) => format!("~{}", &shown[home.len()..]),
        _ => shown,
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
///
/// Each step is skipped when this build has no plugin for the backend it
/// would pick. Both are on by default, so a stock build takes every step and
/// `installed` changes nothing — but a `--features provider-ollama` build has
/// no llama.cpp to fall back to, and steps 1 and 3 would otherwise hand back
/// a config that fails at `build()`. That is the same entry-that-was-never-
/// offered this menu was rewritten to stop producing, one level down: the
/// "Local" row is offered when *either* backend is present, so the row's own
/// resolution has to respect which one that was.
///
/// [`None`] when neither is installed. Unreachable from the menu, which does
/// not offer the row at all in that case, and returned rather than asserted
/// because a caller that is wrong about the plugin set should get an answer
/// it can report instead of a panic.
pub fn plan_local_auto(
    installed: &[ProviderKind],
    existing: &[PathBuf],
    models_dir: &Path,
    ollama_models: &[String],
    suggested: &GgufModel,
    suggested_tag: &str,
) -> Option<LocalPlan> {
    let llamacpp = installed.contains(&ProviderKind::LLAMACPP);
    let ollama = installed.contains(&ProviderKind::OLLAMA);

    if llamacpp && !existing.is_empty() {
        let chosen = existing
            .iter()
            .find(|path| path.file_name().is_some_and(|name| name == suggested.file))
            .unwrap_or(&existing[0]);
        return Some(LocalPlan::LlamaCpp {
            gguf_path: chosen.display().to_string(),
        });
    }
    if ollama && !ollama_models.is_empty() {
        let model = ollama_models
            .iter()
            .find(|model| model.as_str() == suggested_tag)
            .unwrap_or(&ollama_models[0]);
        return Some(LocalPlan::Ollama {
            model: model.clone(),
        });
    }
    if llamacpp {
        return Some(LocalPlan::LlamaCpp {
            gguf_path: models_dir.join(suggested.file).display().to_string(),
        });
    }
    // Ollama is installed but has nothing pulled: fall through to it anyway
    // with the hardware-suggested tag, which `collect_ollama` would have
    // offered and which is pulled on first run.
    ollama.then(|| LocalPlan::Ollama {
        model: suggested_tag.to_string(),
    })
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
    let plan = plan_local_auto(
        &crate::llm::registry::kinds(),
        &existing,
        &dir,
        &installed_ollama_models(),
        suggested,
        &suggested_tag,
    )
    .context("the one-click local pick needs `provider-llamacpp` or `provider-ollama`")?;
    let answers = match plan {
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

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn base_answers() -> Answers {
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
            skin: Some(Skin::Wizard),
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

    pub(super) fn tier(file: &'static str) -> GgufModel {
        GgufModel {
            name: "Test",
            file,
            url: "https://example.com/x.gguf",
            approx_gb: 1,
        }
    }

    /// Both local backends compiled in, which is what a stock build has and
    /// what every plan test below except the last three assumes.
    pub(super) fn both_local() -> Vec<ProviderKind> {
        vec![ProviderKind::LLAMACPP, ProviderKind::OLLAMA]
    }

    #[test]
    fn local_plan_prefers_a_downloaded_gguf() {
        let dir = Path::new("/m");
        let existing = vec![PathBuf::from("/m/a.gguf"), PathBuf::from("/m/big.gguf")];
        // The suggested tier is on disk: it wins over the first-by-name file.
        let plan = plan_local_auto(
            &both_local(),
            &existing,
            dir,
            &["qwen3.5:9b".to_string()],
            &tier("big.gguf"),
            "qwen3.5:9b",
        );
        assert_eq!(
            plan,
            Some(LocalPlan::LlamaCpp {
                gguf_path: "/m/big.gguf".to_string()
            })
        );
        // Suggested tier not on disk: first existing GGUF wins, still no
        // download and still ahead of any Ollama install.
        let plan = plan_local_auto(
            &both_local(),
            &existing,
            dir,
            &["qwen3.5:9b".to_string()],
            &tier("other.gguf"),
            "qwen3.5:9b",
        );
        assert_eq!(
            plan,
            Some(LocalPlan::LlamaCpp {
                gguf_path: "/m/a.gguf".to_string()
            })
        );
    }

    #[test]
    fn local_plan_reuses_an_ollama_install_with_models() {
        let dir = Path::new("/m");
        let pulled = vec!["llama3:8b".to_string(), "qwen3.5:9b".to_string()];
        // The hardware-suggested tag is pulled: use it.
        let plan = plan_local_auto(
            &both_local(),
            &[],
            dir,
            &pulled,
            &tier("x.gguf"),
            "qwen3.5:9b",
        );
        assert_eq!(
            plan,
            Some(LocalPlan::Ollama {
                model: "qwen3.5:9b".to_string()
            })
        );
        // Suggested tag not pulled: first listed model.
        let plan = plan_local_auto(
            &both_local(),
            &[],
            dir,
            &pulled,
            &tier("x.gguf"),
            "qwen3.6:27b",
        );
        assert_eq!(
            plan,
            Some(LocalPlan::Ollama {
                model: "llama3:8b".to_string()
            })
        );
    }

    #[test]
    fn local_plan_falls_back_to_a_fresh_llamacpp_download() {
        let plan = plan_local_auto(
            &both_local(),
            &[],
            Path::new("/m"),
            &[],
            &tier("big.gguf"),
            "qwen3.5:9b",
        );
        assert_eq!(
            plan,
            Some(LocalPlan::LlamaCpp {
                gguf_path: "/m/big.gguf".to_string()
            })
        );
    }

    /// The one-click local pick never resolves to a backend this build left
    /// out, whatever is lying around on the machine.
    ///
    /// Both directions, because both are wrong in the same way and only one
    /// of them is obvious: a GGUF already downloaded is the *strongest*
    /// signal in the whole function, and it still must not produce
    /// `kind = "llamacpp"` on a build that cannot serve one. The row is
    /// offered because Ollama is present, so what it resolves to has to be
    /// Ollama.
    #[test]
    fn the_one_click_local_pick_skips_a_backend_this_build_lacks() {
        let dir = Path::new("/m");
        let gguf = vec![PathBuf::from("/m/a.gguf")];
        let pulled = vec!["llama3:8b".to_string()];

        let ollama_only = vec![ProviderKind::OLLAMA];
        assert_eq!(
            plan_local_auto(
                &ollama_only,
                &gguf,
                dir,
                &pulled,
                &tier("x.gguf"),
                "qwen3.5:9b"
            ),
            Some(LocalPlan::Ollama {
                model: "llama3:8b".to_string()
            }),
            "a downloaded GGUF must not win on a build with no llama.cpp"
        );
        // Nothing pulled either: still Ollama, with the tag `collect_ollama`
        // would have offered, because there is no other backend to fall to.
        assert_eq!(
            plan_local_auto(&ollama_only, &[], dir, &[], &tier("x.gguf"), "qwen3.5:9b"),
            Some(LocalPlan::Ollama {
                model: "qwen3.5:9b".to_string()
            })
        );

        let llamacpp_only = vec![ProviderKind::LLAMACPP];
        assert_eq!(
            plan_local_auto(
                &llamacpp_only,
                &[],
                dir,
                &pulled,
                &tier("big.gguf"),
                "llama3:8b"
            ),
            Some(LocalPlan::LlamaCpp {
                gguf_path: "/m/big.gguf".to_string()
            }),
            "a pulled Ollama model must not win on a build with no Ollama"
        );

        assert_eq!(
            plan_local_auto(&[], &gguf, dir, &pulled, &tier("x.gguf"), "llama3:8b"),
            None,
            "neither backend compiled in leaves nothing to plan"
        );
    }

    /// Every kind a stock build ships, whether or not this one does. Used to
    /// pin the menu's display order independently of the feature set the test
    /// happens to be running under.
    pub(super) fn all_shipped_kinds() -> Vec<ProviderKind> {
        vec![
            ProviderKind::ANTHROPIC,
            ProviderKind::CHATGPT_OAUTH,
            ProviderKind::CLOUDFLARE,
            ProviderKind::LLAMACPP,
            ProviderKind::OLLAMA,
            ProviderKind::OPENAI,
            ProviderKind::OPENROUTER,
            ProviderKind::XAI,
            ProviderKind::XAI_OAUTH,
        ]
    }

    #[test]
    fn a_first_run_answers_only_the_provider() {
        let config = Answers::first_run(ProviderAnswers {
            provider_name: "xai".to_string(),
            kind: ProviderKind::XAI_OAUTH,
            base_url: XAI_BASE_URL.to_string(),
            model: XAI_MODELS[0].to_string(),
            api_key_env: None,
            api_key: None,
            gguf_path: None,
        })
        .into_config();
        assert_eq!(config.active_provider.as_deref(), Some("xai"));
        assert_eq!(config.active().kind, ProviderKind::XAI_OAUTH);
        assert_eq!(config.mode, Mode::Genie);
        assert_eq!(config.gateway.kind, GatewayKind::None);
        assert_eq!(config.web.search_backend, "duckduckgo");
        // Never asked, so never written: `WIZARD_SKIN` keeps working.
        assert_eq!(config.ui.skin, None);
    }

    #[test]
    fn the_first_screen_offers_only_what_this_build_installed() {
        assert!(first_run_choices(&[], None).is_empty());
        let labels = |kinds: &[ProviderKind]| -> Vec<&str> {
            first_run_choices(kinds, None)
                .iter()
                .map(|choice| choice.label)
                .collect()
        };
        assert_eq!(labels(&[ProviderKind::XAI_OAUTH]), ["Sign in with xAI"]);
        assert_eq!(labels(&[ProviderKind::ANTHROPIC]), ["Paste an API key"]);
        assert_eq!(labels(&[ProviderKind::OLLAMA]), ["Run a model locally"]);
        let mut expected = vec!["Sign in with xAI"];
        if cfg!(feature = "provider-chatgpt") {
            expected.push("Sign in with ChatGPT");
        }
        expected.extend(["Paste an API key", "Run a model locally"]);
        assert_eq!(labels(&all_shipped_kinds()), expected);
    }

    /// The label on a sign-in row names the model that lands in config.toml.
    #[test]
    fn sign_in_rows_name_the_model_they_configure() {
        let choices = first_run_choices(&all_shipped_kinds(), None);
        let xai = choices
            .iter()
            .find(|choice| choice.label == "Sign in with xAI")
            .expect("xai row");
        assert!(xai.detail.starts_with(XAI_MODELS[0]), "{}", xai.detail);
        #[cfg(feature = "provider-chatgpt")]
        {
            let chatgpt = choices
                .iter()
                .find(|choice| choice.label == "Sign in with ChatGPT")
                .expect("chatgpt row");
            let model = crate::plugins::chatgpt::oauth::provider_config().model;
            assert!(chatgpt.detail.starts_with(&model), "{}", chatgpt.detail);
        }
    }

    #[test]
    fn the_key_row_counts_what_it_does_not_name() {
        let rows = key_providers(&all_shipped_kinds());
        let detail = key_row_detail(&rows);
        assert!(
            detail.starts_with("Anthropic, OpenAI, xAI, Gemini and "),
            "{detail}"
        );
        assert!(
            detail.ends_with(&format!("{} more", rows.len() - 4)),
            "{detail}"
        );
        let only_openai = key_providers(&[ProviderKind::OPENAI]);
        let detail = key_row_detail(&only_openai);
        assert!(detail.starts_with("OpenAI, Gemini and "), "{detail}");
        assert_eq!(
            key_row_detail(&key_providers(&[ProviderKind::ANTHROPIC])),
            "Anthropic"
        );
    }

    #[test]
    fn the_local_row_says_what_it_will_download() {
        let missing = LocalPlan::LlamaCpp {
            gguf_path: "/nowhere/Qwen3.6-35B-A3B-UD-Q4_K_M.gguf".to_string(),
        };
        assert_eq!(
            local_row_detail(&missing),
            "Qwen3.6 35B, about 20 GB download, llama.cpp"
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let present = dir.path().join("Qwen3.5-9B-Q4_K_M.gguf");
        std::fs::write(&present, b"gguf").unwrap();
        assert_eq!(
            local_row_detail(&LocalPlan::LlamaCpp {
                gguf_path: present.display().to_string()
            }),
            "Qwen3.5 9B, already downloaded, llama.cpp"
        );
        assert_eq!(
            local_row_detail(&LocalPlan::Ollama {
                model: "qwen3.5:9b".to_string()
            }),
            "qwen3.5:9b via Ollama"
        );
    }

    #[test]
    fn the_key_list_leads_with_xai_and_carries_every_preset() {
        let rows = key_providers(&all_shipped_kinds());
        assert_eq!(rows[0].name, "xai");
        assert_eq!(rows[0].model, XAI_MODELS[0]);
        for preset in crate::llm::compat::PRESETS {
            assert!(
                rows.iter().any(|row| row.name == preset.name),
                "{} is missing",
                preset.name
            );
        }
        // Only what the build can reach.
        let openai_only = key_providers(&[ProviderKind::OPENAI]);
        assert!(
            openai_only
                .iter()
                .all(|row| row.kind == ProviderKind::OPENAI)
        );
        assert!(openai_only.iter().any(|row| row.name == "gemini"));
        assert!(openai_only.iter().all(|row| row.name != "xai"));
        // OpenRouter is not left on the Auto Router, which may pick a model
        // that cannot call tools.
        let openrouter = rows.iter().find(|row| row.name == "openrouter").unwrap();
        assert_eq!(openrouter.model, OPENROUTER_FIRST_RUN_MODEL);
        assert_ne!(openrouter.model, "openrouter/auto");
    }

    #[test]
    fn an_exported_key_variable_preselects_its_provider() {
        let rows = key_providers(&all_shipped_kinds());
        let env = |set: &[&str]| -> Vec<Option<String>> {
            rows.iter()
                .map(|row| set.contains(&row.key_env).then(|| "sk-x".to_string()))
                .collect()
        };
        let anthropic = rows.iter().position(|row| row.name == "claude").unwrap();
        assert_eq!(
            preselected_key_provider(&env(&["ANTHROPIC_API_KEY"])),
            Some(anthropic)
        );
        assert_eq!(preselected_key_provider(&env(&[])), None);
        // The first exported one wins, in list order.
        assert_eq!(
            preselected_key_provider(&env(&["XAI_API_KEY", "OPENAI_API_KEY"])),
            Some(0)
        );
    }

    /// A pasted key must be the key that is sent. `resolved_key` reads the
    /// env var first, so a stale export would shadow the paste unless the
    /// variable is dropped from the config.
    #[test]
    fn a_pasted_key_beats_an_exported_variable() {
        let rows = key_providers(&all_shipped_kinds());
        let openai = rows.iter().find(|row| row.name == "openai").unwrap();
        let kept = key_answers(openai, OPENAI_BASE_URL.to_string(), None, true);
        assert_eq!(kept.api_key_env.as_deref(), Some("OPENAI_API_KEY"));
        assert_eq!(kept.api_key, None);

        let pasted = key_answers(
            openai,
            OPENAI_BASE_URL.to_string(),
            Some("sk-new".to_string()),
            true,
        );
        assert_eq!(
            pasted.api_key_env.as_deref(),
            Some(""),
            "an empty name turns the variable off, default included"
        );
        assert_eq!(pasted.api_key.as_deref(), Some("sk-new"));
        // Through the resolver, for a backend with a default variable: the
        // config it produces must not let the export back in.
        let xai = rows.iter().find(|row| row.name == "xai").unwrap();
        let config = Answers::first_run(key_answers(
            xai,
            XAI_BASE_URL.to_string(),
            Some("xai-new".to_string()),
            true,
        ))
        .into_config();
        let active = config.active();
        assert_eq!(active.api_key_env.as_deref(), Some(""));
        assert_eq!(active.key_env_name(), None, "no variable, not the default");

        // With nothing exported, the variable stays as the documented override.
        let fresh = key_answers(
            openai,
            OPENAI_BASE_URL.to_string(),
            Some("sk-new".to_string()),
            false,
        );
        assert_eq!(fresh.api_key_env.as_deref(), Some("OPENAI_API_KEY"));
    }

    #[test]
    fn a_rejected_key_is_reported_in_plain_words_never_the_body() {
        let body = r#"{"type":"error","error":{"message":"API key is invalid."}}"#;
        let err = anyhow::Error::new(crate::llm::ProviderError::http(
            401,
            format!("https://api.anthropic.com returned HTTP 401: {body}"),
        ));
        match rejection("api.anthropic.com", &err) {
            Check::Rejected(reason) => {
                assert_eq!(
                    reason,
                    "api.anthropic.com rejected that key (401): paste another, or esc."
                );
            }
            _ => panic!("a 401 is a rejection"),
        }
        let forbidden = anyhow::Error::new(crate::llm::ProviderError::http(403, "nope"));
        match rejection("api.cloudflare.com", &forbidden) {
            Check::Rejected(reason) => assert_eq!(
                reason,
                "api.cloudflare.com rejected that token or account id (403): paste another, or esc."
            ),
            _ => panic!("a 403 is a rejection"),
        }
        let transport = anyhow::anyhow!("connection refused");
        assert!(matches!(
            rejection("api.openai.com", &transport),
            Check::Unreachable(_)
        ));
        let server = anyhow::Error::new(crate::llm::ProviderError::http(500, "boom"));
        assert!(matches!(
            rejection("api.openai.com", &server),
            Check::Unreachable(_)
        ));
    }

    #[test]
    fn hosts_and_glimpses_are_cut_the_same_way_every_time() {
        assert_eq!(url_host("https://api.anthropic.com"), "api.anthropic.com");
        assert_eq!(url_host("https://api.openai.com/v1"), "api.openai.com");
        assert_eq!(url_host("localhost:8000/v1"), "localhost:8000");
        assert_eq!(key_glimpse("sk-abcdefgh1234"), "sk-…1234");
        assert_eq!(key_glimpse("dummy"), "5 characters");
    }

    /// The check keys off what the backend needs, so a key pasted over an
    /// exported variable (the paste most likely to be wrong) is checked
    /// too; nothing pasted and nothing exported skips it and names the
    /// variable; a server that takes no key is checked keyless.
    #[test]
    fn the_credential_check_runs_in_every_keyed_path() {
        let keyed = Credentials::ApiKey {
            default_env: Some("XAI_API_KEY".to_string()),
        };
        // Pasted over an export: `api_key_env` is empty, the check still runs.
        assert_eq!(
            credential_plan(&keyed, None, true, false, false),
            Plan::Check
        );
        // Kept the export.
        assert_eq!(
            credential_plan(&keyed, Some("XAI_API_KEY"), false, true, false),
            Plan::Check
        );
        // Nothing pasted, nothing exported: no request, the card names it.
        assert_eq!(
            credential_plan(&keyed, Some("XAI_API_KEY"), false, false, false),
            Plan::NoKey("$XAI_API_KEY".to_string())
        );
        // A compat endpoint with no variable and no key: checked keyless.
        let bare = Credentials::ApiKey { default_env: None };
        assert_eq!(
            credential_plan(&bare, None, false, false, false),
            Plan::Check
        );
        // A sign-in or a local backend has nothing to check here.
        assert_eq!(
            credential_plan(&keyed, None, false, false, true),
            Plan::Skip
        );
        assert_eq!(
            credential_plan(&Credentials::Local, None, false, false, false),
            Plan::Skip
        );
    }

    /// A key is staged for the check and written only after it: a rejected
    /// paste leaves the store as it was.
    #[test]
    fn a_rejected_key_is_never_written() {
        let name = "first-run-rejected-test";
        crate::credentials::stage(name, "sk-bad");
        assert_eq!(crate::credentials::get(name).as_deref(), Some("sk-bad"));
        crate::credentials::unstage(name);
        assert_eq!(crate::credentials::get(name), None);
    }
}
