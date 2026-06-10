//! User configuration: `~/.wizard/config.toml` plus env overrides and
//! well-known paths under `~/.wizard/` (see "Data on disk" in
//! `docs/architecture.md`).

use std::fmt;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::cli::Cli;

/// Personality mode. Shares tools and model; differs in prompting,
/// temperature, step budget, and confirmation behavior (`docs/modes.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum Mode {
    /// Interactive TUI. Confirms risky actions (writes, shell, git).
    #[default]
    Genie,
    /// Autonomous agent. Auto-approves all tool calls.
    Sovereign,
}

impl Mode {
    /// Sampling temperature for this mode (genie 0.8, sovereign 0.6).
    pub fn temperature(self) -> f32 {
        match self {
            Mode::Genie => 0.8,
            Mode::Sovereign => 0.6,
        }
    }

    /// Default agent-loop step budget per turn (genie 25, sovereign 100).
    pub fn default_max_steps(self) -> u32 {
        match self {
            Mode::Genie => 25,
            Mode::Sovereign => 100,
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Mode::Genie => write!(f, "genie"),
            Mode::Sovereign => write!(f, "sovereign"),
        }
    }
}

/// Contents of `~/.wizard/config.toml`. Unknown keys are ignored; missing
/// keys take the documented defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Ollama model tag (default `qwen3.6:27b`).
    pub model: String,
    /// Base URL of the Ollama server.
    pub ollama_host: String,
    /// Default personality mode.
    pub mode: Mode,
    /// Skip confirmation prompts in genie mode.
    pub auto_approve: bool,
    /// Agent loop limit per turn (genie). Sovereign uses its own default
    /// unless this is explicitly raised above it.
    pub max_steps: u32,
    /// Perpetual sovereign operation: keep working/self-directing/self-improving
    /// until stopped.
    pub continuous: bool,
    /// Base seconds for exponential backoff when the LLM server is unreachable
    /// or rate-limited.
    pub retry_base_secs: u64,
    /// Cap on backoff sleep in seconds.
    pub retry_max_secs: u64,
    /// Pause between continuous cycles (0 = none).
    pub cycle_pause_secs: u64,
    /// When the serialized chat history exceeds this many bytes, compact older
    /// messages into a summary.
    pub compact_threshold_bytes: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: "qwen3.6:27b".to_string(),
            ollama_host: "http://127.0.0.1:11434".to_string(),
            mode: Mode::Genie,
            auto_approve: false,
            max_steps: 25,
            continuous: false,
            retry_base_secs: 5,
            retry_max_secs: 300,
            cycle_pause_secs: 0,
            compact_threshold_bytes: 48_000,
        }
    }
}

impl Config {
    /// `~/.wizard` — root of all Wizard state on disk.
    pub fn wizard_dir() -> Result<PathBuf> {
        let home = dirs::home_dir().context("could not determine home directory")?;
        Ok(home.join(".wizard"))
    }

    /// `~/.wizard/config.toml`
    pub fn path() -> Result<PathBuf> {
        Ok(Self::wizard_dir()?.join("config.toml"))
    }

    /// `~/.wizard/mcp.toml` — MCP server declarations.
    pub fn mcp_config_path() -> Result<PathBuf> {
        Ok(Self::wizard_dir()?.join("mcp.toml"))
    }

    /// `~/.wizard/sessions/` — JSONL chat history.
    pub fn sessions_dir() -> Result<PathBuf> {
        Ok(Self::wizard_dir()?.join("sessions"))
    }

    /// `~/.wizard/tools/` — agent-authored scripted tools.
    pub fn scripted_tools_dir() -> Result<PathBuf> {
        Ok(Self::wizard_dir()?.join("tools"))
    }

    /// `~/.wizard/skills/` — user/evolved skills (in addition to bundled ones).
    pub fn skills_dir() -> Result<PathBuf> {
        Ok(Self::wizard_dir()?.join("skills"))
    }

    /// `~/.wizard/subagents/` — user-defined subagent definitions (TOML).
    pub fn subagents_dir() -> Result<PathBuf> {
        Ok(Self::wizard_dir()?.join("subagents"))
    }

    /// `~/.wizard/src/` — source checkout for deep evolve.
    pub fn source_dir() -> Result<PathBuf> {
        Ok(Self::wizard_dir()?.join("src"))
    }

    /// `~/.wizard/evolution.jsonl` — self-extension log.
    pub fn evolution_log_path() -> Result<PathBuf> {
        Ok(Self::wizard_dir()?.join("evolution.jsonl"))
    }

    /// `~/.wizard/logs/` — debug traces.
    pub fn logs_dir() -> Result<PathBuf> {
        Ok(Self::wizard_dir()?.join("logs"))
    }

    /// Create the `~/.wizard` directory tree (sessions, tools, skills, logs)
    /// if it does not exist yet. Idempotent; called on every load so a fresh
    /// install is usable without running the installer.
    pub fn ensure_dirs() -> Result<()> {
        for dir in [
            Self::wizard_dir()?,
            Self::sessions_dir()?,
            Self::scripted_tools_dir()?,
            Self::skills_dir()?,
            Self::subagents_dir()?,
            Self::logs_dir()?,
        ] {
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        Ok(())
    }

    /// Load config from disk, falling back to defaults when the file is
    /// missing, then apply env overrides (`WIZARD_MODEL`,
    /// `WIZARD_OLLAMA_HOST`). Creates the `~/.wizard` directory tree on
    /// first run.
    pub fn load() -> Result<Self> {
        Self::ensure_dirs()?;

        let path = Self::path()?;
        let mut config = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?
        } else {
            Self::default()
        };
        config.apply_env();

        Ok(config)
    }

    /// Apply environment-variable overrides on top of file/default config.
    /// Empty values are ignored.
    fn apply_env(&mut self) {
        self.apply_env_from(|name| std::env::var(name).ok());
    }

    /// Testable core of [`apply_env`]: `lookup` supplies the value of an
    /// environment variable, or `None` when unset.
    fn apply_env_from(&mut self, lookup: impl Fn(&str) -> Option<String>) {
        if let Some(model) = lookup("WIZARD_MODEL")
            && !model.trim().is_empty()
        {
            self.model = model.trim().to_string();
        }
        if let Some(host) = lookup("WIZARD_OLLAMA_HOST") {
            let host = host.trim().trim_end_matches('/');
            if !host.is_empty() {
                self.ollama_host = host.to_string();
            }
        }
    }

    /// Persist config to `~/.wizard/config.toml`, creating the directory if
    /// needed.
    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let raw = toml::to_string_pretty(self).context("serializing config")?;
        std::fs::write(&path, raw).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Apply CLI flag overrides on top of file/env config for this run.
    /// CLI mode wins; `--auto` forces `auto_approve`; sovereign mode raises
    /// `max_steps` to its default if the configured value is lower.
    pub fn apply_cli(&mut self, cli: &Cli) {
        if let Some(mode) = cli.mode {
            self.mode = mode;
        }
        if cli.continuous {
            self.mode = Mode::Sovereign;
            self.continuous = true;
        }
        if cli.auto || self.mode == Mode::Sovereign {
            self.auto_approve = true;
        }
        if self.mode == Mode::Sovereign && self.max_steps < Mode::Sovereign.default_max_steps() {
            self.max_steps = Mode::Sovereign.default_max_steps();
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    fn cli(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("wizard").chain(args.iter().copied()))
            .expect("valid args")
    }

    #[test]
    fn defaults_match_docs() {
        let config = Config::default();
        assert_eq!(config.model, "qwen3.6:27b");
        assert_eq!(config.ollama_host, "http://127.0.0.1:11434");
        assert_eq!(config.mode, Mode::Genie);
        assert!(!config.auto_approve);
        assert_eq!(config.max_steps, 25);
        assert!(!config.continuous);
        assert_eq!(config.retry_base_secs, 5);
        assert_eq!(config.retry_max_secs, 300);
        assert_eq!(config.cycle_pause_secs, 0);
        assert_eq!(config.compact_threshold_bytes, 48_000);
    }

    #[test]
    fn mode_parameters() {
        assert_eq!(Mode::Genie.temperature(), 0.8);
        assert_eq!(Mode::Sovereign.temperature(), 0.6);
        assert_eq!(Mode::Genie.default_max_steps(), 25);
        assert_eq!(Mode::Sovereign.default_max_steps(), 100);
        assert_eq!(Mode::Genie.to_string(), "genie");
        assert_eq!(Mode::Sovereign.to_string(), "sovereign");
    }

    #[test]
    fn missing_keys_take_defaults() {
        let config: Config = toml::from_str("model = \"qwen3.5:9b\"").expect("valid toml");
        assert_eq!(config.model, "qwen3.5:9b");
        assert_eq!(config.ollama_host, "http://127.0.0.1:11434");
        assert_eq!(config.mode, Mode::Genie);
        assert_eq!(config.max_steps, 25);
    }

    #[test]
    fn full_file_round_trips() {
        let original = Config {
            model: "llama3.3:70b".to_string(),
            ollama_host: "http://10.0.0.5:11434".to_string(),
            mode: Mode::Sovereign,
            auto_approve: true,
            max_steps: 200,
            continuous: true,
            retry_base_secs: 10,
            retry_max_secs: 600,
            cycle_pause_secs: 30,
            compact_threshold_bytes: 96_000,
        };
        let raw = toml::to_string_pretty(&original).expect("serialize");
        let parsed: Config = toml::from_str(&raw).expect("parse back");
        assert_eq!(parsed.model, original.model);
        assert_eq!(parsed.ollama_host, original.ollama_host);
        assert_eq!(parsed.mode, original.mode);
        assert_eq!(parsed.auto_approve, original.auto_approve);
        assert_eq!(parsed.max_steps, original.max_steps);
        assert_eq!(parsed.continuous, original.continuous);
        assert_eq!(parsed.retry_base_secs, original.retry_base_secs);
        assert_eq!(parsed.retry_max_secs, original.retry_max_secs);
        assert_eq!(parsed.cycle_pause_secs, original.cycle_pause_secs);
        assert_eq!(
            parsed.compact_threshold_bytes,
            original.compact_threshold_bytes
        );
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let config: Config =
            toml::from_str("model = \"m\"\nfuture_option = true").expect("valid toml");
        assert_eq!(config.model, "m");
    }

    #[test]
    fn env_overrides_model_and_host() {
        let mut config = Config::default();
        config.apply_env_from(|name| match name {
            "WIZARD_MODEL" => Some("  llama3.3:70b  ".to_string()),
            "WIZARD_OLLAMA_HOST" => Some("http://10.0.0.5:11434///".to_string()),
            _ => None,
        });
        assert_eq!(config.model, "llama3.3:70b", "model is trimmed");
        assert_eq!(
            config.ollama_host, "http://10.0.0.5:11434",
            "host trailing slashes are trimmed"
        );
    }

    #[test]
    fn env_unset_keeps_existing_values() {
        let mut config = Config::default();
        config.apply_env_from(|_| None);
        assert_eq!(config.model, "qwen3.6:27b");
        assert_eq!(config.ollama_host, "http://127.0.0.1:11434");
    }

    #[test]
    fn env_empty_values_are_ignored() {
        let mut config = Config::default();
        config.apply_env_from(|name| match name {
            "WIZARD_MODEL" => Some("   ".to_string()),
            "WIZARD_OLLAMA_HOST" => Some("".to_string()),
            _ => None,
        });
        assert_eq!(config.model, "qwen3.6:27b");
        assert_eq!(config.ollama_host, "http://127.0.0.1:11434");
    }

    #[test]
    fn cli_mode_overrides_config() {
        let mut config = Config::default();
        config.apply_cli(&cli(&["--mode", "sovereign"]));
        assert_eq!(config.mode, Mode::Sovereign);
        assert!(config.auto_approve, "sovereign implies auto-approve");
        assert_eq!(config.max_steps, 100, "sovereign raises the step budget");
    }

    #[test]
    fn continuous_flag_forces_sovereign() {
        let mut config = Config::default();
        config.apply_cli(&cli(&["--continuous"]));
        assert_eq!(config.mode, Mode::Sovereign);
        assert!(config.continuous);
        assert!(config.auto_approve);
        assert_eq!(config.max_steps, 100);
    }

    #[test]
    fn sovereign_keeps_explicitly_higher_max_steps() {
        let mut config = Config {
            max_steps: 250,
            ..Config::default()
        };
        config.apply_cli(&cli(&["--mode", "sovereign"]));
        assert_eq!(config.max_steps, 250);
    }

    #[test]
    fn auto_flag_forces_auto_approve_in_genie() {
        let mut config = Config::default();
        config.apply_cli(&cli(&["--auto"]));
        assert_eq!(config.mode, Mode::Genie);
        assert!(config.auto_approve);
        assert_eq!(config.max_steps, 25, "genie keeps its budget");
    }

    #[test]
    fn no_flags_leaves_config_untouched() {
        let mut config = Config::default();
        config.apply_cli(&cli(&[]));
        assert_eq!(config.mode, Mode::Genie);
        assert!(!config.auto_approve);
        assert_eq!(config.max_steps, 25);
    }

    #[test]
    fn config_sovereign_mode_implies_auto_approve_without_flags() {
        let mut config = Config {
            mode: Mode::Sovereign,
            ..Config::default()
        };
        config.apply_cli(&cli(&[]));
        assert!(config.auto_approve);
        assert_eq!(config.max_steps, 100);
    }
}
