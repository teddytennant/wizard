//! The one table that names every compiled-in provider.
//!
//! This is the provider half of the file `docs/plugins.md` describes as
//! "`src/plugins/mod.rs` holds the one table mapping feature to constructor;
//! it is the only file that names every Rust plugin". It exists so that the
//! rest of core does not: `config.rs` used to import nine concrete provider
//! types to construct them, and now imports none. This file names the ones
//! that are not plugins yet, and it names them to ask each for its descriptor
//! — never to construct one.
//!
//! Keeping it as a separate module from [`super::registry`] is the point. The
//! registry is core and knows nothing about which providers exist; this table
//! is the seam that gets deleted one line at a time, as each provider becomes
//! a plugin that calls `Ctx::provider` for itself. When the last line goes, so
//! does the file.
//!
//! One line has gone. Anthropic is [`crate::plugins::anthropic`] — behind
//! `--features provider-anthropic`, registered through `Ctx::provider` at
//! kernel boot, and absent from a build that leaves the feature out. The eight
//! below are still compiled in unconditionally and registered eagerly, which
//! is why [`SHIPPED`] is the *floor* a build answers to rather than the whole
//! of it: what a build actually installs is this table plus whichever plugins
//! it was compiled with, and only [`super::registry::kinds`] knows that.

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

/// The kinds compiled in unconditionally, whatever features a build carries.
///
/// Named as data so tests can assert the table is complete without repeating
/// it. Deliberately not "the kinds this build ships": a provider plugin adds
/// to that set and is not listed here, because listing it here is exactly the
/// naming this file exists to stop doing.
pub const SHIPPED: [ProviderKind; 8] = [
    ProviderKind::LLAMACPP,
    ProviderKind::OLLAMA,
    ProviderKind::OPENAI,
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

    /// Every kind in the table is installed and usable. A subset assertion
    /// rather than an equality one, because a plugin provider is installed too
    /// and is not in the table — `plugins::anthropic_is_present_exactly_when_its_feature_is`
    /// is the other half, and between them they cover the nine kinds a stock
    /// build answers to, which is the whole back-compat claim.
    #[test]
    fn every_shipped_provider_is_installed() {
        let installed = registry::kinds();
        for kind in SHIPPED {
            assert!(installed.contains(&kind), "{kind}");
        }
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
