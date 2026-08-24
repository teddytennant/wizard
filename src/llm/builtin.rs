//! The one table that names every compiled-in provider.
//!
//! This is the provider half of the file `docs/plugins.md` describes as
//! "`src/plugins/mod.rs` holds the one table mapping feature to constructor;
//! it is the only file that names every Rust plugin". It exists so that the
//! rest of core does not: `config.rs` used to import nine concrete provider
//! types to construct them, and now imports none. This is the only file left
//! that names all nine, and it names them to ask each for its descriptor —
//! never to construct one.
//!
//! Keeping it as a separate module from [`super::registry`] is the point. The
//! registry is core and knows nothing about which providers exist; this table
//! is the seam the next phase deletes, one line at a time, as each provider
//! becomes a plugin that calls `Ctx::provider` for itself. When the last line
//! goes, so does the file.
//!
//! No cargo features here yet, deliberately. Every provider is compiled in and
//! registered eagerly, exactly as before this change — the enum is open now,
//! and nothing has moved through the door.

use super::registry::{ProviderKind, ProviderRegistry};

/// The registry every `kind = "..."` resolves against at startup.
///
/// Seeded lazily on first use rather than from `main`, because
/// `ProviderConfig::build` is reachable from unit tests that never run a
/// `main` and from `wizard doctor` before any kernel exists. "Eagerly at
/// startup" and "on the first lookup" are the same thing when the first
/// lookup is the first thing startup does.
pub(super) fn registry() -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();
    for descriptor in [
        super::llamacpp::descriptor(),
        super::ollama::descriptor(),
        super::openai::descriptor(),
        super::anthropic::descriptor(),
        super::openrouter::descriptor(),
        super::xai_oauth::key_descriptor(),
        super::xai_oauth::oauth_descriptor(),
        super::chatgpt_oauth::descriptor(),
        super::cloudflare::descriptor(),
    ] {
        // A duplicate here is a bug in this file, not a user's problem, and it
        // would silently drop a provider users have configured. Panicking at
        // the first lookup is the only way it gets noticed.
        let kind = descriptor.kind().clone();
        registry
            .insert(descriptor)
            .unwrap_or_else(|err| panic!("built-in provider '{kind}': {err}"));
    }
    registry
}

/// The kinds this build ships. Named as data so tests can assert the table is
/// complete without repeating it.
pub const SHIPPED: [ProviderKind; 9] = [
    ProviderKind::LLAMACPP,
    ProviderKind::OLLAMA,
    ProviderKind::OPENAI,
    ProviderKind::ANTHROPIC,
    ProviderKind::OPENROUTER,
    ProviderKind::XAI,
    ProviderKind::XAI_OAUTH,
    ProviderKind::CHATGPT_OAUTH,
    ProviderKind::CLOUDFLARE,
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::registry;

    /// The nine kinds that were enum variants are still the nine kinds a
    /// stock build answers to. This is the whole back-compat claim in one
    /// assertion.
    #[test]
    fn every_shipped_provider_is_installed() {
        let mut expected = SHIPPED.to_vec();
        expected.sort();
        assert_eq!(registry::kinds(), expected);
        for kind in SHIPPED {
            let descriptor = registry::installed(&kind).expect("installed");
            assert_eq!(descriptor.kind(), &kind);
            assert!(!descriptor.display_name().is_empty(), "{kind}");
        }
    }

    /// Exactly one backend has a process `/server` manages. Ollama runs
    /// locally too and must not be caught by that flag.
    #[test]
    fn only_llamacpp_owns_a_local_server() {
        for kind in SHIPPED {
            let descriptor = registry::installed(&kind).expect("installed");
            assert_eq!(
                descriptor.manages_local_server(),
                kind == ProviderKind::LLAMACPP,
                "{kind}"
            );
        }
    }
}
