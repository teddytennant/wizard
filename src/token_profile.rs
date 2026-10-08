//! Token profile: how much of the harness goes into every request.
//!
//! Picked by `--token-profile`, then `WIZARD_TOKEN_PROFILE`, then the
//! `token_profile` config key; `safe` when none is set. `stock` is the
//! harness as it was through 3.2. Each later profile includes everything the
//! one before it does:
//!
//! - `safe` fixes skill frontmatter parsing, sends the subagent roster as
//!   names only, sends a continuous run's mission once per compaction, stubs
//!   an identical re-read of an unchanged file, stops repeating a subagent
//!   result the completion note already delivered, puts MCP tools behind
//!   `tool_search`, and digests old tool-call arguments.
//! - `lean` defers every tool outside the everyday set behind `tool_search`
//!   and caps `execute` output at 12 KB.
//! - `min` cuts the system prompt and core tool schemas to the bone.

use std::sync::OnceLock;

/// Environment variable that picks the profile.
pub const ENV: &str = "WIZARD_TOKEN_PROFILE";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum TokenProfile {
    Stock,
    #[default]
    Safe,
    Lean,
    Min,
}

impl TokenProfile {
    /// Parse a profile name. Unknown names are `None`.
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "stock" => Some(Self::Stock),
            "safe" => Some(Self::Safe),
            "lean" => Some(Self::Lean),
            "min" => Some(Self::Min),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stock => "stock",
            Self::Safe => "safe",
            Self::Lean => "lean",
            Self::Min => "min",
        }
    }

    /// `safe` and everything above it.
    pub fn safe(self) -> bool {
        self >= Self::Safe
    }

    /// `lean` and everything above it.
    pub fn lean(self) -> bool {
        self >= Self::Lean
    }

    /// `min` only.
    pub fn min(self) -> bool {
        self >= Self::Min
    }
}

static CURRENT: OnceLock<TokenProfile> = OnceLock::new();

/// Resolve the profile from the environment (where `--token-profile` has
/// already been written) and then `configured`, the config key. An unknown
/// name falls back to the default with a warning. Only the first call
/// decides; later ones return what it chose.
pub fn init(configured: Option<&str>) -> TokenProfile {
    *CURRENT.get_or_init(|| resolve(std::env::var(ENV).ok().as_deref(), configured))
}

fn resolve(env: Option<&str>, configured: Option<&str>) -> TokenProfile {
    let (source, raw) = match (env.map(str::trim), configured.map(str::trim)) {
        (Some(raw), _) if !raw.is_empty() => (ENV, raw),
        (_, Some(raw)) if !raw.is_empty() => ("token_profile", raw),
        _ => return TokenProfile::default(),
    };
    TokenProfile::parse(raw).unwrap_or_else(|| {
        let fallback = TokenProfile::default();
        tracing::warn!(
            "{source}={raw:?} is not stock, safe, lean or min; using {}",
            fallback.as_str()
        );
        fallback
    })
}

/// The process's profile. Resolved from the environment alone if nothing
/// has called [`init`] with the config yet.
pub fn current() -> TokenProfile {
    init(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names_and_orders_profiles() {
        assert_eq!(TokenProfile::parse("stock"), Some(TokenProfile::Stock));
        assert_eq!(TokenProfile::parse("Lean"), Some(TokenProfile::Lean));
        assert_eq!(TokenProfile::parse("nope"), None);
        assert!(TokenProfile::Min.lean() && TokenProfile::Min.safe());
        assert!(TokenProfile::Lean.safe() && !TokenProfile::Lean.min());
        assert!(!TokenProfile::Stock.safe());
    }

    #[test]
    fn the_flag_or_env_wins_over_config_and_safe_is_the_default() {
        assert_eq!(resolve(None, None), TokenProfile::Safe);
        assert_eq!(resolve(Some(""), Some(" ")), TokenProfile::Safe);
        assert_eq!(resolve(None, Some("stock")), TokenProfile::Stock);
        assert_eq!(resolve(Some("min"), Some("stock")), TokenProfile::Min);
        assert_eq!(resolve(Some("typo"), Some("stock")), TokenProfile::Safe);
    }
}
