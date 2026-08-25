//! The provider abstraction: one trait every LLM backend implements so the
//! agent loop, the tool registry, and the TUI are decoupled from any specific
//! API (llama.cpp, Ollama, OpenAI-compatible, Anthropic, ...).
//!
//! Concrete implementations live in sibling modules ([`super::llamacpp`],
//! [`super::ollama`], [`super::openai`], and the provider plugins). A provider is
//! built from a [`crate::config::ProviderConfig`] and handed to the agent as
//! an `Arc<dyn LlmProvider>`.

use async_trait::async_trait;

/// A streaming chat backend. Implementations translate Wizard's native wire
/// types (see [`crate::llm`]) to and from their own API shape, exposing a
/// uniform [`ChatChunk`](crate::llm::ChatChunk) stream the agent loop consumes.
///
/// All methods are `async` and fallible; transport and API errors surface as
/// `anyhow::Error` so the TUI can render actionable messages.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Cheap reachability/auth probe run at startup. Errors when the backend
    /// is unreachable or the credentials are rejected.
    async fn health(&self) -> anyhow::Result<()>;

    /// Whether `model` supports native (structured) tool calling. When this
    /// returns `false` the agent loop falls back to the prompt-based JSON tool
    /// protocol.
    async fn supports_native_tools(&self, model: &str) -> anyhow::Result<bool>;

    /// List the models the backend exposes (for the `/model` picker).
    async fn list_models(&self) -> anyhow::Result<Vec<String>>;

    /// Start a streaming chat completion, yielding
    /// [`ChatChunk`](crate::llm::ChatChunk)s until one with `done == true`.
    async fn chat_stream(
        &self,
        request: crate::llm::ChatRequest,
    ) -> anyhow::Result<crate::llm::ChatStream>;

    /// Total context window of `model` in tokens, when known. Drives
    /// token-aware history compaction; `None` means unknown, in which case
    /// only the byte threshold applies. Implementations may consult static
    /// tables (cloud APIs) or query the backend (llama.cpp `/props`) — a
    /// probe failure must degrade to `None`, never error.
    async fn context_window(&self, _model: &str) -> Option<u32> {
        None
    }

    /// Short human label for the status bar / errors (e.g. the host or
    /// `"openai:gpt-4o"`).
    fn label(&self) -> String;
}

/// What to assume about `model` when the probe itself fails (the backend is
/// mid-restart, the endpoint 404s, the JSON is a shape we don't know).
///
/// The two mistakes are not symmetric. Driving a model that has no native tool
/// calling as though it had it fails hard — the backend rejects the request, or
/// accepts it and silently drops the tools, and the agent has no hands. Driving
/// a native-capable model through the prompt-based JSON protocol only costs the
/// protocol's prompt tokens; it still calls tools. So a failed probe takes the
/// one that works either way.
pub const NATIVE_TOOLS_ON_PROBE_FAILURE: bool = false;

/// Whether `model` on `client` gets native tool calls or the JSON tool protocol.
///
/// Every surface resolves the question here, so a flaky probe cannot mean native
/// tools in one and the JSON protocol in another — same model, same wire format,
/// whichever surface is driving.
pub async fn probe_native_tools(client: &dyn LlmProvider, model: &str) -> bool {
    match client.supports_native_tools(model).await {
        Ok(supported) => supported,
        Err(err) => {
            tracing::warn!(
                "could not probe tool support for '{model}': {err:#}; \
                 assuming native_tools={NATIVE_TOOLS_ON_PROBE_FAILURE}"
            );
            NATIVE_TOOLS_ON_PROBE_FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Answers the probe with whatever it was built with.
    struct Probe(anyhow::Result<bool>);

    #[async_trait]
    impl LlmProvider for Probe {
        async fn health(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn supports_native_tools(&self, _model: &str) -> anyhow::Result<bool> {
            match &self.0 {
                Ok(supported) => Ok(*supported),
                Err(err) => anyhow::bail!("{err}"),
            }
        }
        async fn list_models(&self) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }
        async fn chat_stream(
            &self,
            _request: crate::llm::ChatRequest,
        ) -> anyhow::Result<crate::llm::ChatStream> {
            anyhow::bail!("no model behind this test")
        }
        fn label(&self) -> String {
            "probe".to_string()
        }
    }

    /// Every surface resolves the protocol through this one function, so a flaky
    /// probe cannot mean native tools in the GUI and the JSON protocol in the
    /// TUI. This pins the fallback it lands on: flip the constant and the assert
    /// below fails, wherever the caller lives.
    #[tokio::test]
    async fn a_failed_probe_falls_back_to_the_one_protocol_that_works_either_way() {
        let answered = Probe(Ok(true));
        assert!(probe_native_tools(&answered, "m").await);
        let answered = Probe(Ok(false));
        assert!(!probe_native_tools(&answered, "m").await);

        let broken = Probe(Err(anyhow::anyhow!("connection reset")));
        assert!(
            !probe_native_tools(&broken, "m").await,
            "an unanswered probe takes the JSON tool protocol: it drives a \
             native-capable model fine, where the reverse leaves a model without \
             native tool calling no hands at all"
        );
    }
}
