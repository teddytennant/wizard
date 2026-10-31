//! Fast mode: the Anthropic provider's higher-speed tier, switched with
//! `/fast`, `fastMode` in settings or `CLAUDE_CODE_DISABLE_FAST_MODE`.
//!
//! The switch is process-wide because `/fast` flips it mid-session and the
//! provider reads it on every request. No other provider has a speed tier, so
//! on those the switch is stored and does nothing.

use std::sync::atomic::{AtomicBool, Ordering};

static FAST: AtomicBool = AtomicBool::new(false);

/// The beta header value that turns the tier on.
pub const BETA: &str = "fast-mode-2026-02-01";

pub fn enabled() -> bool {
    FAST.load(Ordering::Relaxed)
}

pub fn set(on: bool) {
    FAST.store(on, Ordering::Relaxed);
}

/// Start the session from the settings: `fastMode` turns it on unless
/// `fastModePerSessionOptIn` says each session starts off, or
/// `CLAUDE_CODE_DISABLE_FAST_MODE` forbids it.
pub fn init(settings: &super::claude::ClaudeSettings, get: super::claude_env::Get) {
    let disabled = super::claude_env::truthy(get, "CLAUDE_CODE_DISABLE_FAST_MODE");
    let wanted = settings.fast_mode == Some(true) && !settings.fast_mode_per_session_opt_in;
    set(wanted && !disabled);
}

/// Whether a model has the tier. Only Opus does.
pub fn applies_to(model: &str) -> bool {
    model.to_ascii_lowercase().contains("opus")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_opus_has_the_tier() {
        assert!(applies_to("claude-opus-4-6"));
        assert!(!applies_to("claude-sonnet-4-5"));
    }

    #[test]
    fn settings_and_the_environment_decide_the_starting_state() {
        let on = |opt_in: bool| {
            let mut settings = super::super::claude::ClaudeSettings::default();
            settings.fast_mode = Some(true);
            settings.fast_mode_per_session_opt_in = opt_in;
            settings
        };
        let none = |_: &str| None;
        let disabled = |name: &str| (name == "CLAUDE_CODE_DISABLE_FAST_MODE").then(|| "1".into());
        init(&on(false), &none);
        assert!(enabled());
        init(&on(true), &none);
        assert!(!enabled(), "per-session opt-in starts off");
        init(&on(false), &disabled);
        assert!(!enabled());
    }
}
