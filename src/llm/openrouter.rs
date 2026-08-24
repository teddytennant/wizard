//! OpenRouter (`https://openrouter.ai`): hundreds of hosted models behind one
//! OpenAI-compatible Chat Completions endpoint and one API key.
//!
//! The wire protocol is handled by [`super::wire::OpenAiProvider`]; this
//! module only supplies the defaults (`kind = "openrouter"`) and the
//! attribution headers OpenRouter recommends on every request.

use std::sync::Arc;

use super::registry::{Credentials, ProviderDescriptor, ProviderKind};
use super::wire::{OpenAiProvider, StaticToken};

/// Default Chat Completions base URL.
pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
/// Default model: OpenRouter's Auto Router, which picks a model per prompt.
pub const DEFAULT_MODEL: &str = "openrouter/auto";
/// Default env var holding the OpenRouter API key.
pub const DEFAULT_KEY_ENV: &str = "OPENROUTER_API_KEY";
/// `HTTP-Referer` attribution header value (identifies Wizard to OpenRouter).
pub const ATTRIBUTION_REFERER: &str = "https://github.com/teddytennant/wizard";
/// `X-Title` attribution header value.
pub const ATTRIBUTION_TITLE: &str = "Wizard";

/// Build a fully-configured OpenRouter client: vendor `openrouter` (so the
/// label is `openrouter:<model>`) with the attribution headers attached.
pub fn provider(
    base_url: impl Into<String>,
    model: impl Into<String>,
    api_key: impl Into<String>,
) -> OpenAiProvider {
    OpenAiProvider::with_token_source(
        base_url,
        model,
        Arc::new(StaticToken::new(api_key)),
        "openrouter",
    )
    .with_headers(&[
        ("HTTP-Referer", ATTRIBUTION_REFERER),
        ("X-Title", ATTRIBUTION_TITLE),
    ])
}

/// How `kind = "openrouter"` is registered.
///
/// No missing-key warning, matching the old `match` arm exactly. OpenRouter
/// answers a keyless request with a usable error of its own, and the arm never
/// warned; the two arms that do warn are preserved just as precisely.
pub fn descriptor() -> ProviderDescriptor {
    ProviderDescriptor::new(
        ProviderKind::OPENROUTER,
        "OpenRouter",
        Credentials::ApiKey {
            default_env: Some(DEFAULT_KEY_ENV.to_string()),
        },
        |config| {
            Ok(Arc::new(provider(
                config.base_url.clone(),
                config.model.clone(),
                config.api_key(),
            )))
        },
    )
}

#[cfg(test)]
mod tests {
    use crate::llm::provider::LlmProvider;

    use super::*;

    #[test]
    fn label_uses_the_openrouter_vendor_prefix() {
        let provider = provider(DEFAULT_BASE_URL, DEFAULT_MODEL, "sk-or-test");
        assert_eq!(provider.label(), "openrouter:openrouter/auto");
    }
}
