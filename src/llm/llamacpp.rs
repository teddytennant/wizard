//! Provider for llama.cpp's `llama-server`.
//!
//! `llama-server` exposes the OpenAI-compatible Chat Completions API under
//! `/v1`, so chat streaming, model listing, and tool support all delegate to
//! an inner [`OpenAiProvider`] bound to `{base_url}/v1`. Only what differs
//! lives here: the health probe hits llama-server's native `GET /health`
//! (which distinguishes "still loading the model" from "down"), and
//! connection failures tell the user how to start the server.
//!
//! Requests are therefore built entirely by the inner client, tool-call ids
//! and parallel batches included. Prompt caching is the one hosted feature
//! that has no counterpart here, and it degrades to nothing rather than to a
//! field on the wire: llama-server keeps a KV cache per slot and reuses the
//! longest matching prefix of the next request on its own, with no API to
//! address it by, so there is no `prompt_cache_key` to send and sending one
//! would only add a key this server never asked for. Leaving it off costs
//! nothing here: the shared client sends the field only when the module that
//! owns the endpoint installs a key function, and only `openai` does.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::OnceCell;

use super::provider::LlmProvider;
use super::wire::OpenAiProvider;
use super::{ChatRequest, ChatStream, ProviderError};

/// How long to wait for a TCP connection before declaring llama-server down.
/// Shorter than the shared chat budget because this client only ever makes
/// the two tiny local probes below: a loopback connect either lands at once
/// or the server is not there. There is deliberately no read timeout:
/// `/health` answers immediately even while the model is still loading, and
/// nothing here streams.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Client bound to one llama-server instance. Cheap to clone.
#[derive(Debug, Clone)]
pub struct LlamaCppProvider {
    http: reqwest::Client,
    /// Server root, e.g. `http://127.0.0.1:11435` (no `/v1` suffix). Trailing
    /// slashes are trimmed.
    base_url: String,
    /// Model tag for [`LlmProvider::label`]; llama-server serves whatever
    /// GGUF it was started with regardless of the requested model.
    model: String,
    /// OpenAI-compatible client bound to `{base_url}/v1`, handling chat
    /// streaming and `/v1/models`. Keyless — llama-server needs no auth.
    inner: OpenAiProvider,
    /// Cached result of the `GET /props` context-window probe (`n_ctx`).
    /// Probed once per provider instance; a failed probe caches `None`.
    ctx_window: Arc<OnceCell<Option<u32>>>,
}

impl LlamaCppProvider {
    /// Build a client for `base_url` (the server root, e.g.
    /// `http://127.0.0.1:11435` — without `/v1`).
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        let model = model.into();
        // The inner client speaks to a process on this machine, so it must not
        // inherit the cloud read timeout: a large GGUF prefilling a long
        // prompt on weak hardware is silent for as long as it takes, and five
        // minutes of that is a slow model, not a stalled connection. Killing
        // it would surface as a transient error and send the agent's retry
        // loop round again on a request that was working.
        let inner = crate::llm::with_local_inference_timeouts(|| {
            OpenAiProvider::new(format!("{base_url}/v1"), model.clone(), "")
        });
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            // Builder construction only fails when the TLS backend cannot
            // initialize; fall back to the default client rather than panic.
            .unwrap_or_default();
        Self {
            http,
            base_url,
            model,
            inner,
            ctx_window: Arc::new(OnceCell::new()),
        }
    }

    /// Probe llama-server's `GET /props` for the loaded model's context size
    /// (`default_generation_settings.n_ctx`). Any failure — server down,
    /// older server without the endpoint, unexpected shape — yields `None`.
    async fn fetch_n_ctx(&self) -> Option<u32> {
        let response = self
            .http
            .get(format!("{}/props", self.base_url))
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let body: serde_json::Value = response.json().await.ok()?;
        body.get("default_generation_settings")?
            .get("n_ctx")?
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
    }

    /// Actionable error for a server that cannot be reached at all.
    fn unreachable(&self, source: reqwest::Error) -> anyhow::Error {
        let message = format!(
            "cannot reach llama-server at {} — is the server running? Start it with \
             `llama-server -m <model.gguf> --port 11435` (or check the provider's `base_url` \
             in ~/.wizard/config.toml). Cause: {source}",
            self.base_url
        );
        anyhow::Error::new(source).context(ProviderError::transport(message))
    }

    /// Re-frame errors bubbling out of the inner OpenAI-compatible client:
    /// when the chain bottoms out in a connection failure, prepend the
    /// "start llama-server" hint. Other errors pass through untouched.
    fn reframe(&self, err: anyhow::Error) -> anyhow::Error {
        let is_connect_failure = err.chain().any(|cause| {
            cause
                .downcast_ref::<reqwest::Error>()
                .is_some_and(|e| e.is_connect() || e.is_timeout())
        });
        if is_connect_failure {
            err.context(format!(
                "cannot reach llama-server at {} — is the server running? Start it with \
                 `llama-server -m <model.gguf> --port 11435`",
                self.base_url
            ))
        } else {
            err
        }
    }
}

#[async_trait]
impl LlmProvider for LlamaCppProvider {
    /// Probe llama-server's native `GET /health`: 200 means ready, 503 means
    /// the model is still loading (llama-server answers before the GGUF is
    /// fully in memory).
    async fn health(&self) -> Result<()> {
        let response = self
            .http
            .get(format!("{}/health", self.base_url))
            .send()
            .await
            .map_err(|source| self.unreachable(source))?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        // Read the header before the body: `text()` consumes the response.
        // llama-server itself sends no `Retry-After`, but a reverse proxy in
        // front of it may, and honoring it beats guessing at a backoff.
        let retry_after = crate::llm::retry_after_from_headers(response.headers());
        if status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
            return Err(crate::llm::http_error_with_retry_after(
                503,
                format!(
                    "llama-server at {} is still loading its model (HTTP 503) — try again shortly",
                    self.base_url
                ),
                retry_after,
            ));
        }
        let body = response.text().await.unwrap_or_default();
        Err(crate::llm::http_error_with_retry_after(
            status.as_u16(),
            format!(
                "llama-server at {} returned HTTP {status}: {body}",
                self.base_url
            ),
            retry_after,
        ))
    }

    async fn supports_native_tools(&self, model: &str) -> Result<bool> {
        self.inner.supports_native_tools(model).await
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        self.inner
            .list_models()
            .await
            .map_err(|err| self.reframe(err))
    }

    async fn chat_stream(&self, request: ChatRequest) -> Result<ChatStream> {
        self.inner
            .chat_stream(request)
            .await
            .map_err(|err| self.reframe(err))
    }

    /// llama-server serves whatever GGUF it was started with, so the live
    /// `/props` probe beats any static table. Cached after the first call.
    async fn context_window(&self, _model: &str) -> Option<u32> {
        *self.ctx_window.get_or_init(|| self.fetch_n_ctx()).await
    }

    fn label(&self) -> String {
        format!("llama.cpp:{}", self.model)
    }
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;

    use super::*;

    #[test]
    fn base_url_trailing_slash_is_trimmed() {
        let provider = LlamaCppProvider::new("http://127.0.0.1:8080///", "qwen3-8b");
        assert_eq!(provider.base_url, "http://127.0.0.1:8080");
        assert_eq!(provider.label(), "llama.cpp:qwen3-8b");
    }

    #[test]
    fn inner_client_targets_the_v1_api() {
        let provider = LlamaCppProvider::new("http://10.0.0.5:8080", "m");
        assert_eq!(provider.inner.label(), "openai:m");
        // The unreachable hint names the server root, not the /v1 endpoint.
        let hint = provider.reframe(anyhow!("plain error"));
        assert_eq!(hint.to_string(), "plain error", "non-connect passthrough");
    }

    #[tokio::test]
    async fn context_window_probe_failure_degrades_to_none() {
        // Port 1 on localhost: connection refused immediately, no server
        // needed. The failed probe caches None instead of erroring.
        let provider = LlamaCppProvider::new("http://127.0.0.1:1", "m");
        assert_eq!(provider.context_window("m").await, None);
        assert_eq!(provider.context_window("m").await, None, "cached");
    }

    #[tokio::test]
    async fn unreachable_chat_errors_with_the_start_hint() {
        // The connect failure bubbles out of the inner OpenAI-compatible
        // client; reframe must prepend the "start llama-server" hint.
        let provider = LlamaCppProvider::new("http://127.0.0.1:1", "m");
        let request = ChatRequest {
            model: "m".to_string(),
            messages: vec![crate::llm::ChatMessage::user("hi")],
            tools: Vec::new(),
            stream: true,
            options: None,
        };
        let err = match provider.chat_stream(request).await {
            Ok(_) => panic!("must fail"),
            Err(err) => err,
        };
        let chain = format!("{err:#}");
        assert!(chain.contains("llama-server -m"), "got: {chain}");
        assert!(chain.contains("http://127.0.0.1:1"), "got: {chain}");
    }

    #[test]
    fn the_inner_client_is_built_for_local_inference() {
        // The inner client records the read-timeout policy it was actually
        // constructed with, so this asserts the constructor's own behaviour
        // rather than restating `with_local_inference_timeouts`: no read
        // timeout, so a multi-minute GGUF prefill is never mistaken for a
        // stalled connection and retried on top of itself.
        let provider = LlamaCppProvider::new("http://127.0.0.1:11435", "m");
        assert_eq!(provider.inner.read_timeout(), None);

        // The same holds for a llama-server reached over a public name (an
        // SSH tunnel, a Tailscale hostname), where the address cannot say it
        // is local inference but the provider kind can. Drop the
        // `with_local_inference_timeouts` scope from `new` and this is the
        // assertion that fails.
        let tunnelled = LlamaCppProvider::new("https://gpu.example.com", "m");
        assert_eq!(tunnelled.inner.read_timeout(), None);

        // Building a provider leaves the thread back on the cloud policy, so
        // the next hosted client constructed on it still gets the stall
        // detector.
        assert!(crate::llm::client_read_timeout().is_some());
    }

    #[tokio::test]
    async fn a_loading_server_hands_its_retry_after_to_the_backoff() {
        // llama-server itself sends no `Retry-After`, but a reverse proxy in
        // front of it does, and the 503-while-loading answer is exactly when
        // waiting the stated time beats guessing. Driven through a real
        // socket because the header is only readable from a real response.
        let root = crate::llm::test_support::one_shot_http_server(
            "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 5\r\nContent-Length: \
             7\r\nConnection: close\r\n\r\nloading",
        )
        .await;
        let provider = LlamaCppProvider::new(root, "m");
        let err = provider.health().await.expect_err("503 is not healthy");
        assert!(err.to_string().contains("still loading"), "{err}");
        assert_eq!(
            err.downcast_ref::<ProviderError>()
                .expect("typed provider error")
                .status,
            Some(503)
        );
        assert_eq!(
            err.downcast_ref::<crate::llm::RetryAfter>()
                .map(|hint| hint.0),
            Some(Duration::from_secs(5)),
            "the proxy's own deadline reaches the retry loop"
        );
    }

    /// A two-call parallel batch, driven through the real client to a
    /// recorded llama-server stream.
    ///
    /// llama-server's tool-calling support is a grammar-constrained
    /// re-implementation of OpenAI's shape rather than the same code, and a
    /// batch is where the two most easily diverge, so the body this provider
    /// sends is asserted rather than assumed. The same test pins the prompt
    /// cache degrading to *nothing*: a local server has no key to route on.
    #[tokio::test]
    async fn a_parallel_batch_reaches_llama_server_in_the_shared_shape() {
        use futures_util::StreamExt as _;

        use crate::llm::test_support::{
            PARALLEL_TOOL_BATCH_SSE, Recorded, assert_batch_is_answerable, parallel_batch_request,
        };

        let recorded = Recorded::replay(PARALLEL_TOOL_BATCH_SSE).await;
        let provider = LlamaCppProvider::new(recorded.root.as_str(), "qwen3-8b");

        let mut stream = provider
            .chat_stream(parallel_batch_request("qwen3-8b"))
            .await
            .expect("stream opens");
        let mut last = None;
        while let Some(chunk) = stream.next().await {
            last = Some(chunk.expect("chunk decodes"));
        }

        let sent = recorded.request_body();
        assert_batch_is_answerable(sent["messages"].as_array().expect("messages"), 2);
        assert!(
            sent.get("prompt_cache_key").is_none(),
            "llama-server reuses its own KV cache and has no key to route on: {sent}"
        );

        let calls = last
            .expect("a final chunk")
            .message
            .expect("tool call message")
            .tool_calls()
            .iter()
            .map(|call| (call.id.clone(), call.function.name.clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            calls,
            vec![
                ("call_aaa".to_string(), "read_file".to_string()),
                ("call_bbb".to_string(), "read_file".to_string()),
            ],
            "two calls to one tool, told apart by id"
        );
    }

    #[tokio::test]
    async fn health_failure_is_actionable() {
        // Port 1 on localhost: connection refused immediately, no server needed.
        let provider = LlamaCppProvider::new("http://127.0.0.1:1", "m");
        let err = provider.health().await.expect_err("must fail");
        let message = err.to_string();
        assert!(message.contains("http://127.0.0.1:1"), "got: {message}");
        assert!(message.contains("llama-server -m"), "got: {message}");
    }
}
