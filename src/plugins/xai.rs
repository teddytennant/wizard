//! xAI (Grok), as a plugin: two kinds — `xai` with a plain API key,
//! `xaioauth` with an account sign-in — behind `--features provider-xai`.
//!
//! # Why this file is forty lines when `xai_oauth.rs` is a thousand
//!
//! Because the transport is not xAI's. Both kinds speak OpenAI-compatible
//! Chat Completions under the `xai` vendor label, so the client is
//! [`crate::llm::wire::OpenAiProvider`] — core infrastructure, shared with
//! five other backends — and all this plugin supplies is which token source
//! to hand it. There is no xAI-shaped protocol to move.
//!
//! # Why the token store and the sign-in stayed in core
//!
//! [`crate::llm::xai_oauth`] holds the OAuth flow, the token file under
//! `~/.wizard`, the refresh-with-a-lock machinery and [`XaiTokenSource`], and
//! it is *core*, not part of this plugin. That reads backwards until you count
//! the callers: five of the six are not this file.
//!
//! * `plugins/web.rs` authenticates xAI's server-side **search** API with that
//!   token, and `web_search` is a core tool that reaches for xAI whatever the
//!   configured chat backend is;
//! * `tools/image.rs` does the same for xAI's **image** API, which is the
//!   default image endpoint even when the active provider is llama.cpp;
//! * `sync.rs` includes the token file in the set it backs up;
//! * onboarding and `app/prompts.rs` ask whether a session exists, to decide
//!   what to offer;
//! * `--login xai`, `/login xai` and the GUI's sign-in sheet drive the flow.
//!
//! So the token store is a credential subsystem that a chat provider happens
//! to be one consumer of, and moving it here would mean a build without
//! `provider-xai` lost web search and image generation — two tools that have
//! nothing to do with which model answers a turn. What is provider-shaped
//! about xAI is exactly what is below: two descriptors saying which credential
//! goes with which kind.
//!
//! A build compiled without this feature still signs in, still searches, still
//! generates images, and answers `kind = "xai"` with the named error.

use std::sync::Arc;

use anyhow::Context;

use crate::kernel::{Capability, Ctx, Plugin, PluginManifest};
use crate::llm::registry::{Credentials, ProviderDescriptor, ProviderKind};
use crate::llm::wire::{OpenAiProvider, StaticToken};
use crate::llm::xai_oauth::{DEFAULT_KEY_ENV, XaiTokenSource};

/// How `kind = "xai"` is registered — the plain-API-key flavor.
pub fn key_descriptor() -> ProviderDescriptor {
    ProviderDescriptor::new(
        ProviderKind::XAI,
        "xAI",
        Credentials::ApiKey {
            default_env: Some(DEFAULT_KEY_ENV.to_string()),
        },
        |config| {
            Ok(Arc::new(OpenAiProvider::with_token_source(
                config.base_url.clone(),
                config.model.clone(),
                Arc::new(StaticToken::new(config.api_key())),
                "xai",
            )))
        },
    )
}

/// How `kind = "xaioauth"` is registered — the account sign-in flavor, whose
/// credential is the token store rather than a key.
pub fn oauth_descriptor() -> ProviderDescriptor {
    ProviderDescriptor::new(
        ProviderKind::XAI_OAUTH,
        "xAI",
        Credentials::Account {
            login: "xai".to_string(),
        },
        |config| {
            let source = XaiTokenSource::new().context("setting up xAI OAuth token storage")?;
            Ok(Arc::new(OpenAiProvider::with_token_source(
                config.base_url.clone(),
                config.model.clone(),
                Arc::new(source),
                "xai",
            )))
        },
    )
}

/// xAI as a kernel plugin, registering both of its kinds.
///
/// One plugin rather than two features because the two kinds differ only in
/// where the bearer token comes from: same endpoint, same wire shape, same
/// vendor label, forty lines between them. A `provider-xai-oauth` feature
/// would be a build flag whose entire content is `Credentials::Account`.
pub struct XaiPlugin {
    manifest: PluginManifest,
}

impl XaiPlugin {
    pub fn new() -> Self {
        Self {
            manifest: PluginManifest {
                name: "xai".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                description: "xAI Grok, by API key or account sign-in".to_string(),
                capabilities: vec![Capability::Network],
                optional_deps: Vec::new(),
                profiles: vec!["full".to_string(), "server".to_string()],
            },
        }
    }
}

impl Default for XaiPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for XaiPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn apply(&self, ctx: &mut Ctx) -> anyhow::Result<()> {
        ctx.provider(key_descriptor())?;
        ctx.provider(oauth_descriptor())?;
        Ok(())
    }
}
