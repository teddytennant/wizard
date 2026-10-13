//! The OpenAI-compatible family, as a plugin: `https://api.openai.com`'s own
//! Chat Completions endpoint, whatever else a user configures as
//! `kind = "openai"`, and [`openrouter`]. Behind `--features provider-openai`.
//!
//! Almost nothing about talking to OpenAI is peculiar to OpenAI. The request
//! shape, the SSE decoding, the bearer-token seam, the retry classification
//! and the model-family field rules are the *protocol*, shared by every
//! adapter in this family, and they live in [`crate::llm::wire`]. This module is
//! what is left when those are taken out, and it is deliberately small: five
//! other providers used to import their protocol from here, which is what
//! stopped any of them from being lifted out on their own. `wire` stayed in
//! core when this file became a plugin, for exactly that reason: five backends
//! build on it, so it is infrastructure and not this plugin's property.
//!
//! What is left is one request field. OpenAI's Chat Completions API accepts a
//! `prompt_cache_key` that routes a turn to the cache the previous turn
//! warmed, and so does xAI's; the key, and the list of endpoints that take it,
//! live in [`crate::llm::wire`] because two plugins now install them. What is
//! this module's is the decision: `kind = "openai"` is also how vLLM, LM
//! Studio, DeepSeek and every `compat.rs` preset is reached, so [`provider`]
//! attaches the key from the configured URL and not from the kind.
//!
//! The configured URL decides one more thing. On `api.openai.com` the
//! reasoning families refuse function tools on Chat Completions once a
//! reasoning effort is set, and answer them on `/v1/responses`. So on that
//! host alone, [`Routed`] sends those models through the Responses client the
//! `provider-chatgpt` plugin already has, authorized by the same API key.

pub mod openrouter;

use crate::kernel::{Capability, Ctx, Plugin, PluginManifest};
use crate::llm::registry::{Credentials, ProviderDescriptor, ProviderKind};
use crate::llm::wire::{OpenAiProvider, endpoint_takes_a_cache_key, prompt_cache_key};
#[cfg(feature = "provider-chatgpt")]
use crate::llm::{
    ChatRequest, ChatStream,
    provider::LlmProvider,
    wire::{OPENAI_API_ROOT, is_under_api_root, openai_reasoning_family},
};

/// Build the client for a provider configured as `kind = "openai"`.
///
/// `base_url` is whatever the user configured, and it is often not OpenAI:
/// the `openai` kind is also how vLLM, LM Studio, DeepSeek and the
/// `compat.rs` presets (Groq, together.ai, Gemini) are reached. That is why
/// the prompt cache key is attached from the URL rather than from the kind —
/// the kind says "speaks this wire shape", not "is OpenAI". An xAI base URL
/// configured this way gets the key too, which is right: it is the endpoint
/// that implements the field.
pub fn provider(
    base_url: impl Into<String>,
    model: impl Into<String>,
    api_key: impl Into<String>,
) -> OpenAiProvider {
    let base_url = base_url.into();
    let keyed = endpoint_takes_a_cache_key(&base_url);
    let provider = OpenAiProvider::new(base_url, model, api_key);
    if keyed {
        provider.with_prompt_cache_key(prompt_cache_key)
    } else {
        provider
    }
}

/// How `kind = "openai"` is registered.
///
/// [`Credentials::ApiKey`] with no default env var, because this kind is also
/// how vLLM, LM Studio, DeepSeek and every `compat.rs` preset is reached:
/// there is no one variable to guess at, so an unconfigured `api_key_env`
/// falls through to the stored credential rather than to `OPENAI_API_KEY`.
/// That is what the old `match` arm did by passing `None`, and guessing here
/// would start sending an OpenAI key to a local vLLM.
pub fn descriptor() -> ProviderDescriptor {
    ProviderDescriptor::new(
        ProviderKind::OPENAI,
        "OpenAI-compatible",
        Credentials::ApiKey { default_env: None },
        |config| {
            let key = config.api_key();
            if key.is_empty() {
                config.warn_missing_key("API key", "an env var");
            }
            let chat = provider(config.base_url.clone(), config.model.clone(), key.clone());
            #[cfg(feature = "provider-chatgpt")]
            if is_under_api_root(&config.base_url, OPENAI_API_ROOT) {
                let responses = crate::plugins::chatgpt::ChatgptProvider::with_api_key(
                    config.base_url.clone(),
                    config.model.clone(),
                    key,
                );
                return Ok(std::sync::Arc::new(Routed { chat, responses }));
            }
            Ok(std::sync::Arc::new(chat))
        },
    )
}

/// `kind = "openai"` pointed at OpenAI itself: Chat Completions, except for
/// the models that need the Responses API.
///
/// The choice is made per request and not when the client is built, because
/// a request carries its own model and it need not be the configured one.
#[cfg(feature = "provider-chatgpt")]
#[derive(Debug)]
struct Routed {
    chat: OpenAiProvider,
    responses: crate::plugins::chatgpt::ChatgptProvider,
}

#[cfg(feature = "provider-chatgpt")]
#[async_trait::async_trait]
impl LlmProvider for Routed {
    async fn health(&self) -> anyhow::Result<()> {
        self.chat.health().await
    }

    async fn supports_native_tools(&self, model: &str) -> anyhow::Result<bool> {
        self.chat.supports_native_tools(model).await
    }

    async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        self.chat.list_models().await
    }

    async fn chat_stream(&self, request: ChatRequest) -> anyhow::Result<ChatStream> {
        if openai_reasoning_family(&request.model) {
            self.responses.chat_stream(request).await
        } else {
            self.chat.chat_stream(request).await
        }
    }

    async fn context_window(&self, model: &str) -> Option<u32> {
        self.chat.context_window(model).await
    }

    fn label(&self) -> String {
        self.chat.label()
    }
}

/// The OpenAI-compatible family as a kernel plugin.
///
/// Two kinds, one plugin, because they are one endpoint shape with two
/// sets of defaults: `openrouter` is `openai` with a fixed base URL and
/// two attribution headers. Splitting them would give the smaller half a
/// cargo feature whose whole content is a `with_headers` call, and would
/// leave a build that had `openrouter` but not the `openai` kind that
/// vLLM, LM Studio, DeepSeek and every `compat.rs` preset are configured
/// as — a combination nobody wants and everybody would have to test.
///
/// The wire machinery itself is [`crate::llm::wire`] and stays in core:
/// five backends build on it, so it is infrastructure rather than this
/// plugin's property. What is left here is the one field that really is
/// OpenAI's — `prompt_cache_key`, which no other endpoint on this wire
/// implements.
///
/// `network` is declared because that is what this plugin does, even though
/// the capability set only gates the Lua host bridge today. A manifest that
/// under-declares is the failure mode worth avoiding: the grant prompt is
/// generated from it.
pub struct OpenAiPlugin {
    manifest: PluginManifest,
}

impl OpenAiPlugin {
    pub fn new() -> Self {
        Self {
            manifest: PluginManifest {
                name: "openai".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                description: "OpenAI-compatible Chat Completions, and OpenRouter".to_string(),
                capabilities: vec![Capability::Network],
                optional_deps: Vec::new(),
                profiles: vec![
                    "minimal".to_string(),
                    "server".to_string(),
                    "default".to_string(),
                ],
            },
        }
    }
}

impl Default for OpenAiPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for OpenAiPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn apply(&self, ctx: &mut Ctx) -> anyhow::Result<()> {
        ctx.provider(descriptor())?;
        ctx.provider(openrouter::descriptor())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{ChatMessage, ChatRequest};

    #[test]
    fn the_prompt_cache_key_rides_only_on_endpoints_that_take_one() {
        let request = ChatRequest {
            model: "gpt-4o".to_string(),
            messages: vec![
                ChatMessage::system("You are Wizard."),
                ChatMessage::user("hi"),
            ],
            tools: Vec::new(),
            stream: true,
            options: None,
        };

        // Both endpoints that document the field, reached through this kind.
        // xAI is here because `kind = "openai"` with xAI's base URL is a real
        // way to configure it, and the field belongs to the endpoint.
        for base_url in [
            "https://api.openai.com/v1",
            "https://api.openai.com/v1/",
            "https://api.x.ai/v1",
        ] {
            let hosted = provider(base_url, "gpt-4o", "sk-test");
            let key = hosted.build_request_body(&request)["prompt_cache_key"]
                .as_str()
                .unwrap_or_else(|| panic!("{base_url} takes a cache key"))
                .to_string();
            assert!(key.starts_with("wz-"), "{key}");
        }

        // Everything else in the family degrades to no key rather than
        // putting a field on the wire that is at best ignored: a local
        // OpenAI-compatible server, Cloudflare's endpoint, OpenRouter.
        for base_url in [
            "http://127.0.0.1:1234/v1",
            "https://api.cloudflare.com/client/v4/accounts/acc/ai/v1",
            "https://openrouter.ai/api/v1",
            // Hosts that merely start with one of the real names are not it.
            "https://api.openai.com.example.net/v1",
            "https://api.x.ai.example.net/v1",
        ] {
            let other = provider(base_url, "gpt-4o", "k");
            assert!(
                other
                    .build_request_body(&request)
                    .get("prompt_cache_key")
                    .is_none(),
                "{base_url} must not receive prompt_cache_key"
            );
        }
    }

    /// One completed Responses reply, enough for the client to finish a turn.
    #[cfg(feature = "provider-chatgpt")]
    const RESPONSES_SSE: &str = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n\
        data: {\"type\":\"response.completed\",\"response\":{}}\n\n";

    #[cfg(feature = "provider-chatgpt")]
    async fn drain(provider: &dyn LlmProvider, model: &str) {
        use futures_util::StreamExt as _;

        let mut stream = provider
            .chat_stream(crate::llm::test_support::parallel_batch_request(model))
            .await
            .expect("stream opens");
        while let Some(chunk) = stream.next().await {
            chunk.expect("chunk decodes");
        }
    }

    /// On OpenAI's own host a reasoning model goes to `/v1/responses` and
    /// everything else stays on Chat Completions. The two halves are told
    /// apart by the body each server was sent: `input` items or `messages`.
    #[cfg(feature = "provider-chatgpt")]
    #[tokio::test]
    async fn reasoning_models_are_routed_to_the_responses_api() {
        use crate::llm::test_support::{PARALLEL_TOOL_BATCH_SSE, Recorded};
        use crate::plugins::chatgpt::ChatgptProvider;

        for (model, to_responses) in [
            ("gpt-6-astra", true),
            ("gpt-5.5", true),
            ("o4-mini", true),
            ("gpt-4o", false),
            ("gpt-4.1-mini", false),
        ] {
            let chat = Recorded::replay(PARALLEL_TOOL_BATCH_SSE).await;
            let responses = Recorded::replay(RESPONSES_SSE).await;
            let routed = Routed {
                chat: provider(format!("{}/v1", chat.root), model, "sk-test"),
                responses: ChatgptProvider::with_api_key(
                    format!("{}/v1", responses.root),
                    model,
                    "sk-test",
                ),
            };
            assert_eq!(routed.label(), format!("openai:{model}"));
            drain(&routed, model).await;

            let sent = if to_responses { &responses } else { &chat }.request_body();
            assert_eq!(sent["model"], model);
            assert_eq!(sent.get("input").is_some(), to_responses, "{model}");
            assert_eq!(sent.get("messages").is_none(), to_responses, "{model}");
        }
    }

    /// Any other base URL keeps every model on Chat Completions, a reasoning
    /// tag included: the `openai` kind is how OpenRouter, xAI, proxies and
    /// local servers are reached, and none of them is known to serve
    /// `/responses`.
    #[cfg(feature = "provider-chatgpt")]
    #[tokio::test]
    async fn another_base_url_stays_on_chat_completions() {
        use crate::llm::test_support::{PARALLEL_TOOL_BATCH_SSE, Recorded};

        let recorded = Recorded::replay(PARALLEL_TOOL_BATCH_SSE).await;
        let config = crate::config::ProviderConfig {
            name: "elsewhere".to_string(),
            kind: ProviderKind::OPENAI,
            base_url: format!("{}/v1", recorded.root),
            model: "gpt-6-astra".to_string(),
            // No env var and no stored key: a keyless local server.
            api_key_env: Some(String::new()),
            ..Default::default()
        };
        let built = descriptor().build(&config).expect("builds");
        drain(built.as_ref(), "gpt-6-astra").await;
        let sent = recorded.request_body();
        assert!(sent.get("messages").is_some(), "{sent}");
        assert!(sent.get("input").is_none(), "{sent}");

        assert!(is_under_api_root(
            "https://api.openai.com/v1/",
            OPENAI_API_ROOT
        ));
        for base_url in [
            "https://openrouter.ai/api/v1",
            "https://api.x.ai/v1",
            "http://127.0.0.1:1234/v1",
            "https://api.openai.com.example.net/v1",
            "https://my-proxy.example/openai/v1",
        ] {
            assert!(!is_under_api_root(base_url, OPENAI_API_ROOT), "{base_url}");
        }
    }

    /// The seam itself: a shared client nobody handed a key function to sends
    /// no key, even pointed at OpenAI.
    ///
    /// This is what lets each plugin decide. OpenRouter, Cloudflare and
    /// llama.cpp build the client directly and reach this branch, and each of
    /// them documents that it sends no key. Were the client to go back to
    /// sniffing the URL, they would inherit a field the moment one of them was
    /// pointed at a proxy of an endpoint that takes one, and nothing else
    /// would notice.
    #[test]
    fn the_shared_client_alone_sends_no_key() {
        let request = ChatRequest {
            model: "gpt-4o".to_string(),
            messages: vec![
                ChatMessage::system("You are Wizard."),
                ChatMessage::user("hi"),
            ],
            tools: Vec::new(),
            stream: true,
            options: None,
        };
        let bare = OpenAiProvider::new("https://api.openai.com/v1", "gpt-4o", "sk-test");
        assert!(
            bare.build_request_body(&request)
                .get("prompt_cache_key")
                .is_none()
        );
    }
}
