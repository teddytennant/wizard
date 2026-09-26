//! What an ACP client can choose per session: the model (every configured
//! provider, with the models it can run), the reasoning effort, and the mode.
//!
//! A model is named `<provider>/<model>`, where `<provider>` is the `name` of
//! a `[[providers]]` entry in `~/.wizard/config.toml` — or `local` when none
//! is configured and Wizard falls back to its llama.cpp default. Provider
//! names never contain a `/`, so the id splits at its first one and the model
//! part keeps any slashes of its own (`openrouter/anthropic/claude-sonnet-5`).
//!
//! A choice applies to its session only. The client's pick is recorded in a
//! [`Selection`] and the session's agent is built from a copy of the config
//! with that selection applied, so a session running a different model never
//! rewrites `active_provider` for the TUI or any other session.
//!
//! The model list comes from each provider's own `list_models`, fetched off
//! the request path and cached in `~/.wizard/cache/acp-models.json`, so a
//! `session/new` answers without waiting on the network once the cache is
//! warm. A provider's configured model is always offered, cached or not.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption,
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{Config, Mode, ProviderConfig, ReasoningEffort};

/// Config option id for the model select (category `model`).
pub(super) const MODEL_OPTION: &str = "model";
/// Config option id for the reasoning-effort select (category `thought_level`).
pub(super) const EFFORT_OPTION: &str = "thought_level";
/// Config option id for Wizard's genie/sovereign mode. Deliberately not
/// category `mode`: ACP clients read that one as a permission mode, and
/// Wizard never asks for permission in either.
pub(super) const MODE_OPTION: &str = "wizard_mode";

/// The effort value that leaves the provider's default in place.
const EFFORT_DEFAULT: &str = "default";

/// How long a cached model list is trusted before a background refresh.
const CATALOG_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// Bound on one provider's `list_models` during a refresh.
const LIST_TIMEOUT: Duration = Duration::from_secs(8);

/// Most models offered per provider, so an aggregator's catalog of hundreds
/// does not bury the configured ones.
const MODELS_PER_PROVIDER: usize = 60;

/// What one session runs: a provider and model, an effort, and a mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Selection {
    pub provider: String,
    pub model: String,
    pub effort: Option<ReasoningEffort>,
    pub mode: Mode,
}

impl Selection {
    /// The config's own defaults: the active provider and its model.
    pub fn from_config(config: &Config) -> Self {
        let active = config.active();
        Self {
            provider: active.name,
            model: active.model,
            effort: config.reasoning_effort,
            mode: config.mode,
        }
    }

    /// `<provider>/<model>`, the id the model select carries.
    pub fn model_id(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }

    /// A copy of `config` that builds an agent for this selection.
    pub fn apply(&self, config: &Config) -> Config {
        let mut config = config.clone();
        config.reasoning_effort = self.effort;
        config.mode = self.mode;
        match config
            .providers
            .iter_mut()
            .find(|provider| provider.name == self.provider)
        {
            Some(provider) => {
                provider.model = self.model.clone();
                config.active_provider = Some(self.provider.clone());
            }
            // No providers configured: `Config::active` synthesizes the local
            // default from the legacy `model` field.
            None => config.model = self.model.clone(),
        }
        config
    }
}

/// The providers a session can pick from: every configured one, or the
/// synthesized local default when there are none.
fn providers(config: &Config) -> Vec<ProviderConfig> {
    if config.providers.is_empty() {
        vec![config.active()]
    } else {
        config.providers.clone()
    }
}

/// Split a `<provider>/<model>` id against the configured providers. `None`
/// when it names no provider this config has, or no model.
pub(super) fn parse_model_id(id: &str, config: &Config) -> Option<(String, String)> {
    let (provider, model) = id.split_once('/')?;
    if model.trim().is_empty() {
        return None;
    }
    providers(config)
        .iter()
        .any(|p| p.name == provider)
        .then(|| (provider.to_string(), model.to_string()))
}

/// Parse an effort select value: `Some(None)` is the provider default.
pub(super) fn parse_effort(value: &str) -> Option<Option<ReasoningEffort>> {
    Some(match value {
        EFFORT_DEFAULT => None,
        "low" => Some(ReasoningEffort::Low),
        "medium" => Some(ReasoningEffort::Medium),
        "high" => Some(ReasoningEffort::High),
        "xhigh" => Some(ReasoningEffort::Xhigh),
        _ => return None,
    })
}

/// Parse a mode select value.
pub(super) fn parse_mode(value: &str) -> Option<Mode> {
    match value {
        "genie" => Some(Mode::Genie),
        "sovereign" => Some(Mode::Sovereign),
        _ => None,
    }
}

fn mode_value(mode: Mode) -> &'static str {
    match mode {
        Mode::Genie => "genie",
        Mode::Sovereign => "sovereign",
    }
}

/// Whether a tag from a provider's model list is something the agent can chat
/// with. `/models` endpoints also list image, video, speech, and embedding
/// models, which would only fail the first turn.
pub(super) fn is_chat_model(tag: &str) -> bool {
    const NOT_CHAT: [&str; 12] = [
        "image",
        "imagine",
        "video",
        "embed",
        "tts",
        "whisper",
        "audio",
        "realtime",
        "transcribe",
        "moderation",
        "dall-e",
        "speech",
    ];
    let tag = tag.trim().to_ascii_lowercase();
    !tag.is_empty() && !NOT_CHAT.iter().any(|word| tag.contains(word))
}

/// Bumped when a stored list's meaning changes (2: lists are newest first),
/// so an older cache is refetched instead of shown.
const CATALOG_VERSION: u32 = 2;

/// Model tags per provider, as the providers themselves listed them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(super) struct Catalog {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    providers: BTreeMap<String, CatalogEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct CatalogEntry {
    /// `<kind>|<base_url>`: a provider renamed onto another backend is
    /// refetched rather than offered the old backend's models.
    fingerprint: String,
    /// Unix seconds of the fetch.
    fetched_at: u64,
    models: Vec<String>,
}

fn fingerprint(provider: &ProviderConfig) -> String {
    format!("{}|{}", provider.kind.as_str(), provider.base_url)
}

pub(super) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

impl Catalog {
    /// `~/.wizard/cache/acp-models.json`.
    pub fn path() -> Result<PathBuf> {
        Ok(Config::wizard_dir()?.join("cache").join("acp-models.json"))
    }

    /// The cached catalog, or an empty one when there is none or it is
    /// unreadable (a cache is never worth failing a session over).
    pub fn load() -> Self {
        Self::path()
            .ok()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str::<Self>(&text).ok())
            .filter(|catalog| catalog.version == CATALOG_VERSION)
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let text = serde_json::to_string_pretty(self).context("serializing model catalog")?;
        let staging = path.with_extension("json.tmp");
        std::fs::write(&staging, text).with_context(|| format!("writing {}", staging.display()))?;
        std::fs::rename(&staging, &path).with_context(|| format!("writing {}", path.display()))
    }

    fn entry(&self, provider: &ProviderConfig) -> Option<&CatalogEntry> {
        self.providers
            .get(&provider.name)
            .filter(|entry| entry.fingerprint == fingerprint(provider))
    }

    /// Whether some configured provider has never been listed, so a
    /// `session/new` would offer only its configured model.
    pub fn is_missing_any(&self, config: &Config) -> bool {
        providers(config).iter().any(|p| self.entry(p).is_none())
    }

    /// Whether a background refresh is due: something is missing, or the
    /// oldest list is past its TTL.
    pub fn needs_refresh(&self, config: &Config, now: u64) -> bool {
        providers(config).iter().any(|p| match self.entry(p) {
            None => true,
            Some(entry) => now.saturating_sub(entry.fetched_at) > CATALOG_TTL.as_secs(),
        })
    }

    /// List every provider's models concurrently. A provider that fails or
    /// times out keeps its previous list, if it had one.
    pub async fn refresh(&self, config: &Config) -> Self {
        let fetches = providers(config).into_iter().map(|provider| async move {
            let listed = async {
                let client = provider.build()?;
                client.list_models().await
            };
            let models = match tokio::time::timeout(LIST_TIMEOUT, listed).await {
                Ok(Ok(models)) => Some(models),
                Ok(Err(err)) => {
                    tracing::debug!("acp: listing models for {} failed: {err:#}", provider.name);
                    None
                }
                Err(_) => {
                    tracing::debug!("acp: listing models for {} timed out", provider.name);
                    None
                }
            };
            (provider, models)
        });
        let now = now_secs();
        let mut next = Self {
            version: CATALOG_VERSION,
            ..Self::default()
        };
        for (provider, models) in futures_util::future::join_all(fetches).await {
            let entry = match models {
                Some(models) => {
                    let mut kept: Vec<String> = Vec::new();
                    for tag in models {
                        if is_chat_model(&tag) && !kept.contains(&tag) {
                            kept.push(tag);
                        }
                    }
                    kept.truncate(MODELS_PER_PROVIDER);
                    CatalogEntry {
                        fingerprint: fingerprint(&provider),
                        fetched_at: now,
                        models: kept,
                    }
                }
                None => match self.entry(&provider) {
                    Some(previous) => previous.clone(),
                    None => continue,
                },
            };
            next.providers.insert(provider.name.clone(), entry);
        }
        next
    }

    #[cfg(test)]
    fn with(mut self, provider: &ProviderConfig, fetched_at: u64, models: &[&str]) -> Self {
        self.providers.insert(
            provider.name.clone(),
            CatalogEntry {
                fingerprint: fingerprint(provider),
                fetched_at,
                models: models.iter().map(|m| m.to_string()).collect(),
            },
        );
        self
    }
}

/// One row of the model select.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Choice {
    pub id: String,
    pub label: String,
    pub description: String,
}

/// Every model a session can pick: the selected provider first, then the
/// rest in config order; within a provider, what it listed (newest first),
/// then its configured model and the current selection when it didn't list
/// them — the selection is always present.
pub(super) fn choices(config: &Config, catalog: &Catalog, selection: &Selection) -> Vec<Choice> {
    let mut providers = providers(config);
    providers.sort_by_key(|p| p.name != selection.provider);
    let mut rows: Vec<(ProviderConfig, String)> = Vec::new();
    for provider in providers {
        let mut tags = Vec::new();
        if let Some(entry) = catalog.entry(&provider) {
            tags.extend(entry.models.iter().cloned());
        }
        tags.push(provider.model.clone());
        if provider.name == selection.provider {
            tags.push(selection.model.clone());
        }
        let mut seen: Vec<String> = Vec::new();
        for tag in tags {
            if tag.trim().is_empty() || seen.contains(&tag) {
                continue;
            }
            seen.push(tag.clone());
            rows.push((provider.clone(), tag));
        }
    }
    rows.iter()
        .map(|(provider, tag)| {
            // The same tag under two providers (an API key and an account on
            // one vendor) needs the provider to tell the rows apart.
            let shared = rows
                .iter()
                .filter(|(other, other_tag)| other_tag == tag && other.name != provider.name)
                .count()
                > 0;
            let label = if shared {
                format!("{tag} ({})", provider.name)
            } else {
                tag.clone()
            };
            let backend = provider
                .descriptor()
                .map(|descriptor| descriptor.display_name().to_string())
                .unwrap_or_else(|| provider.kind.as_str().to_string());
            let mut description = format!("{backend} · {}", provider.name);
            if *tag == provider.model {
                description.push_str(" · configured");
            }
            Choice {
                id: format!("{}/{tag}", provider.name),
                label,
                description,
            }
        })
        .collect()
}

/// The config options a session advertises in `session/new`,
/// `session/load`, and every `session/set_config_option` answer.
pub(super) fn config_options(
    config: &Config,
    catalog: &Catalog,
    selection: &Selection,
) -> Vec<SessionConfigOption> {
    let models: Vec<SessionConfigSelectOption> = choices(config, catalog, selection)
        .into_iter()
        .map(|choice| {
            SessionConfigSelectOption::new(choice.id, choice.label).description(choice.description)
        })
        .collect();
    let efforts = [
        (EFFORT_DEFAULT, "Default", "The provider's own default"),
        ("low", "Low", "Fastest, least reasoning"),
        ("medium", "Medium", "Balanced"),
        ("high", "High", "Deeper reasoning"),
        (
            "xhigh",
            "Extra high",
            "Grok 4.6 and later; others treat it as high",
        ),
    ]
    .into_iter()
    .map(|(value, name, description)| {
        SessionConfigSelectOption::new(value, name).description(description.to_string())
    })
    .collect::<Vec<_>>();
    let modes = vec![
        SessionConfigSelectOption::new("genie", "Genie")
            .description("Interactive: acts on each request and reports back".to_string()),
        SessionConfigSelectOption::new("sovereign", "Sovereign")
            .description("Autonomous: keeps working toward the goal on its own".to_string()),
    ];
    vec![
        SessionConfigOption::select(MODEL_OPTION, "Model", selection.model_id(), models)
            .category(SessionConfigOptionCategory::Model)
            .description("Provider and model this session runs".to_string()),
        SessionConfigOption::select(
            EFFORT_OPTION,
            "Reasoning",
            selection
                .effort
                .map_or(EFFORT_DEFAULT, ReasoningEffort::as_str),
            efforts,
        )
        .category(SessionConfigOptionCategory::ThoughtLevel)
        .description(
            "Reasoning effort, for models that take one (Grok 4.x, GPT-5, o-series)".to_string(),
        ),
        SessionConfigOption::select(MODE_OPTION, "Mode", mode_value(selection.mode), modes)
            .description("Wizard's personality mode".to_string()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderKind;

    fn provider(name: &str, kind: &'static str, model: &str) -> ProviderConfig {
        ProviderConfig {
            name: name.to_string(),
            kind: ProviderKind::known(kind),
            base_url: format!("https://{name}.test/v1"),
            model: model.to_string(),
            api_key_env: None,
            gguf_path: None,
            usd_per_mtok_in: None,
            usd_per_mtok_out: None,
            vision: None,
        }
    }

    fn config(providers: Vec<ProviderConfig>, active: Option<&str>) -> Config {
        Config {
            providers,
            active_provider: active.map(str::to_string),
            ..Config::default()
        }
    }

    #[test]
    fn a_selection_starts_from_the_active_provider_and_applies_to_a_copy() {
        let config = config(
            vec![
                provider("xai", "xaioauth", "grok-4.6"),
                provider("chatgpt", "chatgptoauth", "gpt-5.6-sol"),
            ],
            Some("xai"),
        );
        let mut selection = Selection::from_config(&config);
        assert_eq!(selection.model_id(), "xai/grok-4.6");

        selection.provider = "chatgpt".into();
        selection.model = "gpt-5.6-mini".into();
        selection.effort = Some(ReasoningEffort::High);
        selection.mode = Mode::Sovereign;
        let applied = selection.apply(&config);
        assert_eq!(applied.active().name, "chatgpt");
        assert_eq!(applied.active().model, "gpt-5.6-mini");
        assert_eq!(applied.reasoning_effort, Some(ReasoningEffort::High));
        assert_eq!(applied.mode, Mode::Sovereign);
        // The original is untouched: a session's pick is its own.
        assert_eq!(config.active().name, "xai");
        assert_eq!(config.active().model, "grok-4.6");
    }

    #[test]
    fn with_no_providers_the_local_default_is_the_one_choice() {
        let config = config(Vec::new(), None);
        let selection = Selection::from_config(&config);
        assert_eq!(selection.provider, "local");
        assert_eq!(
            parse_model_id("local/qwen3-coder:30b", &config),
            Some(("local".into(), "qwen3-coder:30b".into()))
        );
        let mut picked = selection.clone();
        picked.model = "qwen3-coder:30b".into();
        assert_eq!(picked.apply(&config).active().model, "qwen3-coder:30b");
    }

    #[test]
    fn model_ids_split_at_the_first_slash_and_name_a_configured_provider() {
        let config = config(
            vec![provider(
                "openrouter",
                "openrouter",
                "anthropic/claude-sonnet-5",
            )],
            None,
        );
        assert_eq!(
            parse_model_id("openrouter/anthropic/claude-sonnet-5", &config),
            Some(("openrouter".into(), "anthropic/claude-sonnet-5".into()))
        );
        assert_eq!(parse_model_id("elsewhere/gpt-5", &config), None);
        assert_eq!(parse_model_id("openrouter/", &config), None);
        assert_eq!(parse_model_id("no-slash", &config), None);
    }

    #[test]
    fn efforts_and_modes_parse_their_select_values() {
        assert_eq!(parse_effort("default"), Some(None));
        assert_eq!(parse_effort("xhigh"), Some(Some(ReasoningEffort::Xhigh)));
        assert_eq!(parse_effort("max"), None);
        assert_eq!(parse_mode("sovereign"), Some(Mode::Sovereign));
        assert_eq!(parse_mode("yolo"), None);
    }

    #[test]
    fn only_chat_models_are_offered() {
        for tag in ["grok-4.6", "gpt-5.6-sol", "claude-opus-5-5", "qwen3:30b"] {
            assert!(is_chat_model(tag), "{tag}");
        }
        for tag in [
            "grok-imagine-image",
            "grok-2-image-1212",
            "text-embedding-3-large",
            "gpt-4o-mini-tts",
            "whisper-1",
            "",
        ] {
            assert!(!is_chat_model(tag), "{tag}");
        }
    }

    #[test]
    fn choices_put_the_selected_provider_first_newest_models_first() {
        let xai = provider("xai", "xaioauth", "grok-4.6");
        let key = provider("xai-key", "xai", "grok-4.6");
        let config = config(vec![key.clone(), xai.clone()], Some("xai"));
        let catalog = Catalog::default()
            .with(&xai, 1, &["grok-4.7", "grok-4.6", "grok-4.6-fast"])
            .with(&key, 1, &["grok-4.6"]);
        let selection = Selection::from_config(&config);
        let rows = choices(&config, &catalog, &selection);
        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "xai/grok-4.7",
                "xai/grok-4.6",
                "xai/grok-4.6-fast",
                "xai-key/grok-4.6"
            ]
        );
        assert_eq!(rows[0].label, "grok-4.7");
        assert_eq!(rows[1].label, "grok-4.6 (xai)");
        assert!(rows[1].description.ends_with("· configured"));
    }

    #[test]
    fn an_unlisted_selection_follows_the_listed_models() {
        let xai = provider("xai", "xaioauth", "grok-legacy");
        let config = config(vec![xai.clone()], Some("xai"));
        let catalog = Catalog::default().with(&xai, 1, &["grok-4.7", "grok-4.6"]);
        let rows = choices(&config, &catalog, &Selection::from_config(&config));
        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["xai/grok-4.7", "xai/grok-4.6", "xai/grok-legacy"]);
    }

    #[test]
    fn a_catalog_for_another_backend_is_not_offered() {
        let before = provider("main", "openai", "gpt-5.6");
        let mut after = before.clone();
        after.base_url = "http://127.0.0.1:11434".into();
        let catalog = Catalog::default().with(&before, 1, &["gpt-5.6", "gpt-5.6-mini"]);
        let config = config(vec![after], None);
        assert!(catalog.is_missing_any(&config));
        let rows = choices(&config, &catalog, &Selection::from_config(&config));
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn a_stale_or_missing_list_is_due_for_refresh() {
        let xai = provider("xai", "xaioauth", "grok-4.6");
        let config = config(vec![xai.clone()], None);
        let now = 1_000_000;
        assert!(Catalog::default().needs_refresh(&config, now));
        let fresh = Catalog::default().with(&xai, now - 60, &["grok-4.6"]);
        assert!(!fresh.needs_refresh(&config, now));
        assert!(!fresh.is_missing_any(&config));
        let stale = Catalog::default().with(&xai, now - CATALOG_TTL.as_secs() - 1, &[]);
        assert!(stale.needs_refresh(&config, now));
    }

    #[test]
    fn the_options_carry_the_categories_clients_read() {
        let config = config(vec![provider("xai", "xaioauth", "grok-4.6")], None);
        let mut selection = Selection::from_config(&config);
        selection.effort = Some(ReasoningEffort::Medium);
        let options =
            serde_json::to_value(config_options(&config, &Catalog::default(), &selection)).unwrap();
        assert_eq!(options[0]["id"], MODEL_OPTION);
        assert_eq!(options[0]["category"], "model");
        assert_eq!(options[0]["type"], "select");
        assert_eq!(options[0]["currentValue"], "xai/grok-4.6");
        assert_eq!(options[0]["options"][0]["value"], "xai/grok-4.6");
        assert_eq!(options[1]["id"], EFFORT_OPTION);
        assert_eq!(options[1]["category"], "thought_level");
        assert_eq!(options[1]["currentValue"], "medium");
        assert_eq!(options[2]["id"], MODE_OPTION);
        assert!(options[2].get("category").is_none());
        assert_eq!(options[2]["currentValue"], "genie");
    }
}
