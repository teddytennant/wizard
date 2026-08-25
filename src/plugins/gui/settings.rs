//! The window's view of `~/.wizard/config.toml`: a store that serializes writes
//! and always re-reads the file before changing it, plus the provider presets
//! the settings sheet and onboarding offer.
//!
//! Config is shared mutable state across processes — the TUI, other windows,
//! and this one all write the same file, and [`Config::save`]
//! rewrites it whole. A long-lived process that saved a snapshot it loaded at
//! startup would silently drop everything added since. So every mutation here
//! re-reads the file, applies the change to *that*, and writes it back under a
//! lock; a stale in-memory copy can never be the thing that lands on disk.

use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result};

use crate::config::{Config, Credentials, ProviderConfig, ProviderKind};
use crate::credentials;
use crate::llm::registry::defaults;
use crate::llm::xai_oauth;

/// A provider offered by the settings sheet's "add provider" list and by
/// onboarding: the defaults to prefill, and what the user still has to give.
#[derive(Debug, Clone)]
pub struct Preset {
    /// Suggested provider name (the credentials key, and the sidebar label).
    pub name: &'static str,
    pub label: &'static str,
    pub kind: &'static str,
    pub base_url: &'static str,
    pub model: &'static str,
    /// The provider cannot answer without an API key.
    pub needs_key: bool,
    /// The base URL is a template the user must complete (Cloudflare's
    /// account id); the UI keeps the field editable and shows the placeholder.
    pub needs_base_url: bool,
}

/// The providers the window can set up by pasting a key. `xaioauth` and
/// `chatgptoauth` are deliberately absent: a subscription is not a string you
/// can paste, so they are earned through the sign-in rows
/// (`POST /api/login/{provider}`, see [`crate::plugins::gui::oauth`]) and then show up
/// here like any other provider.
pub const PRESETS: &[Preset] = &[
    Preset {
        name: "anthropic",
        label: "Anthropic",
        kind: "anthropic",
        base_url: "https://api.anthropic.com",
        model: "claude-fable-5",
        needs_key: true,
        needs_base_url: false,
    },
    Preset {
        name: "openai",
        label: "OpenAI",
        kind: "openai",
        base_url: "https://api.openai.com/v1",
        model: "gpt-5.6-sol",
        needs_key: true,
        needs_base_url: false,
    },
    Preset {
        name: "xai",
        label: "xAI",
        kind: "xai",
        base_url: "https://api.x.ai/v1",
        model: xai_oauth::DEFAULT_MODEL,
        needs_key: true,
        needs_base_url: false,
    },
    Preset {
        name: "openrouter",
        label: "OpenRouter",
        kind: "openrouter",
        base_url: defaults::OPENROUTER_BASE_URL,
        model: defaults::OPENROUTER_MODEL,
        needs_key: true,
        needs_base_url: false,
    },
    Preset {
        name: "cloudflare",
        label: "Cloudflare Workers AI",
        kind: "cloudflare",
        base_url: defaults::CLOUDFLARE_BASE_URL_TEMPLATE,
        model: defaults::CLOUDFLARE_MODEL,
        needs_key: true,
        needs_base_url: true,
    },
    Preset {
        name: "ollama",
        label: "Ollama",
        kind: "ollama",
        base_url: "http://127.0.0.1:11434",
        model: "qwen3:8b",
        needs_key: false,
        needs_base_url: false,
    },
    Preset {
        name: "llamacpp",
        label: "llama.cpp",
        kind: "llamacpp",
        base_url: "http://127.0.0.1:11435",
        model: "qwen3.6:27b",
        needs_key: false,
        needs_base_url: false,
    },
];

/// Every preset the settings sheet offers: the dedicated-kind rows above,
/// followed by the OpenAI-compatible cloud providers from
/// [`crate::llm::compat::PRESETS`].
pub fn presets() -> Vec<Preset> {
    PRESETS
        .iter()
        .cloned()
        .chain(crate::llm::compat::PRESETS.iter().map(|preset| Preset {
            name: preset.name,
            label: preset.label,
            kind: "openai",
            base_url: preset.base_url,
            model: preset.default_model(),
            needs_key: true,
            needs_base_url: false,
        }))
        .collect()
}

/// Where a provider's API key comes from, for the settings sheet's key column.
/// The order mirrors [`ProviderConfig`]'s own resolution: the environment
/// variable wins over the credential file.
#[derive(Debug, Clone, Copy)]
pub enum KeySource {
    /// Stored in `~/.wizard/credentials.toml` under the provider's name, with
    /// no environment variable overriding it.
    Stored,
    /// Read from an environment variable at request time. Reported even when a
    /// key is also stored, because the variable is what wins.
    Env,
    /// An OAuth token from `wizard login` — no key to manage here.
    Oauth,
    /// A local backend that needs no key.
    NotNeeded,
    /// The provider needs a key and has none: requests will 401.
    Missing,
}

/// Where `provider` would get its key right now.
pub fn key_source(provider: &ProviderConfig) -> KeySource {
    key_source_from(provider, |name| std::env::var(name).ok(), credentials::get)
}

/// Testable core of [`key_source`]: `lookup` supplies the value of an
/// environment variable and `stored` the key held under a provider name in
/// `credentials.toml`, both `None` when unset.
///
/// The order here is not this module's to choose: it has to be the order
/// `ProviderConfig::resolved_key` uses, because that is the key the next
/// request actually goes out with. That resolver takes the environment
/// variable first (exporting a key is the documented one-run override, so it
/// must not be silently ignored because something was stored months ago), and
/// this column used to answer credentials-first, so a user who exported a
/// fresh key over a revoked stored one was shown "stored" for a provider
/// running off the environment, and had no way to see why their requests
/// 401'd. The resolver itself is private to [`crate::config`], so the
/// precedence is mirrored here rather than called; the test below pins the two
/// together over the same table of cases.
fn key_source_from(
    provider: &ProviderConfig,
    lookup: impl Fn(&str) -> Option<String>,
    stored: impl Fn(&str) -> Option<String>,
) -> KeySource {
    // Three questions this used to answer with two separate hand-maintained
    // tables — one here, one in the arms of `ProviderConfig::build` — that had
    // to agree and were never checked against each other. Both now read the
    // one descriptor.
    let credentials = provider.credentials();
    match &credentials {
        Credentials::Local => return KeySource::NotNeeded,
        Credentials::Account { .. } => return KeySource::Oauth,
        Credentials::ApiKey { .. } => {}
    }
    let env = provider
        .api_key_env
        .as_deref()
        .or_else(|| credentials.default_env());
    // Trimmed on both sides, exactly as the resolver trims: a variable holding
    // only whitespace is not a key, and neither is a blank stored entry.
    if env
        .and_then(&lookup)
        .is_some_and(|key| !key.trim().is_empty())
    {
        return KeySource::Env;
    }
    if stored(&provider.name).is_some_and(|key| !key.trim().is_empty()) {
        return KeySource::Stored;
    }
    KeySource::Missing
}

/// Serialized access to `~/.wizard/config.toml`.
///
/// `current()` is the config the window acts on (env overrides applied, as
/// everywhere else in wizard); `update()` is the only way to change the file.
pub struct ConfigStore {
    /// The last known config, kept so a transient read failure still leaves
    /// the window able to answer.
    cached: Mutex<Config>,
    /// Held across the read-modify-write of a mutation, so two concurrent
    /// settings requests cannot interleave into a lost update.
    write_lock: Mutex<()>,
}

impl ConfigStore {
    pub fn new(config: Config) -> Self {
        Self {
            cached: Mutex::new(config),
            write_lock: Mutex::new(()),
        }
    }

    /// The current config: re-read from disk so edits made by the TUI (or
    /// another window) are picked up without a restart. A read failure falls
    /// back to the last good copy rather than failing the read.
    pub fn current(&self) -> Config {
        match Config::load() {
            Ok(config) => {
                *self.lock_cached() = config.clone();
                config
            }
            Err(err) => {
                tracing::warn!("re-reading the config failed, using the cached copy: {err:#}");
                self.lock_cached().clone()
            }
        }
    }

    /// Apply `mutate` to the config **as it is on disk** and write it back.
    ///
    /// The mutation runs against a raw parse of the file — not against
    /// [`Config::load`], whose env overrides (`WIZARD_MODEL` and friends) would
    /// otherwise be baked into the file as if the user had typed them.
    pub fn update<F>(&self, mutate: F) -> Result<Config>
    where
        F: FnOnce(&mut Config) -> Result<()>,
    {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let mut on_disk = read_raw()?;
        mutate(&mut on_disk)?;
        on_disk.save().context("saving the config")?;
        drop(_guard);
        Ok(self.current())
    }

    fn lock_cached(&self) -> std::sync::MutexGuard<'_, Config> {
        self.cached.lock().unwrap_or_else(|err| err.into_inner())
    }
}

/// Parse `~/.wizard/config.toml` with no env overrides applied. A missing file
/// is a fresh install, not an error.
fn read_raw() -> Result<Config> {
    let path = Config::path()?;
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(Config::default());
    };
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// The path the settings sheet shows, best-effort.
pub fn config_path() -> String {
    Config::path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "~/.wizard/config.toml".to_string())
}

/// Add `provider`, replacing any provider of the same name (that is what an
/// edit is), and make it active when asked.
pub fn upsert_provider(config: &mut Config, provider: ProviderConfig, activate: bool) {
    let name = provider.name.clone();
    config.providers.retain(|existing| existing.name != name);
    config.providers.push(provider);
    if activate || config.active_provider.is_none() {
        config.active_provider = Some(name);
    }
}

/// Remove the provider named `name`. Removing the active one hands `active` to
/// whatever is left, so the config never points at a provider that is gone.
pub fn remove_provider(config: &mut Config, name: &str) -> Result<()> {
    anyhow::ensure!(
        config.providers.iter().any(|p| p.name == name),
        "no provider named '{name}'"
    );
    config.providers.retain(|p| p.name != name);
    if config.active_provider.as_deref() == Some(name) {
        config.active_provider = config.providers.first().map(|p| p.name.clone());
    }
    Ok(())
}

/// Store a `~/.wizard/credentials.toml` entry; an empty `key` is ignored
/// (an edit that leaves the field blank keeps the stored key).
pub fn store_key(name: &str, key: &str) -> Result<()> {
    if key.trim().is_empty() {
        return Ok(());
    }
    credentials::store(name, key.trim())
}

/// Path of the credentials file, for the settings sheet's "where keys live" note.
pub fn credentials_path() -> Option<PathBuf> {
    credentials::path().ok()
}

/* ---------------------------------------------------------------------- */
/* What a settings screen shows, and what its buttons do                  */
/* ---------------------------------------------------------------------- */

// Everything below was `src/plugins/gui/server.rs`'s, sitting between an axum extractor
// and an axum response. None of it was web-specific: a settings screen has to
// list the providers, say where each one's key comes from, prove one answers,
// add, edit, remove and switch, whatever draws it. Pulling it out of the route
// handlers is what let the window call the same functions instead of writing a
// second answer to "what does Remove do to the active provider" — and it is why
// deleting those handlers with the rest of the browser GUI cost nothing here.

/// How long a provider gets to answer a probe.
///
/// Ten seconds because this is a person waiting on a button they just pressed;
/// a provider that needs longer than that to list its models is a provider that
/// is going to make the first turn feel broken too.
pub const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The largest step limit the settings screen accepts. A sanity bound on a
/// number typed into a box, not a policy: 0 (unlimited) is the default and is
/// always allowed.
pub const MAX_STEP_LIMIT: u32 = 1000;

/// What a save or a test tells the user about whether the provider works.
#[derive(Debug, Clone)]
pub struct ProviderProbe {
    pub ok: bool,
    pub error: Option<String>,
    pub models: Vec<String>,
}

/// One configured provider, as a settings screen lists it.
#[derive(Debug, Clone)]
pub struct ProviderRow {
    pub name: String,
    pub kind: String,
    pub base_url: String,
    pub model: String,
    /// Where its key comes from — the field that makes "why is it 401ing"
    /// answerable without opening a file.
    pub key: KeySource,
    pub active: bool,
}

/// The whole settings screen's state, derived from a [`Config`].
#[derive(Debug, Clone)]
pub struct SettingsView {
    /// Nothing is configured, so a surface should open onboarding rather than a
    /// chat: there is nothing to send a message to yet.
    pub first_run: bool,
    pub config_path: String,
    pub credentials_path: Option<String>,
    pub active: Option<String>,
    /// 0 means no limit.
    pub max_steps: u32,
    pub providers: Vec<ProviderRow>,
    pub presets: Vec<Preset>,
}

/// The settings screen for `config`.
pub fn view(config: &Config) -> SettingsView {
    let active = config.active().name;
    SettingsView {
        first_run: config.providers.is_empty(),
        config_path: config_path(),
        credentials_path: credentials_path().map(|path| path.display().to_string()),
        active: config.active_provider.clone(),
        max_steps: config.max_steps.cap().unwrap_or(0),
        providers: config
            .providers
            .iter()
            .map(|provider| ProviderRow {
                name: provider.name.clone(),
                kind: provider.kind.to_string(),
                base_url: provider.base_url.clone(),
                model: provider.model.clone(),
                key: key_source(provider),
                active: provider.name == active,
            })
            .collect(),
        presets: presets(),
    }
}

/// A [`ProviderKind`] from the string a form field carries.
///
/// Used to round-trip through TOML, because deserializing was the only place
/// that knew which spellings were valid. Deserializing no longer rejects
/// anything (see [`crate::llm::registry`]), so the check is now what it was
/// always standing in for: is a backend registered under this id.
pub fn parse_kind(kind: &str) -> Result<ProviderKind, String> {
    let kind = ProviderKind::new(kind);
    match crate::llm::registry::installed(&kind) {
        Some(_) => Ok(kind),
        None => Err(crate::llm::registry::unknown(&kind).to_string()),
    }
}

/// Build the provider's client and ask it for its models: the cheapest call
/// that proves the base URL, the key and the network all work at once.
pub async fn probe(provider: &ProviderConfig) -> ProviderProbe {
    let client = match provider.build() {
        Ok(client) => client,
        Err(err) => {
            return ProviderProbe {
                ok: false,
                error: Some(format!("{err:#}")),
                models: Vec::new(),
            };
        }
    };
    match tokio::time::timeout(PROBE_TIMEOUT, client.list_models()).await {
        Ok(Ok(models)) => ProviderProbe {
            ok: true,
            error: None,
            models,
        },
        Ok(Err(err)) => ProviderProbe {
            ok: false,
            error: Some(format!("{err:#}")),
            models: Vec::new(),
        },
        Err(_) => ProviderProbe {
            ok: false,
            error: Some(format!(
                "the provider did not answer within {}s",
                PROBE_TIMEOUT.as_secs()
            )),
            models: Vec::new(),
        },
    }
}

/// One provider, as the form submits it. Reusing an existing `name` *is* an
/// edit: the name is the identity, and the credential file is keyed by it.
#[derive(Debug, Clone)]
pub struct NewProvider {
    pub name: String,
    pub kind: String,
    pub base_url: String,
    pub model: String,
    /// `None` on an edit that left the field blank, which keeps the stored key.
    pub api_key: Option<String>,
    /// Make it active. The form defaults it to yes: you configured it in order
    /// to use it.
    pub activate: bool,
}

/// Why a settings write did not happen.
///
/// Two variants and not one, because the two have different answers: the first
/// is the form's fault and the field can be corrected, the second is the disk's
/// and it cannot. The window puts the first under the field and the second in a
/// notice.
#[derive(Debug)]
pub enum SaveError {
    /// The form is wrong. Safe to show verbatim next to the field.
    Invalid(String),
    /// Storing the key or writing the config failed.
    Failed(anyhow::Error),
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SaveError::Invalid(why) => write!(f, "{why}"),
            SaveError::Failed(err) => write!(f, "{err:#}"),
        }
    }
}

/// Persist a provider (and its key), then probe it.
///
/// The provider is saved **even when the probe fails**, and that is a decision
/// rather than an oversight: a typo'd key should leave an editable row, not
/// vanish and make the user retype the base URL too. The probe result says so
/// plainly instead.
pub async fn save_provider(
    store: &ConfigStore,
    form: NewProvider,
) -> Result<(SettingsView, ProviderProbe), SaveError> {
    let name = form.name.trim().to_string();
    if name.is_empty() {
        return Err(SaveError::Invalid("the provider needs a name".to_string()));
    }
    let kind = parse_kind(&form.kind).map_err(SaveError::Invalid)?;
    let base_url = form.base_url.trim().to_string();
    let model = form.model.trim().to_string();

    if base_url.is_empty() {
        return Err(SaveError::Invalid(
            "the provider needs a base URL".to_string(),
        ));
    }
    if model.is_empty() {
        return Err(SaveError::Invalid("the provider needs a model".to_string()));
    }
    if let Some(key) = &form.api_key {
        store_key(&name, key).map_err(SaveError::Failed)?;
    }

    let provider = ProviderConfig {
        name,
        kind,
        base_url,
        model,
        // The key lives in the credential file under this provider's name; an
        // env var would be a second source of truth for the same secret.
        api_key_env: None,
        gguf_path: None,
        usd_per_mtok_in: None,
        usd_per_mtok_out: None,
    };
    let config = store
        .update({
            let provider = provider.clone();
            move |config| {
                upsert_provider(config, provider, form.activate);
                Ok(())
            }
        })
        .map_err(SaveError::Failed)?;
    Ok((view(&config), probe(&provider).await))
}

/// Probe a provider already in the config. `None` when there is no such
/// provider — which a settings screen should treat as a stale row rather than
/// as a failure of the provider.
pub async fn test_provider(store: &ConfigStore, name: &str) -> Option<ProviderProbe> {
    let config = store.current();
    let provider = config.providers.iter().find(|p| p.name == name)?;
    Some(probe(provider).await)
}

/// Switch the active provider.
pub fn activate_provider(store: &ConfigStore, name: &str) -> Result<SettingsView> {
    let config = store.update(|config| {
        anyhow::ensure!(
            config.providers.iter().any(|p| p.name == name),
            "no provider named '{name}'"
        );
        config.active_provider = Some(name.to_string());
        Ok(())
    })?;
    Ok(view(&config))
}

/// Forget a provider and its stored key.
///
/// The key removal is best-effort: a leftover credential is harmless, but
/// leaving it behind would silently reattach to a provider re-added under the
/// same name later, which is the confusing half of the two failures.
pub fn forget_provider(store: &ConfigStore, name: &str) -> Result<SettingsView> {
    let config = store.update(|config| remove_provider(config, name))?;
    if let Err(err) = credentials::remove(name) {
        tracing::warn!("could not remove the stored key for '{name}': {err:#}");
    }
    Ok(view(&config))
}

/// Set the step budget every surface runs on. `0` is no limit.
pub fn set_step_limit(store: &ConfigStore, steps: u32) -> Result<SettingsView, SaveError> {
    if steps > MAX_STEP_LIMIT {
        return Err(SaveError::Invalid(format!(
            "the step limit must be 0 (no limit) or at most {MAX_STEP_LIMIT}"
        )));
    }
    let config = store
        .update(|config| {
            config.max_steps = crate::config::StepBudget::new(steps);
            Ok(())
        })
        .map_err(SaveError::Failed)?;
    Ok(view(&config))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(name: &str) -> ProviderConfig {
        ProviderConfig {
            name: name.to_string(),
            kind: ProviderKind::OPENAI,
            base_url: "https://example.test/v1".to_string(),
            model: "m".to_string(),
            api_key_env: None,
            gguf_path: None,
            usd_per_mtok_in: None,
            usd_per_mtok_out: None,
        }
    }

    #[test]
    fn upsert_replaces_by_name_and_can_activate() {
        let mut config = Config::default();
        upsert_provider(&mut config, provider("a"), false);
        // The first provider always becomes active — a config with providers
        // and no active one would silently fall back to the first anyway.
        assert_eq!(config.active_provider.as_deref(), Some("a"));

        upsert_provider(&mut config, provider("b"), false);
        assert_eq!(config.providers.len(), 2);
        assert_eq!(config.active_provider.as_deref(), Some("a"));

        let mut edited = provider("a");
        edited.model = "m2".to_string();
        upsert_provider(&mut config, edited, true);
        assert_eq!(config.providers.len(), 2, "an edit is not a second entry");
        assert_eq!(config.active_provider.as_deref(), Some("a"));
        assert_eq!(
            config
                .providers
                .iter()
                .find(|p| p.name == "a")
                .unwrap()
                .model,
            "m2"
        );
    }

    #[test]
    fn removing_the_active_provider_hands_active_to_a_survivor() {
        let mut config = Config::default();
        upsert_provider(&mut config, provider("a"), true);
        upsert_provider(&mut config, provider("b"), false);
        remove_provider(&mut config, "a").unwrap();
        assert_eq!(config.active_provider.as_deref(), Some("b"));

        remove_provider(&mut config, "b").unwrap();
        assert!(config.providers.is_empty());
        assert_eq!(config.active_provider, None, "nothing left to point at");
        assert!(remove_provider(&mut config, "gone").is_err());
    }

    #[test]
    fn local_providers_need_no_key() {
        let mut local = provider("local");
        local.kind = ProviderKind::LLAMACPP;
        assert!(matches!(key_source(&local), KeySource::NotNeeded));

        let mut oauth = provider("xai");
        oauth.kind = ProviderKind::XAI_OAUTH;
        assert!(matches!(key_source(&oauth), KeySource::Oauth));
    }

    #[test]
    fn a_cloud_provider_with_no_key_anywhere_is_reported_missing() {
        // A distinctive name: the credential store is shared process-wide in
        // tests, and this provider must have no stored key.
        let unkeyed = provider("settings-test-unkeyed");
        assert!(matches!(key_source(&unkeyed), KeySource::Missing));

        // An env fallback that names an unset variable is no key either — the
        // settings sheet must say "missing", not "env".
        let mut env_only = provider("settings-test-env");
        env_only.api_key_env = Some("WIZARD_TEST_KEY_THAT_IS_NEVER_SET".to_string());
        assert!(matches!(key_source(&env_only), KeySource::Missing));
    }

    /// Adversarial: the key column must name the key the *next request* will
    /// send, not the one most recently written. `ProviderConfig::resolved_key`
    /// takes the environment variable first, and this column used to answer
    /// credentials-first, so a user who exported a fresh key over a revoked
    /// stored one saw "stored" while every request went out with the export.
    /// Driven through the injectable core so nothing here touches the process
    /// environment or the `credentials.toml` this test binary shares.
    #[test]
    fn the_key_column_names_the_key_that_actually_wins() {
        let nothing = |_: &str| None;
        let env_is = |value: &'static str| move |_: &str| Some(value.to_string());
        let stored_is = |value: &'static str| move |_: &str| Some(value.to_string());

        let mut cloud = provider("openai");
        cloud.kind = ProviderKind::OPENAI;
        cloud.api_key_env = Some("OPENAI_API_KEY".to_string());

        // Both set: the export wins, so the column has to say so.
        assert!(matches!(
            key_source_from(&cloud, env_is("sk-exported"), stored_is("sk-pasted")),
            KeySource::Env
        ));
        // Only stored.
        assert!(matches!(
            key_source_from(&cloud, nothing, stored_is("sk-pasted")),
            KeySource::Stored
        ));
        // Only exported.
        assert!(matches!(
            key_source_from(&cloud, env_is("sk-exported"), nothing),
            KeySource::Env
        ));
        // A variable holding whitespace is not a key: the stored one is still
        // what resolves, and the column must not claim otherwise.
        assert!(matches!(
            key_source_from(&cloud, env_is("   "), stored_is("sk-pasted")),
            KeySource::Stored
        ));
        // Neither: the state that 401s.
        assert!(matches!(
            key_source_from(&cloud, nothing, nothing),
            KeySource::Missing
        ));

        // A kind with a default env var and none configured still reads it.
        let mut defaulted = provider("openrouter");
        defaulted.kind = ProviderKind::OPENROUTER;
        defaulted.api_key_env = None;
        assert!(matches!(
            key_source_from(
                &defaulted,
                |name: &str| (name == defaults::OPENROUTER_KEY_ENV).then(|| "sk-or".to_string()),
                nothing
            ),
            KeySource::Env
        ));

        // Backends that need no key are answered before either lookup runs.
        let mut local = provider("local");
        local.kind = ProviderKind::LLAMACPP;
        assert!(matches!(
            key_source_from(&local, env_is("sk-ignored"), stored_is("sk-ignored")),
            KeySource::NotNeeded
        ));
    }

    #[test]
    fn presets_are_all_valid_provider_kinds() {
        for preset in presets() {
            let kind = parse_kind(preset.kind)
                .unwrap_or_else(|err| panic!("preset {}: {err}", preset.name));
            // Every preset that needs no key must be a local backend, which
            // the descriptor now answers directly instead of by listing the
            // two kinds that happen to be local today.
            if !preset.needs_key {
                let descriptor = crate::llm::registry::installed(&kind).expect("registered");
                assert!(
                    descriptor.credentials().is_local(),
                    "preset {}",
                    preset.name
                );
            }
        }
    }
}
