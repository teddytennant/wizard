//! Wizard configuration.
//!
//! A small, single-provider config: the model, the OpenAI-compatible endpoint
//! the NexAU agent talks to, the working directory the agent operates in, and
//! the personality mode. It is stored as TOML at `~/.wizard/config.toml` and
//! resolved into a [`BridgeConfig`] to launch the Python bridge.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::backend::nexau::BridgeConfig;

/// Personality mode. Purely cosmetic in Wizard (the agent loop lives in
/// NexAU); it drives the `/mode` switch and the status-bar label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Interactive: acts on each turn and stops.
    #[default]
    Genie,
    /// Autonomous framing for longer, self-directed work.
    Sovereign,
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Mode::Genie => f.write_str("genie"),
            Mode::Sovereign => f.write_str("sovereign"),
        }
    }
}

/// How the agent authenticates to its endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Auth {
    /// A static API key (from `api_key_env` or `api_key`), or none for local.
    #[default]
    ApiKey,
    /// xAI account sign-in: bearer tokens from `wizard login xai`, refreshed
    /// automatically. No key in the config file.
    XaiOauth,
}

/// Cosmetic UI settings carried in the config file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UiConfig {
    /// Gerund verbs shown next to the busy spinner ("Conjuring…"). A
    /// non-empty list replaces [`UiConfig::DEFAULT_SPINNER_VERBS`]; missing
    /// or empty keeps the defaults.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spinner_verbs: Vec<String>,
}

impl UiConfig {
    /// Baked-in magic-flavored spinner verbs, used when `spinner_verbs`
    /// is unset or empty.
    pub const DEFAULT_SPINNER_VERBS: [&'static str; 20] = [
        "Conjuring",
        "Scrying",
        "Brewing",
        "Transmuting",
        "Enchanting",
        "Divining",
        "Summoning",
        "Incanting",
        "Channeling",
        "Bewitching",
        "Alchemizing",
        "Spellweaving",
        "Polymorphing",
        "Wandwaving",
        "Hexing",
        "Levitating",
        "Crystal-gazing",
        "Runereading",
        "Familiar-consulting",
        "Grimoire-flipping",
    ];

    /// Pick a spinner verb for the given seed: deterministic per seed, spread
    /// across the active list (custom when non-empty, defaults otherwise).
    pub fn spinner_verb(&self, seed: u64) -> &str {
        let roll = splitmix64(seed);
        if self.spinner_verbs.is_empty() {
            Self::DEFAULT_SPINNER_VERBS[(roll % Self::DEFAULT_SPINNER_VERBS.len() as u64) as usize]
        } else {
            &self.spinner_verbs[(roll % self.spinner_verbs.len() as u64) as usize]
        }
    }
}

/// Settings for driving AHE's real harness-evolution loop (`evolve.py`) from
/// `wizard evolve …` and the `/evolve` slash command. Optional: evolve is OFF
/// unless an `[evolve]` section with an `ahe_repo` is present.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolveConfig {
    /// Path to an Agentic-Harness-Engineering checkout. Wizard launches its
    /// `scripts/evolve.sh`; AHE owns its own `.env` (LLM keys) and `configs/`.
    /// AHE runs harnesses on the **local Docker daemon** — no cloud sandbox.
    pub ahe_repo: PathBuf,
    /// Experiment config to run, as a path relative to `ahe_repo` (or an
    /// absolute path).
    #[serde(default = "default_experiment_config")]
    pub experiment_config: String,
}

/// Default experiment config: AHE's fully-local Docker smoke-test experiment
/// (one trivial task, no cloud sandbox).
fn default_experiment_config() -> String {
    "configs/experiments/exp-local-sample.yaml".to_string()
}

/// Default wire API: chat/completions, which the widest range of endpoints
/// (local llama.cpp, xAI, OpenAI, OpenRouter, DeepSeek) support. NexAU's other
/// accepted values are `openai_responses`, `anthropic_chat_completion`, and
/// `gemini_rest`.
fn default_api_type() -> String {
    "openai_chat_completion".to_string()
}

/// A fast, well-distributed integer hash for deterministic per-seed picks.
fn splitmix64(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Wizard's whole configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// `LLM_MODEL` for the NexAU agent.
    pub model: String,
    /// OpenAI-compatible base URL the agent calls (`LLM_BASE_URL`).
    pub base_url: String,
    /// Wire API the agent speaks (`LLM_API_TYPE`): `openai` (chat/completions,
    /// broadly compatible incl. local llama.cpp) or `openai_responses`.
    #[serde(default = "default_api_type")]
    pub api_type: String,
    /// Environment variable the API key is read from at launch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// API key stored directly in the config (used when no env var is set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Directory the agent's tools operate in (`SANDBOX_WORK_DIR`).
    pub workdir: PathBuf,
    /// How the agent authenticates (static key vs. xAI OAuth sign-in).
    #[serde(default)]
    pub auth: Auth,
    /// Personality mode.
    #[serde(default)]
    pub mode: Mode,
    /// Python interpreter for the bridge. Defaults to `<repo>/.venv/bin/python`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python: Option<PathBuf>,
    /// Bridge script. Defaults to `<repo>/backend/nexau_bridge.py`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bridge_script: Option<PathBuf>,
    /// Cosmetic UI settings.
    #[serde(default)]
    pub ui: UiConfig,
    /// Optional AHE harness-evolution driver settings. Absent = `/evolve` and
    /// `wizard evolve` report that evolve is unconfigured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evolve: Option<EvolveConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: "gpt-4o".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            api_type: default_api_type(),
            api_key_env: Some("LLM_API_KEY".to_string()),
            api_key: None,
            workdir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            auth: Auth::ApiKey,
            mode: Mode::Genie,
            python: None,
            bridge_script: None,
            ui: UiConfig::default(),
            evolve: None,
        }
    }
}

impl Config {
    /// `~/.wizard`, created if missing.
    pub fn wizard_dir() -> Result<PathBuf> {
        let dir = dirs::home_dir()
            .context("could not resolve the home directory")?
            .join(".wizard");
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(dir)
    }

    /// `~/.wizard/config.toml`.
    pub fn path() -> Result<PathBuf> {
        Ok(Self::wizard_dir()?.join("config.toml"))
    }

    /// The crate checkout, used to locate the bundled `.venv` and bridge
    /// script by default.
    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    /// Load the config from disk. `Ok(None)` when no config file exists yet.
    pub fn load() -> Result<Option<Self>> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        match toml::from_str::<Config>(&raw) {
            Ok(config) => Ok(Some(config)),
            // An incompatible file (e.g. a pre-NexAU wizard config) is treated
            // as absent so setup re-runs, rather than hard-failing at launch.
            Err(err) => {
                eprintln!(
                    "warning: ignoring incompatible config at {} ({err}); run setup to recreate it",
                    path.display()
                );
                Ok(None)
            }
        }
    }

    /// Persist the config to `~/.wizard/config.toml`.
    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        let raw = toml::to_string_pretty(self).context("serializing config")?;
        std::fs::write(&path, raw).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// True when the endpoint is a local server that needs no API key.
    fn is_local(&self) -> bool {
        let url = self.base_url.to_ascii_lowercase();
        url.contains("127.0.0.1") || url.contains("localhost") || url.contains("0.0.0.0")
    }

    /// True when the agent authenticates via xAI OAuth rather than a key.
    pub fn is_oauth(&self) -> bool {
        self.auth == Auth::XaiOauth
    }

    /// Whether the config can authenticate: a signed-in xAI session, a local
    /// endpoint, or a key that resolves to something non-empty. Drives the
    /// auto-onboarding fallback so an unauthenticated config opens setup
    /// instead of hard-failing.
    pub fn has_usable_auth(&self) -> bool {
        match self.auth {
            Auth::XaiOauth => crate::auth::xai_oauth::is_logged_in(),
            Auth::ApiKey => self.is_local() || !self.resolve_api_key().is_empty(),
        }
    }

    /// Resolve the API key: the env var named by `api_key_env` when set and
    /// non-empty, otherwise the stored `api_key`, otherwise empty.
    fn resolve_api_key(&self) -> String {
        if let Some(var) = &self.api_key_env
            && let Ok(value) = std::env::var(var)
            && !value.trim().is_empty()
        {
            return value;
        }
        self.api_key.clone().unwrap_or_default()
    }

    /// Default Python interpreter: the config override, else the crate's
    /// bundled virtualenv.
    fn python_path(&self) -> PathBuf {
        self.python
            .clone()
            .unwrap_or_else(|| Self::repo_root().join(".venv").join("bin").join("python"))
    }

    /// Default bridge script: the config override, else the crate's
    /// `backend/nexau_bridge.py`.
    fn bridge_script_path(&self) -> PathBuf {
        self.bridge_script
            .clone()
            .unwrap_or_else(|| Self::repo_root().join("backend").join("nexau_bridge.py"))
    }

    /// The AHE evolve settings when configured, or a clear error pointing the
    /// user at the `[evolve]` section they must add.
    pub fn evolve_ready(&self) -> Result<&EvolveConfig> {
        self.evolve.as_ref().context(
            "evolve is not configured — add an [evolve] section with an `ahe_repo` \
             pointing at an Agentic-Harness-Engineering checkout to ~/.wizard/config.toml",
        )
    }

    /// Everything the bridge subprocess needs to launch.
    pub fn bridge_config(&self) -> Result<BridgeConfig> {
        let log_path = Self::wizard_dir()?.join("logs").join("bridge.log");
        Ok(BridgeConfig {
            python: self.python_path(),
            script: self.bridge_script_path(),
            workdir: self.workdir.clone(),
            model: self.model.clone(),
            base_url: self.base_url.clone(),
            api_type: self.api_type.clone(),
            api_key: self.resolve_api_key(),
            log_path,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spinner_verbs_default_when_empty() {
        let ui = UiConfig::default();
        assert!(UiConfig::DEFAULT_SPINNER_VERBS.contains(&ui.spinner_verb(7)));
    }

    #[test]
    fn spinner_verbs_custom_list_replaces_defaults() {
        let ui = UiConfig {
            spinner_verbs: vec!["Pondering".to_string(), "Musing".to_string()],
        };
        let verb = ui.spinner_verb(3);
        assert!(verb == "Pondering" || verb == "Musing");
    }

    #[test]
    fn spinner_verb_is_deterministic_per_seed() {
        let ui = UiConfig::default();
        assert_eq!(ui.spinner_verb(42), ui.spinner_verb(42));
    }

    #[test]
    fn config_round_trips_through_toml() {
        let config = Config {
            model: "grok-4".to_string(),
            base_url: "https://api.x.ai/v1".to_string(),
            api_type: default_api_type(),
            api_key_env: Some("XAI_API_KEY".to_string()),
            api_key: None,
            workdir: PathBuf::from("/tmp/work"),
            auth: Auth::ApiKey,
            mode: Mode::Sovereign,
            python: None,
            bridge_script: None,
            ui: UiConfig::default(),
            evolve: None,
        };
        let raw = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&raw).unwrap();
        assert_eq!(parsed.model, config.model);
        assert_eq!(parsed.base_url, config.base_url);
        assert_eq!(parsed.mode, Mode::Sovereign);
        assert_eq!(parsed.workdir, config.workdir);
    }

    #[test]
    fn evolve_ready_errors_when_unconfigured() {
        let config = Config::default();
        let err = config.evolve_ready().unwrap_err().to_string();
        assert!(err.contains("[evolve]"), "got: {err}");
        assert!(err.contains("ahe_repo"), "got: {err}");
    }

    #[test]
    fn evolve_section_round_trips_through_toml() {
        let toml_src = r#"
            model = "gpt-4o"
            base_url = "https://api.openai.com/v1"
            workdir = "/tmp/work"

            [evolve]
            ahe_repo = "/srv/ahe"
        "#;
        let config: Config = toml::from_str(toml_src).unwrap();
        let evolve = config.evolve_ready().unwrap();
        assert_eq!(evolve.ahe_repo, PathBuf::from("/srv/ahe"));
        // experiment_config falls back to the baked-in default.
        assert_eq!(evolve.experiment_config, default_experiment_config());
    }

    #[test]
    fn bridge_config_resolves_key_from_env() {
        // SAFETY: single-threaded test process for this var.
        unsafe { std::env::set_var("WIZARD_TEST_KEY", "secret") };
        let config = Config {
            api_key_env: Some("WIZARD_TEST_KEY".to_string()),
            ..Config::default()
        };
        let bridge = config.bridge_config().unwrap();
        assert_eq!(bridge.api_key, "secret");
        unsafe { std::env::remove_var("WIZARD_TEST_KEY") };
    }
}
