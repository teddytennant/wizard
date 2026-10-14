//! What `/ui` carries from one look to the next: the conversation, and the
//! model and mode it runs on. A full look is another process, so the house
//! TUI hands it over in [`ENV`], and a look's `wizard acp` leaves it in a file
//! for the `wizard` that started the look to pick up when it quits.

use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::{Config, Mode, ReasoningEffort};

/// Set on a look's process. Its ACP client resumes `session`, and its
/// `wizard acp` starts sessions on the rest.
pub const ENV: &str = "WIZARD_HANDOFF";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handoff {
    /// `None` when nothing was said yet: there is nothing to resume.
    pub session: Option<String>,
    pub provider: String,
    pub model: String,
    pub mode: Mode,
    #[serde(default)]
    pub effort: Option<ReasoningEffort>,
}

impl Handoff {
    fn path() -> Result<PathBuf> {
        Ok(Config::wizard_dir()?.join("ui-handoff.json"))
    }

    /// Leave this for the `wizard` that started the look.
    pub fn save(&self) -> Result<()> {
        std::fs::write(Self::path()?, serde_json::to_vec(self)?)?;
        Ok(())
    }

    /// What a look left, removing it so it is used once.
    pub fn take() -> Option<Self> {
        let path = Self::path().ok()?;
        let text = std::fs::read(&path).ok()?;
        let _ = std::fs::remove_file(&path);
        serde_json::from_slice(&text).ok()
    }

    pub fn from_env() -> Option<Self> {
        serde_json::from_str(&std::env::var(ENV).ok()?).ok()
    }

    pub fn to_env(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// [`Self::apply`], keeping what it replaced so a save can put it back.
    pub fn overlay(&self, config: &mut Config) -> Overlay {
        let before = config.clone();
        self.apply(config);
        Overlay {
            before,
            after: config.clone(),
        }
    }

    /// `config` running on this provider, model, mode and effort. A provider
    /// that is no longer configured leaves `config` as it was.
    pub fn apply(&self, config: &mut Config) {
        let Some(provider) = config
            .providers
            .iter_mut()
            .find(|provider| provider.name == self.provider)
        else {
            return;
        };
        provider.model = self.model.clone();
        config.active_provider = Some(self.provider.clone());
        config.mode = self.mode;
        config.reasoning_effort = self.effort;
    }
}

/// What a handoff changed in a session's config. The handed-over provider,
/// model, mode and effort belong to the session, not to `config.toml`: a save
/// writes the user's own values back, except for a setting changed since.
#[derive(Debug, Clone)]
pub struct Overlay {
    before: Config,
    after: Config,
}

impl Overlay {
    /// `config` as it should be saved.
    pub fn strip(&self, config: &Config) -> Config {
        let mut saved = config.clone();
        if saved.active_provider == self.after.active_provider {
            saved.active_provider = self.before.active_provider.clone();
        }
        if saved.mode == self.after.mode {
            saved.mode = self.before.mode;
        }
        if saved.reasoning_effort == self.after.reasoning_effort {
            saved.reasoning_effort = self.before.reasoning_effort;
        }
        for provider in &mut saved.providers {
            let model = |config: &Config| {
                config
                    .providers
                    .iter()
                    .find(|p| p.name == provider.name)
                    .map(|p| p.model.clone())
            };
            if let (Some(after), Some(before)) = (model(&self.after), model(&self.before))
                && provider.model == after
            {
                provider.model = before;
            }
        }
        saved
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handoff() -> Handoff {
        Handoff {
            session: Some("2026-10-14T20-45-38".into()),
            provider: "beta".into(),
            model: "beta-other".into(),
            mode: Mode::Sovereign,
            effort: Some(ReasoningEffort::High),
        }
    }

    #[test]
    fn a_saved_handoff_is_taken_once() {
        handoff().save().expect("save");
        assert_eq!(Handoff::take(), Some(handoff()));
        assert_eq!(Handoff::take(), None);
    }

    #[test]
    fn it_round_trips_through_the_environment_value() {
        let value = handoff().to_env();
        assert_eq!(
            serde_json::from_str::<Handoff>(&value).ok(),
            Some(handoff())
        );
    }

    #[test]
    fn applying_switches_to_a_configured_provider_only() {
        let provider = |name: &str| crate::config::ProviderConfig {
            name: name.into(),
            kind: crate::config::ProviderKind::OPENAI,
            base_url: "http://127.0.0.1:9/v1".into(),
            model: format!("{name}-model"),
            api_key_env: None,
            gguf_path: None,
            usd_per_mtok_in: None,
            usd_per_mtok_out: None,
            vision: None,
            model_pinned: None,
        };
        let mut config = Config {
            providers: vec![provider("alpha"), provider("beta")],
            active_provider: Some("alpha".into()),
            ..Config::default()
        };
        handoff().apply(&mut config);
        let active = config.active();
        assert_eq!(
            (active.name.as_str(), active.model.as_str()),
            ("beta", "beta-other")
        );
        assert_eq!(config.mode, Mode::Sovereign);
        assert_eq!(config.reasoning_effort, Some(ReasoningEffort::High));

        let mut gone = Handoff {
            provider: "gamma".into(),
            ..handoff()
        };
        gone.mode = Mode::Chat;
        let mut config = Config {
            providers: vec![provider("alpha")],
            active_provider: Some("alpha".into()),
            ..Config::default()
        };
        gone.apply(&mut config);
        assert_eq!(config.active().name, "alpha");
        assert_eq!(config.mode, Mode::default());
    }
}
