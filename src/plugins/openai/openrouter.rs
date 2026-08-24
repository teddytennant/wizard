//! OpenRouter (`https://openrouter.ai`): hundreds of hosted models behind one
//! OpenAI-compatible Chat Completions endpoint and one API key.
//!
//! The wire protocol is handled by [`crate::llm::wire::OpenAiProvider`]; this
//! module only supplies `kind = "openrouter"` and the attribution headers
//! OpenRouter recommends on every request. The defaults it used to own — base
//! URL, model, key env var — are in `llm::registry::defaults`, because
//! onboarding and the settings sheet prefill a form with them and have to keep
//! doing so on a build compiled without this plugin.
//!
//! It ships inside `provider-openai` rather than under a feature of its own:
//! this is the `openai` kind with a fixed base URL and two headers, and a
//! cargo feature whose whole content is a `with_headers` call is a build
//! combination nobody wants and everybody has to test.

use std::sync::Arc;

use crate::llm::registry::{Credentials, ProviderDescriptor, ProviderKind};
use crate::llm::wire::{OpenAiProvider, StaticToken};

// The base URL, model and key env var are in `registry::defaults`: onboarding
// and the settings sheet prefill a form with them and have to keep doing so on
// a build compiled without this plugin. Core may hold the text; this file
// holds the transport and the attribution headers.
use crate::llm::registry::defaults::OPENROUTER_KEY_ENV as DEFAULT_KEY_ENV;

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
    use crate::llm::registry::defaults::{
        OPENROUTER_BASE_URL as DEFAULT_BASE_URL, OPENROUTER_MODEL as DEFAULT_MODEL,
    };

    use super::*;

    #[test]
    fn label_uses_the_openrouter_vendor_prefix() {
        let provider = provider(DEFAULT_BASE_URL, DEFAULT_MODEL, "sk-or-test");
        assert_eq!(provider.label(), "openrouter:openrouter/auto");
    }
}
