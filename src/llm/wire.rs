//! The OpenAI **Chat Completions** wire protocol (`POST
//! {base_url}/chat/completions`) and the client every adapter that speaks it
//! is built on.
//!
//! This is protocol machinery, not a vendor. Six providers put this exact
//! shape on the wire — OpenAI itself, OpenRouter, xAI by key and by OAuth,
//! Cloudflare Workers AI, llama.cpp — and Groq, together.ai, vLLM, LM Studio
//! and Gemini's compatibility endpoint reach it through the `compat.rs`
//! presets. The protocol is therefore core and the vendor is not: keeping the
//! two in one file is what made `openai.rs` a module five other adapters
//! import from, and that has to stop before any of them can be lifted out.
//!
//! Everything the *shape* decides lives here: translating Wizard's native
//! [`ChatRequest`] into the request body, the bearer-token seam
//! ([`TokenSource`]), the 401-refresh retry and the HTTP-failure mapping,
//! manual SSE parsing with no extra dependencies, per-index tool-call
//! assembly, and the model-family tables saying which optional request fields
//! a model tag tolerates. Those tables read like OpenAI trivia and are not:
//! they are consulted for every request on this wire, so an OpenRouter or xAI
//! request would change shape if they moved out with the vendor.
//!
//! What does not live here is anything true of one endpoint only. There is
//! exactly one such thing today, OpenAI's `prompt_cache_key`, and
//! [`super::openai`] installs it via [`OpenAiProvider::with_prompt_cache_key`].

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use futures_util::{Stream, StreamExt, stream};
use serde::Deserialize;
use serde_json::{Value, json};

use super::provider::LlmProvider;
use super::{
    CacheTokens, ChatChunk, ChatMessage, ChatRequest, ChatStream, ContentBlock, FunctionCall,
    Image, ProviderError, Role, ToolCall,
};

/// Supplies the `Authorization: Bearer` token for each request. The plain
/// API-key case is [`StaticToken`]; OAuth-backed providers (xAI sign-in)
/// plug in a source that refreshes the access token between calls.
#[async_trait]
pub trait TokenSource: Send + Sync + std::fmt::Debug {
    /// The current bearer token, or `None` when the endpoint needs no auth.
    /// May refresh an expiring token before returning it.
    async fn bearer(&self) -> Result<Option<String>>;

    /// Called once after an HTTP 401 from the API. Returns `true` when a
    /// fresh token was obtained and the request should be retried.
    async fn refresh_after_unauthorized(&self) -> Result<bool> {
        Ok(false)
    }

    /// What the user should do about a persistent HTTP 401.
    fn unauthorized_hint(&self) -> &str {
        "check the configured API key env var"
    }

    /// Extra context appended to HTTP 403 errors (e.g. plan-gating hints).
    fn forbidden_hint(&self) -> Option<&str> {
        None
    }
}

/// Fixed API key. An empty key means no `Authorization` header is sent
/// (keyless local servers like vLLM or LM Studio).
#[derive(Debug)]
pub struct StaticToken(Option<String>);

impl StaticToken {
    pub fn new(api_key: impl Into<String>) -> Self {
        let key = api_key.into();
        Self((!key.is_empty()).then_some(key))
    }
}

#[async_trait]
impl TokenSource for StaticToken {
    async fn bearer(&self) -> Result<Option<String>> {
        Ok(self.0.clone())
    }
}

/// Computes the `prompt_cache_key` for one request, from the model tag and
/// the messages, or `None` when the request has nothing worth keying on.
///
/// A function and not a flag: this client knows only that some endpoints have
/// such a field, and what a cache bucket *means* is the endpoint's business.
/// See [`OpenAiProvider::with_prompt_cache_key`].
pub type PromptCacheKeyFn = fn(&str, &[ChatMessage]) -> Option<String>;

/// Client bound to one OpenAI-compatible endpoint.
#[derive(Debug, Clone)]
pub struct OpenAiProvider {
    http: reqwest::Client,
    /// Read timeout `http` was built with, resolved once from the endpoint's
    /// locality (see [`crate::llm::client_read_timeout_for`]). `reqwest`
    /// exposes no accessor for a built client's timeouts, so it is recorded
    /// here: [`OpenAiProvider::with_headers`] has to rebuild the client and
    /// must not silently put a local endpoint back on the cloud policy, and
    /// "which policy did this client get" is the first question when a long
    /// local generation dies at exactly five minutes.
    read_timeout: Option<Duration>,
    /// Base URL including the API version segment, e.g.
    /// `https://api.openai.com/v1`. Trailing slashes are trimmed.
    base_url: String,
    /// Default model tag (used only for [`LlmProvider::label`]; requests carry
    /// their own model).
    model: String,
    /// Bearer token supplier (static key or refreshing OAuth source).
    auth: Arc<dyn TokenSource>,
    /// Vendor prefix for [`LlmProvider::label`] (`openai`, `xai`, ...).
    vendor: &'static str,
    /// Computes the `prompt_cache_key` a request carries, or `None` when the
    /// endpoint has no such field. Installed by the module that owns the
    /// endpoint rather than sniffed from the base URL here: OpenAI's API is
    /// the only member of this family with the field, a strict server can
    /// reject an unknown one outright, and a shared client that guessed would
    /// have to be re-taught every time another endpoint grew a cache.
    prompt_cache_key: Option<PromptCacheKeyFn>,
}

impl OpenAiProvider {
    /// Build a client for `base_url` (which must already include `/v1`).
    /// `api_key` may be empty for keyless local servers (vLLM, LM Studio).
    ///
    /// The client this returns speaks the protocol and nothing more — no
    /// `prompt_cache_key`, whatever `base_url` points at. A caller that owns
    /// an endpoint with one adds it with [`Self::with_prompt_cache_key`];
    /// `super::openai::provider` is the only place that does.
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self::with_token_source(
            base_url,
            model,
            Arc::new(StaticToken::new(api_key)),
            "openai",
        )
    }

    /// Build a client whose bearer token comes from `auth` on every request.
    /// `vendor` is the label prefix shown in the UI (e.g. `xai`).
    pub fn with_token_source(
        base_url: impl Into<String>,
        model: impl Into<String>,
        auth: Arc<dyn TokenSource>,
        vendor: &'static str,
    ) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        // An OpenAI-compatible endpoint is not necessarily a hosted one: LM
        // Studio, vLLM and llama.cpp all speak this wire shape from the
        // user's own machine, where a silent socket means a slow prefill and
        // not a dead connection.
        let read_timeout = crate::llm::client_read_timeout_for(&base_url);
        let http = crate::llm::chat_http_builder(read_timeout)
            .build()
            .unwrap_or_default();
        Self {
            http,
            read_timeout,
            base_url,
            model: model.into(),
            auth,
            vendor,
            prompt_cache_key: None,
        }
    }

    /// Rebuild the inner HTTP client with `headers` sent on every request
    /// (e.g. OpenRouter's attribution headers). Invalid header names or
    /// values are skipped. The timeout policy the client was constructed with
    /// is carried over: adding a header must not move a local endpoint back
    /// onto the cloud read timeout.
    pub fn with_headers(mut self, headers: &[(&str, &str)]) -> Self {
        use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
        let mut map = HeaderMap::new();
        for &(name, value) in headers {
            if let (Ok(name), Ok(value)) =
                (HeaderName::try_from(name), HeaderValue::try_from(value))
            {
                map.insert(name, value);
            }
        }
        self.http = crate::llm::chat_http_builder(self.read_timeout)
            .default_headers(map)
            .build()
            .unwrap_or_default();
        self
    }

    /// Send a `prompt_cache_key` computed by `key` on every request.
    ///
    /// Opt-in because the field belongs to one endpoint. Nothing else on this
    /// wire shape has it: local servers reuse their own KV cache with no API
    /// to address it by, Cloudflare Workers AI has no prompt cache at all,
    /// and the hosted endpoints configured as `openai` providers cache
    /// automatically and document no key field. Sending it to them anyway
    /// would put a field on the wire that is at best ignored and at worst
    /// rejected. See `openai::prompt_cache_key`, the only caller.
    pub fn with_prompt_cache_key(mut self, key: PromptCacheKeyFn) -> Self {
        self.prompt_cache_key = Some(key);
        self
    }

    /// The read timeout this client's endpoint locality resolved to. Test-only
    /// because nothing in the running agent needs to ask; it exists so the
    /// policy a constructor actually applied is assertable at all.
    #[cfg(test)]
    pub(crate) fn read_timeout(&self) -> Option<Duration> {
        self.read_timeout
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// Send a request with the current bearer token attached. On a 401 the
    /// token source gets one chance to refresh, after which the request is
    /// rebuilt (via `build`) and retried exactly once.
    async fn send_authed<F>(&self, build: F) -> Result<reqwest::Response>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        let mut retried = false;
        loop {
            let mut request = build();
            if let Some(token) = self.auth.bearer().await? {
                request = request.bearer_auth(token);
            }
            let response = match request.send().await {
                Ok(response) => response,
                Err(source) => {
                    let message = format!("HTTP request to {} failed: {source}", self.base_url);
                    // Root reqwest error kept on the chain (llama.cpp reframes
                    // connect failures); ProviderError carries the retry class.
                    return Err(
                        anyhow::Error::new(source).context(ProviderError::transport(message))
                    );
                }
            };
            if response.status() == reqwest::StatusCode::UNAUTHORIZED
                && !retried
                && self.auth.refresh_after_unauthorized().await?
            {
                retried = true;
                continue;
            }
            return Ok(response);
        }
    }

    /// Error for a non-success HTTP response, with the token source's hint
    /// appended on 403 (e.g. OAuth plan gating).
    ///
    /// Takes the whole response rather than a status and a body so the
    /// server's `Retry-After` is read off the headers *before* `text()`
    /// consumes it: a 429 that names a deadline is the only thing that stops
    /// the agent's backoff from guessing, and this is the path every chat
    /// completion fails through.
    async fn http_failure(&self, response: reqwest::Response) -> anyhow::Error {
        let status = response.status();
        let retry_after = crate::llm::retry_after_from_headers(response.headers());
        let body = response.text().await.unwrap_or_default();
        let hint = if status == reqwest::StatusCode::FORBIDDEN {
            self.auth
                .forbidden_hint()
                .map(|hint| format!(" ({hint})"))
                .unwrap_or_default()
        } else {
            String::new()
        };
        crate::llm::http_error_with_retry_after(
            status.as_u16(),
            format!("{} returned HTTP {status}: {body}{hint}", self.base_url),
            retry_after,
        )
    }

    /// Translate a native [`ChatRequest`] into the OpenAI Chat Completions
    /// request body. Always sets `stream: true`.
    ///
    /// Crate-visible because the body is half of this adapter family's
    /// contract, and the module that configures the client — `openai`, with
    /// its `prompt_cache_key` — asserts on what comes out without standing up
    /// a socket for it.
    pub(crate) fn build_request_body(&self, request: &ChatRequest) -> Value {
        let messages = build_messages(&request.messages);
        let mut body = json!({
            "model": request.model,
            "messages": messages,
            "stream": true,
            // Without this OpenAI omits `usage` from the SSE stream and
            // token-aware compaction never engages. Compatible servers
            // (llama.cpp, vLLM, OpenRouter, Groq, Ollama's /v1 shim) accept
            // or ignore it.
            "stream_options": { "include_usage": true },
        });
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|spec| {
                json!({
                    "type": "function",
                    "function": {
                        "name": spec.function.name,
                        "description": spec.function.description,
                        "parameters": spec.function.parameters,
                    }
                })
            })
            .collect();
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
        if let Some(options) = &request.options
            && let Some(temperature) = options.temperature
            && !rejects_temperature(&request.model)
        {
            body["temperature"] = json!(temperature);
        }
        if let Some(options) = &request.options
            && let Some(effort) = &options.reasoning_effort
            && supports_reasoning_effort(&request.model)
        {
            body["reasoning_effort"] = json!(effort);
        }
        // Prompt caching on this API is *keyed*, not annotated: there is no
        // per-block breakpoint to place (that is Anthropic's `cache_control`).
        // The server matches the longest cached prefix of the messages array
        // by itself, and this one field only decides which cache the request
        // is routed to, so turn two of a conversation lands on the prefix turn
        // one warmed instead of racing across machines.
        if let Some(key_of) = self.prompt_cache_key
            && let Some(key) = key_of(&request.model, &request.messages)
        {
            body["prompt_cache_key"] = json!(key);
        }
        body
    }
}

/// Models that accept a `reasoning_effort` request field: xAI Grok 4.x and
/// OpenAI's reasoning families (o-series, gpt-5). Anything else 400s on it, so
/// it is sent only for these. Mirrors the families in [`context_window`].
/// Tags marked "non-reasoning" (e.g. xAI's `grok-4.20-*-non-reasoning`)
/// reject the field even inside a supporting family.
fn supports_reasoning_effort(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    if model.contains("non-reasoning") {
        return false;
    }
    model.starts_with("grok-4")
        || model.starts_with("gpt-5")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4")
}

/// OpenAI reasoning models (o-series, gpt-5 family) reject any non-default
/// `temperature` with HTTP 400, so it is omitted for them. Mirrors the model
/// families in [`context_window`].
fn rejects_temperature(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.starts_with("gpt-5")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4")
}

/// Translate native messages into the OpenAI `messages` array. Tool calls
/// carry the id the provider itself issued, and each `tool_result` block
/// becomes its own `tool`-role message bound to that id: the shape this API
/// wants, where Anthropic wants all of a batch's results in one message.
///
/// A `tool`-role [`ChatMessage`] holding a whole parallel batch therefore
/// expands to N consecutive `tool` messages here, with nothing interleaved
/// between them: OpenAI rejects a `tool` message whose `tool_call_id` does
/// not belong to the assistant turn it is answering, and it used to be the
/// agent loop that put a user message full of images (or a system nudge)
/// in the middle of a batch.
///
/// User messages with images become multimodal content arrays (`text` +
/// `image_url` data-URLs).
fn build_messages(messages: &[ChatMessage]) -> Vec<Value> {
    let mut out = Vec::with_capacity(messages.len());

    for message in messages {
        match message.role {
            Role::System => out.push(json!({ "role": "system", "content": message.text() })),
            Role::User if message.images().is_empty() => {
                out.push(json!({ "role": "user", "content": message.text() }))
            }
            // A user message carrying images becomes a multi-part content
            // array: the text first, then one `image_url` part per image as a
            // base64 data URI (the OpenAI / xAI vision format).
            Role::User => {
                let mut parts = vec![json!({ "type": "text", "text": message.text() })];
                for image in message.images() {
                    parts.push(json!({
                        "type": "image_url",
                        "image_url": { "url": image.data_uri() },
                    }));
                }
                out.push(json!({ "role": "user", "content": parts }));
            }
            Role::Assistant => {
                let mut value = json!({ "role": "assistant" });
                // An assistant turn cannot carry image content on this API, so
                // images the model generated are named in the text instead of
                // silently 400-ing the request.
                let content = super::assistant_content(message);
                let tool_calls = message.tool_calls();
                // OpenAI requires `content: null` (not "") when only tool calls
                // are present.
                value["content"] = if content.is_empty() && !tool_calls.is_empty() {
                    Value::Null
                } else {
                    json!(content)
                };
                if !tool_calls.is_empty() {
                    let calls: Vec<Value> = tool_calls
                        .iter()
                        .map(|call| {
                            let arguments = match &call.function.arguments {
                                Value::String(raw) => raw.clone(),
                                other => other.to_string(),
                            };
                            json!({
                                "id": call.id,
                                "type": "function",
                                "function": { "name": call.function.name, "arguments": arguments },
                            })
                        })
                        .collect();
                    value["tool_calls"] = Value::Array(calls);
                }
                out.push(value);
            }
            Role::Tool => {
                for result in message.tool_results() {
                    out.push(json!({
                        "role": "tool",
                        "tool_call_id": result.tool_use_id,
                        "content": result.content,
                    }));
                }
            }
        }
    }
    out
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    async fn health(&self) -> Result<()> {
        let response = self
            .send_authed(|| self.http.get(self.url("/models")))
            .await
            .with_context(|| format!("cannot reach {}", self.base_url))?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(anyhow::Error::new(ProviderError::http(
                401,
                format!(
                    "{} rejected the credentials (HTTP 401): {}",
                    self.base_url,
                    self.auth.unauthorized_hint()
                ),
            )));
        }
        if !response.status().is_success() {
            return Err(self.http_failure(response).await);
        }
        Ok(())
    }

    async fn supports_native_tools(&self, _model: &str) -> Result<bool> {
        // OpenAI-compatible endpoints support structured tool calling; the
        // agent loop's JSON fallback is not needed.
        Ok(true)
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        let response = self
            .send_authed(|| self.http.get(self.url("/models")))
            .await
            .with_context(|| format!("listing models from {}", self.base_url))?;
        if !response.status().is_success() {
            return Err(self.http_failure(response).await);
        }
        let models: ModelsResponse = response
            .json()
            .await
            .context("failed to parse /models response")?;
        Ok(models.data.into_iter().map(|m| m.id).collect())
    }

    async fn chat_stream(&self, request: ChatRequest) -> Result<ChatStream> {
        let body = self.build_request_body(&request);
        let response = self
            .send_authed(|| self.http.post(self.url("/chat/completions")).json(&body))
            .await
            .with_context(|| format!("chat request to {} failed", self.base_url))?;
        if !response.status().is_success() {
            return Err(self.http_failure(response).await);
        }
        let bytes = response
            .bytes_stream()
            .map(|item| match item {
                Ok(chunk) => Ok(chunk.to_vec()),
                Err(e) => Err(anyhow!(e).context(ProviderError::transport(
                    "OpenAI response stream was interrupted",
                ))),
            })
            .boxed();
        Ok(decode_sse(bytes))
    }

    async fn context_window(&self, model: &str) -> Option<u32> {
        context_window(model)
    }

    fn label(&self) -> String {
        format!("{}:{}", self.vendor, self.model)
    }
}

/// Context-window table for OpenAI-compatible endpoints (OpenAI and xAI
/// model families; llama.cpp overrides this with a live `/props` probe).
/// Unknown tags report `None` so compaction falls back to the byte
/// threshold.
pub(crate) fn context_window(model: &str) -> Option<u32> {
    let model = model.to_ascii_lowercase();
    // xAI Grok (served through this provider with vendor "xai").
    if model.starts_with("grok-4.6") || model.starts_with("grok-4.5") {
        return Some(500_000);
    }
    // grok-4.3 and the grok-4.20 snapshots are 1M-context.
    if model.starts_with("grok-4.3") || model.starts_with("grok-4.2") {
        return Some(1_000_000);
    }
    if model.starts_with("grok-4") || model.starts_with("grok-build") {
        return Some(256_000);
    }
    if model.starts_with("grok") {
        return Some(131_072);
    }
    // OpenAI.
    if model.starts_with("gpt-5.6") {
        return Some(1_000_000);
    }
    if model.starts_with("gpt-5") {
        return Some(400_000);
    }
    if model.starts_with("gpt-4.1") {
        return Some(1_047_576);
    }
    if model.starts_with("gpt-4o") || model.starts_with("gpt-4-turbo") {
        return Some(128_000);
    }
    if model.starts_with("o1") || model.starts_with("o3") || model.starts_with("o4") {
        return Some(200_000);
    }
    // Cross-vendor tags served through OpenAI-compatible endpoints
    // (the compat presets and OpenRouter).
    if model.starts_with("gemini-3") || model.starts_with("gemini-2.5") {
        return Some(1_048_576);
    }
    if model.starts_with("deepseek-v4") {
        return Some(1_000_000);
    }
    if model.starts_with("kimi-k3") {
        return Some(1_000_000);
    }
    if model.starts_with("minimax-m2") {
        return Some(204_800);
    }
    None
}

/// `GET /models` response (subset).
#[derive(Debug, Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    data: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
struct ModelEntry {
    id: String,
}

/// One streamed `data: {...}` chunk from Chat Completions (subset).
#[derive(Debug, Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<Usage>,
    /// An error the endpoint reported *inside* an HTTP 200 stream.
    ///
    /// The status line is written before the model runs, so anything that goes
    /// wrong after the first byte cannot be an HTTP status any more. Every
    /// endpoint on this wire shape therefore has an in-band form, and the ones
    /// that matter most are the transient ones: OpenRouter forwards an
    /// upstream 429 or 502 this way, and xAI reports a mid-generation capacity
    /// failure the same. With no field to decode them into they parsed as a
    /// chunk with no choices and no usage — perfectly valid, silently ignored
    /// — and the stream then ended as a *successful, empty* completion. An
    /// empty completion is strictly worse than an error: the error retries,
    /// while the empty completion looks exactly like the model choosing to say
    /// nothing and ends the turn.
    #[serde(default)]
    error: Option<StreamError>,
}

/// An error object riding inside a 200 stream. Endpoints disagree about the
/// shape, so every field is optional and each is read where it lands.
#[derive(Debug, Deserialize)]
struct StreamError {
    #[serde(default)]
    message: Option<String>,
    /// The upstream HTTP status, when the gateway forwards one (OpenRouter
    /// puts the proxied provider's status here). It decides the retry class,
    /// so a 429 relayed mid-stream backs off like a 429 received up front.
    #[serde(default)]
    code: Option<Value>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
}

/// The status an in-band [`StreamError`] should be classified as.
///
/// A numeric `code` is the endpoint telling us the upstream status outright
/// and is used as-is. Otherwise the failure is attributed to the server: it
/// had already accepted the request and started generating, so whatever went
/// wrong was not something about the request that a retry would repeat. 502 is
/// the honest reading of "the thing behind the gateway broke", and it is
/// transient, which is the answer that matters.
fn stream_error_status(error: &StreamError) -> u16 {
    error
        .code
        .as_ref()
        .and_then(|code| match code {
            Value::Number(number) => number.as_u64(),
            // Several gateways send the status as a string, and some send a
            // symbolic name ("rate_limit_exceeded") that is not a status at
            // all; only the digits are believed.
            Value::String(text) => text.parse::<u64>().ok(),
            _ => None,
        })
        .and_then(|code| u16::try_from(code).ok())
        .filter(|&code| (400..600).contains(&code))
        .unwrap_or(502)
}

/// The user-facing message for an in-band [`StreamError`].
fn stream_error_message(error: &StreamError) -> String {
    let detail = error
        .message
        .clone()
        .or_else(|| error.kind.clone())
        .unwrap_or_else(|| "no detail given".to_string());
    format!("the response stream reported an error mid-generation: {detail}")
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Delta {
    /// Visible text. A plain string on every text model; image-capable
    /// endpoints send an array of content parts instead (see [`DeltaContent`]).
    #[serde(default)]
    content: Option<DeltaContent>,
    /// Reasoning ("thinking") fragments streamed before the visible text by
    /// reasoning models (xAI grok-4.3, DeepSeek R1, ...).
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCallDelta>,
    /// Generated images streamed beside the text — OpenRouter's shape for
    /// image-output models, and the one an OpenAI-compatible image endpoint
    /// most naturally emits.
    #[serde(default)]
    images: Vec<ImagePart>,
}

/// A delta's `content`: text, or an array of content parts (the multi-modal
/// shape, where an image arrives as an `image_url` part beside the text).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum DeltaContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

/// One part of a multi-modal `delta.content` array.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ContentPart {
    Text {
        text: String,
    },
    /// Anything with an image payload — an `image_url` part, or a raw
    /// `b64_json` one (see [`ImagePart`]).
    Image(ImagePart),
    /// A part shape we do not understand (a future modality). Matches last, so
    /// an unknown part is ignored rather than failing the parse of the chunk
    /// the text is riding on.
    Other(serde::de::IgnoredAny),
}

/// A generated image on the wire. Both shapes that OpenAI-compatible endpoints
/// return are accepted: an `image_url` part carrying a `data:` URI, and a raw
/// `b64_json` payload with the media type stated separately (the Images-API
/// shape, which gateways inline into the chat delta).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ImagePart {
    Url {
        image_url: ImageUrl,
    },
    B64 {
        b64_json: String,
        /// `image/png` unless the endpoint says otherwise.
        #[serde(default, alias = "mime_type", alias = "media_type")]
        mime: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
struct ImageUrl {
    url: String,
}

impl ImagePart {
    /// Decode the part into an [`Image`]. `None` (with a warning) when the
    /// payload is not a usable image — a broken or absurdly large image is
    /// dropped, never allowed to kill the stream it arrived on.
    fn decode(self) -> Option<Image> {
        let decoded = match self {
            ImagePart::Url { image_url } => Image::from_data_uri(&image_url.url),
            ImagePart::B64 { b64_json, mime } => {
                Image::from_base64(&b64_json, mime.as_deref().unwrap_or("image/png"))
            }
        };
        match decoded {
            Ok(image) => Some(image),
            Err(err) => {
                tracing::warn!("dropping a streamed image: {err}");
                None
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct ToolCallDelta {
    index: u64,
    /// The `call_…` id OpenAI issued, sent once on the delta that opens the
    /// call. It rides through history on the [`ToolCall`] and comes back as
    /// the answering message's `tool_call_id`.
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct FunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Usage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
    /// OpenAI's nesting for the cached-prefix counter.
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
    /// The same number, flattened onto `usage` itself. Several
    /// OpenAI-compatible gateways report it here instead, and reading only
    /// one of the two spellings is how a cache hit silently reads as a miss.
    #[serde(default)]
    cached_tokens: Option<u64>,
    /// DeepSeek's spelling of the same number. Its disk cache reports a
    /// hit/miss *pair* at the top of `usage` and no `prompt_tokens_details`
    /// at all, so neither shape above finds anything on a DeepSeek response.
    /// The miss half is not read: `prompt_tokens == hit + miss` is stated by
    /// DeepSeek's own docs, so the hit alone is the subset this seam wants
    /// and deriving the remainder from `prompt_tokens` cannot disagree with
    /// itself the way carrying both numbers could.
    ///
    /// Checked 2026-08-07 against api-docs.deepseek.com/guides/kv_cache.
    #[serde(default)]
    prompt_cache_hit_tokens: Option<u64>,
}

/// `usage.prompt_tokens_details` (subset): the breakdown of `prompt_tokens`.
#[derive(Debug, Deserialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
    /// OpenRouter's counterpart to `cached_tokens`, and the only cache-write
    /// count that reaches this adapter at all.
    ///
    /// OpenAI's own cache is automatic and bills no write, so this field is
    /// absent on a direct OpenAI response and stays 0. OpenRouter is
    /// different in kind: it proxies Anthropic models, whose cache writes are
    /// billed at a 1.25x premium, and it forwards that count here. Dropping
    /// it would price an OpenRouter Claude turn's cache writes as ordinary
    /// input and under-state the turn.
    ///
    /// Checked 2026-08-07 against openrouter.ai/docs/features/prompt-caching.
    #[serde(default)]
    cache_write_tokens: Option<u64>,
}

impl Usage {
    /// Prompt tokens that were served from the cache, from whichever of the
    /// three shapes the endpoint used.
    ///
    /// This is a *subset* of `prompt_tokens`, not an addition to it: OpenAI
    /// bills the cached part at a discount but still counts it, so the two
    /// must never be summed.
    fn cached_tokens(&self) -> Option<u64> {
        self.prompt_tokens_details
            .as_ref()
            .and_then(|details| details.cached_tokens)
            .or(self.cached_tokens)
            .or(self.prompt_cache_hit_tokens)
    }

    /// Prompt tokens the endpoint wrote into its cache, when it reports any.
    /// See [`PromptTokensDetails::cache_write_tokens`]; `None` everywhere
    /// except OpenRouter, which is the honest answer for a cache that is
    /// filled automatically and free.
    fn cache_write_tokens(&self) -> Option<u64> {
        self.prompt_tokens_details
            .as_ref()
            .and_then(|details| details.cache_write_tokens)
    }
}

/// Per-index accumulator for a streamed tool call.
#[derive(Debug, Default)]
struct ToolAccum {
    id: String,
    name: String,
    arguments: String,
}

/// Split a delta's `content` into its text (if any, non-empty) and the images
/// carried in it as content parts.
fn split_content(content: Option<DeltaContent>) -> (Option<String>, Vec<Image>) {
    match content {
        None => (None, Vec::new()),
        Some(DeltaContent::Text(text)) => ((!text.is_empty()).then_some(text), Vec::new()),
        Some(DeltaContent::Parts(parts)) => {
            let mut text = String::new();
            let mut images = Vec::new();
            for part in parts {
                match part {
                    ContentPart::Text { text: fragment } => text.push_str(&fragment),
                    ContentPart::Image(image) => images.extend(image.decode()),
                    ContentPart::Other(_) => {}
                }
            }
            ((!text.is_empty()).then_some(text), images)
        }
    }
}

/// Decoder state for [`decode_sse`].
struct SseState<S> {
    bytes: S,
    buf: Vec<u8>,
    /// Chunks queued behind the one being returned, when a single delta
    /// carries several things at once (reasoning *and* text, text *and* an
    /// image). Drained before the next line is parsed, so nothing is lost.
    pending: VecDeque<ChatChunk>,
    tool_calls: BTreeMap<u64, ToolAccum>,
    prompt_eval_count: Option<u64>,
    eval_count: Option<u64>,
    /// Prompt tokens the server served from its cache (a subset of
    /// `prompt_eval_count`). See [`Usage::cached_tokens`] and the note in
    /// [`build_final`].
    cached_prompt_tokens: Option<u64>,
    /// Prompt tokens the server wrote into its cache, when it reports any
    /// (OpenRouter does; nothing else on this wire shape does). Also a subset
    /// of `prompt_eval_count`. See [`Usage::cache_write_tokens`].
    cache_write_tokens: Option<u64>,
    /// Last `finish_reason` seen ("stop", "length", "tool_calls", ...).
    done_reason: Option<String>,
    /// Saw `data: [DONE]` or EOF — drain the buffer, then emit the final chunk.
    saw_done: bool,
    /// The endpoint *said* the reply was over: a `data: [DONE]` sentinel, or a
    /// `finish_reason` on a choice. Distinct from [`SseState::saw_done`],
    /// which EOF also sets, and that distinction is the whole point — a
    /// connection that dies mid-generation reaches the same EOF a completed
    /// stream does, and without this flag the decoder cannot tell the two
    /// apart and hands the agent a successful, truncated completion.
    terminated: bool,
    /// An in-band error the stream reported (see [`StreamChunk::error`]),
    /// raised once the buffer has been drained rather than the instant it is
    /// parsed, so the text that arrived before it is still delivered.
    failure: Option<StreamError>,
    /// The synthesized `done: true` chunk has been emitted.
    emitted_final: bool,
}

/// Build the final `done: true` chunk from accumulated tool-call fragments.
fn build_final<S>(state: &SseState<S>) -> ChatChunk {
    let mut tool_calls: Vec<ToolCall> = state
        .tool_calls
        .values()
        .filter(|accum| !accum.name.is_empty())
        .map(|accum| {
            let arguments = if accum.arguments.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str::<Value>(&accum.arguments)
                    .unwrap_or_else(|_| Value::String(accum.arguments.clone()))
            };
            ToolCall {
                id: accum.id.clone(),
                function: FunctionCall {
                    name: accum.name.clone(),
                    arguments,
                },
            }
        })
        .collect();
    // OpenAI itself always sends an id; a compatible server (vLLM, a proxy)
    // may not, and a result with an empty `tool_call_id` is a 400.
    crate::llm::ensure_tool_call_ids(&mut tool_calls);
    // Whether `prompt_cache_key` bought anything is only knowable from this
    // counter, and a cache that silently stopped hitting (a reordered system
    // prompt, a per-turn timestamp in the prefix) costs real money while
    // looking exactly like a cache that is working. It rides out on the chunk
    // (`ChatChunk::cache`) so the turn is billed at the cached rate, and is
    // logged besides: the cost column says the turn was cheap, not which step
    // stopped hitting.
    //
    // The write count is almost always zero and that is not a placeholder:
    // OpenAI's prompt cache is automatic and bills no write, so zero is the
    // honest reading for every endpoint that speaks plain OpenAI. OpenRouter
    // is the exception, because it proxies models (Anthropic's) whose writes
    // *are* billed, and it reports them; see
    // [`PromptTokensDetails::cache_write_tokens`].
    if let Some(cached) = state.cached_prompt_tokens.filter(|&count| count > 0) {
        tracing::debug!(
            cached_tokens = cached,
            cache_write_tokens = ?state.cache_write_tokens,
            prompt_tokens = ?state.prompt_eval_count,
            "prompt cache hit"
        );
    }
    let message = (!tool_calls.is_empty()).then(|| {
        ChatMessage::new(
            Role::Assistant,
            tool_calls.into_iter().map(ContentBlock::ToolUse).collect(),
        )
    });
    ChatChunk {
        message,
        images: Vec::new(),
        thinking: false,
        done: true,
        done_reason: state.done_reason.clone(),
        eval_count: state.eval_count,
        prompt_eval_count: state.prompt_eval_count,
        cache: CacheTokens {
            read: state.cached_prompt_tokens.unwrap_or(0),
            write: state.cache_write_tokens.unwrap_or(0),
        },
    }
}

/// A live `done: false` text chunk; `thinking` marks reasoning deltas.
fn text_chunk(content: String, thinking: bool) -> ChatChunk {
    ChatChunk {
        message: Some(ChatMessage::assistant(content)),
        images: Vec::new(),
        thinking,
        done: false,
        done_reason: None,
        eval_count: None,
        prompt_eval_count: None,
        cache: CacheTokens::NONE,
    }
}

/// A live `done: false` chunk carrying generated images. The agent loop
/// accumulates these onto the assistant message and announces them to the
/// surfaces; see [`ChatChunk::images`].
fn image_chunk(images: Vec<Image>) -> ChatChunk {
    ChatChunk {
        message: None,
        images,
        thinking: false,
        done: false,
        done_reason: None,
        eval_count: None,
        prompt_eval_count: None,
        cache: CacheTokens::NONE,
    }
}

/// Decode an OpenAI SSE byte stream into a [`ChatStream`]: text, reasoning and
/// image deltas are emitted live as `done: false` chunks; tool-call fragments
/// are accumulated per index and emitted in a single synthesized `done: true`
/// chunk at the end.
pub(crate) fn decode_sse<S>(bytes: S) -> ChatStream
where
    S: Stream<Item = Result<Vec<u8>>> + Send + Unpin + 'static,
{
    let state = SseState {
        bytes,
        buf: Vec::new(),
        pending: VecDeque::new(),
        tool_calls: BTreeMap::new(),
        prompt_eval_count: None,
        eval_count: None,
        cached_prompt_tokens: None,
        cache_write_tokens: None,
        done_reason: None,
        saw_done: false,
        terminated: false,
        failure: None,
        emitted_final: false,
    };
    stream::try_unfold(state, |mut state| async move {
        loop {
            if state.emitted_final {
                return Ok(None);
            }
            if let Some(queued) = state.pending.pop_front() {
                return Ok(Some((queued, state)));
            }
            // Drain complete lines, returning the first content delta we find.
            while let Some(pos) = state.buf.iter().position(|&b| b == b'\n') {
                let line_bytes: Vec<u8> = state.buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line_bytes);
                let line = line.trim();
                let Some(payload) = line.strip_prefix("data:") else {
                    continue;
                };
                let payload = payload.trim();
                if payload == "[DONE]" {
                    state.saw_done = true;
                    state.terminated = true;
                    continue;
                }
                let chunk: StreamChunk = match serde_json::from_str(payload) {
                    Ok(chunk) => chunk,
                    // Ignore keep-alives and anything we cannot parse.
                    Err(_) => continue,
                };
                if let Some(error) = chunk.error {
                    // Stop reading the socket, but finish draining what is
                    // already buffered: the deltas ahead of the error are real
                    // output and the agent has already been shown them.
                    state.failure = Some(error);
                    state.saw_done = true;
                    state.terminated = true;
                    continue;
                }
                if let Some(usage) = chunk.usage {
                    if let Some(cached) = usage.cached_tokens() {
                        state.cached_prompt_tokens = Some(cached);
                    }
                    if let Some(written) = usage.cache_write_tokens() {
                        state.cache_write_tokens = Some(written);
                    }
                    if let Some(prompt) = usage.prompt_tokens {
                        state.prompt_eval_count = Some(prompt);
                    }
                    if let Some(completion) = usage.completion_tokens {
                        state.eval_count = Some(completion);
                    }
                }
                if let Some(choice) = chunk.choices.into_iter().next() {
                    if let Some(reason) = choice.finish_reason {
                        state.done_reason = Some(reason);
                        // Several compatible servers send a finish reason and
                        // then simply close, with no `[DONE]` at all. That is
                        // a complete reply and must not read as a cut stream.
                        state.terminated = true;
                    }
                    for delta in choice.delta.tool_calls {
                        let accum = state.tool_calls.entry(delta.index).or_default();
                        if let Some(id) = delta.id.filter(|id| !id.is_empty()) {
                            accum.id = id;
                        }
                        if let Some(function) = delta.function {
                            if let Some(name) = function.name {
                                accum.name.push_str(&name);
                            }
                            if let Some(arguments) = function.arguments {
                                accum.arguments.push_str(&arguments);
                            }
                        }
                    }
                    // One delta can carry reasoning, text and images at once;
                    // each becomes its own chunk, queued in that order.
                    let (content, mut images) = split_content(choice.delta.content);
                    images.extend(
                        choice
                            .delta
                            .images
                            .into_iter()
                            .filter_map(ImagePart::decode),
                    );
                    let reasoning = choice
                        .delta
                        .reasoning_content
                        .filter(|text| !text.is_empty());
                    if let Some(reasoning) = reasoning {
                        state.pending.push_back(text_chunk(reasoning, true));
                    }
                    if let Some(content) = content {
                        state.pending.push_back(text_chunk(content, false));
                    }
                    if !images.is_empty() {
                        state.pending.push_back(image_chunk(images));
                    }
                    if let Some(first) = state.pending.pop_front() {
                        return Ok(Some((first, state)));
                    }
                }
            }
            if state.saw_done {
                if let Some(error) = state.failure.take() {
                    return Err(crate::llm::http_error_with_retry_after(
                        stream_error_status(&error),
                        stream_error_message(&error),
                        None,
                    ));
                }
                if !state.terminated {
                    return Err(crate::llm::stream_ended_early("the response stream"));
                }
                state.emitted_final = true;
                let final_chunk = build_final(&state);
                return Ok(Some((final_chunk, state)));
            }
            match state.bytes.next().await {
                Some(Ok(data)) => state.buf.extend_from_slice(&data),
                Some(Err(e)) => return Err(e),
                None => {
                    // EOF: flush a trailing line without a newline, then emit
                    // the final chunk on the next pass — unless nothing in the
                    // stream ever said the reply was over, in which case this
                    // EOF *is* the failure and the pass above raises it.
                    if !state.buf.is_empty() && state.buf.last() != Some(&b'\n') {
                        state.buf.push(b'\n');
                    }
                    state.saw_done = true;
                }
            }
        }
    })
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::test_support::{
        PARALLEL_TOOL_BATCH_SSE, Recorded, assert_batch_is_answerable, one_shot_http_server,
        parallel_batch_request,
    };
    use crate::llm::{ChatOptions, ToolSpec};

    fn provider() -> OpenAiProvider {
        OpenAiProvider::new("https://api.openai.com/v1/", "gpt-4o", "sk-test")
    }

    #[test]
    fn base_url_trailing_slash_is_trimmed() {
        let provider = OpenAiProvider::new("http://localhost:1234/v1///", "m", "");
        assert_eq!(
            provider.url("/chat/completions"),
            "http://localhost:1234/v1/chat/completions"
        );
        assert_eq!(provider.label(), "openai:m");
    }

    #[test]
    fn vendor_prefix_shows_in_the_label() {
        let provider = OpenAiProvider::with_token_source(
            "https://api.x.ai/v1",
            "grok-4.3",
            Arc::new(StaticToken::new("k")),
            "xai",
        );
        assert_eq!(provider.label(), "xai:grok-4.3");
    }

    #[test]
    fn with_headers_keeps_url_and_label() {
        let provider = provider().with_headers(&[
            ("HTTP-Referer", "https://example.com"),
            ("X-Title", "Wizard"),
        ]);
        assert_eq!(
            provider.url("/chat/completions"),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(provider.label(), "openai:gpt-4o");
    }

    #[test]
    fn with_headers_skips_invalid_headers() {
        // An invalid name/value must not panic or break the client.
        let provider = provider().with_headers(&[("bad header", "x"), ("X-Ok", "bad\nvalue")]);
        assert_eq!(provider.label(), "openai:gpt-4o");
    }

    #[tokio::test]
    async fn static_token_skips_the_header_when_empty() {
        assert_eq!(
            StaticToken::new("sk-test").bearer().await.expect("ok"),
            Some("sk-test".to_string())
        );
        assert_eq!(StaticToken::new("").bearer().await.expect("ok"), None);
    }

    #[test]
    fn user_images_become_image_url_parts() {
        let messages = vec![ChatMessage::user_with_images(
            "what is on screen?",
            vec![
                Image::new("QUJD", "image/png"),
                Image::new("REVG", "image/webp"),
            ],
        )];
        let out = build_messages(&messages);
        let content = &out[0]["content"];
        assert!(content.is_array(), "image-bearing content is multi-part");
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "what is on screen?");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,QUJD");
        // The media type rides with the bytes — not hard-coded to PNG.
        assert_eq!(
            content[2]["image_url"]["url"],
            "data:image/webp;base64,REVG"
        );
    }

    #[test]
    fn user_without_images_stays_a_plain_string() {
        let out = build_messages(&[ChatMessage::user("hi")]);
        assert_eq!(out[0]["content"], "hi");
    }

    #[test]
    fn assistant_images_are_named_in_the_text_not_sent_as_blocks() {
        // No chat API takes image content in an assistant turn; replaying one
        // must degrade to text rather than 400 the request.
        let mut assistant = ChatMessage::assistant("here it is");
        assistant.push_image(Image::new("QUJD", "image/png"));
        let out = build_messages(&[assistant]);
        let content = out[0]["content"].as_str().expect("plain text content");
        assert!(content.starts_with("here it is"));
        assert!(
            content.contains("generated 1 image(s) (image/png)"),
            "{content}"
        );
        assert!(
            !serde_json::to_string(&out[0])
                .unwrap()
                .contains("image_url"),
            "the image itself is dropped from the wire"
        );
    }

    #[test]
    fn translates_native_request_to_openai_shape() {
        let mut assistant = ChatMessage::assistant("");
        assistant.push_tool_call(ToolCall::new("read_file", json!({ "path": "src/main.rs" })));
        let call_id = assistant.tool_calls()[0].id.clone();
        let request = ChatRequest {
            model: "gpt-4o".to_string(),
            messages: vec![
                ChatMessage::system("You are Wizard."),
                ChatMessage::user("read it"),
                assistant,
                ChatMessage::tool_result(&call_id, "read_file", "fn main() {}"),
            ],
            tools: vec![ToolSpec::function(
                "read_file",
                "Read a file.",
                json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
            )],
            stream: true,
            options: Some(ChatOptions {
                temperature: Some(0.7),
                num_ctx: None,
                reasoning_effort: None,
            }),
        };

        let body = provider().build_request_body(&request);
        assert_eq!(body["model"], "gpt-4o");
        assert_eq!(body["stream"], true);
        assert_eq!(
            body["stream_options"]["include_usage"], true,
            "usage must be requested on SSE streams"
        );
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["role"], "user");

        let assistant = &body["messages"][2];
        assert_eq!(assistant["role"], "assistant");
        assert!(assistant["content"].is_null(), "tool-only content is null");
        let call = &assistant["tool_calls"][0];
        assert_eq!(call["type"], "function");
        assert_eq!(call["function"]["name"], "read_file");
        // arguments are serialized to a JSON *string* for OpenAI.
        assert_eq!(call["id"], call_id, "the provider's own id, verbatim");
        let args = call["function"]["arguments"]
            .as_str()
            .expect("arguments serialized to a string");
        assert!(args.contains("src/main.rs"));

        let tool_msg = &body["messages"][3];
        assert_eq!(tool_msg["role"], "tool");
        assert_eq!(
            tool_msg["tool_call_id"], call_id,
            "result correlates to the call"
        );
        assert_eq!(tool_msg["content"], "fn main() {}");

        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "read_file");
        let temperature = body["temperature"].as_f64().expect("temperature number");
        assert!((temperature - 0.7).abs() < 1e-6);
    }

    /// A two-call batch, which both Claude and GPT emit by default and for
    /// which there was no test anywhere.
    ///
    /// This API wants one `tool` message per result, and it wants them
    /// consecutive: a user message (the images payload) or a system message
    /// (the failure nudge) spliced between two results is rejected, and the
    /// agent loop used to push exactly that. The batch is answered by one
    /// [`ChatMessage`] here, so the only thing that can come after the results
    /// is another turn.
    #[test]
    fn a_parallel_batch_becomes_consecutive_tool_messages_correlated_by_id() {
        let mut assistant = ChatMessage::assistant("");
        assistant.push_tool_call(ToolCall::new("read_file", json!({ "path": "a" })));
        assistant.push_tool_call(ToolCall::new("read_file", json!({ "path": "b" })));
        let ids: Vec<String> = assistant
            .tool_calls()
            .iter()
            .map(|call| call.id.clone())
            .collect();
        let mut results = ChatMessage::tool_result(&ids[0], "read_file", "contents of a");
        results.push_tool_result(&ids[1], "read_file", "contents of b");

        let out = build_messages(&[ChatMessage::user("read both"), assistant, results]);
        assert_eq!(out.len(), 4, "user, assistant, then one message per result");
        assert_eq!(out[1]["tool_calls"][0]["id"], ids[0]);
        assert_eq!(out[1]["tool_calls"][1]["id"], ids[1]);
        // Both calls name the same tool, so a name-plus-order match cannot
        // tell them apart; the ids can.
        assert_eq!(out[2]["role"], "tool");
        assert_eq!(out[2]["tool_call_id"], ids[0]);
        assert_eq!(out[2]["content"], "contents of a");
        assert_eq!(out[3]["role"], "tool");
        assert_eq!(out[3]["tool_call_id"], ids[1]);
        assert_eq!(out[3]["content"], "contents of b");
    }

    /// The whole reason tool-call ids exist. Both calls in this batch name
    /// `read_file`, so "which result answers which call" has no answer in the
    /// tool name; only the id distinguishes them, and getting it wrong feeds
    /// the model file `a`'s contents as the answer to the read of `b`.
    #[test]
    fn two_calls_to_the_same_tool_stay_distinguishable_by_id() {
        let request = parallel_batch_request("gpt-4o");
        let out = build_messages(&request.messages);

        assert_batch_is_answerable(&out, 2);
        assert_eq!(out[2]["tool_calls"][0]["function"]["name"], "read_file");
        assert_eq!(out[2]["tool_calls"][1]["function"]["name"], "read_file");
        // The bodies are what makes a mix-up observable: swap the two
        // `tool_call_id`s and these two lines are what catches it.
        assert_eq!(out[3]["content"], "contents of a");
        assert_eq!(out[4]["content"], "contents of b");
    }

    /// The other half of the contract: a real request, over a real socket,
    /// answered by a recorded two-call stream.
    ///
    /// `build_messages` tests assert what this adapter *believes* it sends.
    /// This one asserts what it actually sent, which is the only place a body
    /// the API would reject with an HTTP 400 can be caught.
    #[tokio::test]
    async fn a_parallel_batch_round_trips_over_a_recorded_stream() {
        let recorded = Recorded::replay(PARALLEL_TOOL_BATCH_SSE).await;
        let provider = OpenAiProvider::new(format!("{}/v1", recorded.root), "gpt-4o", "sk-test");

        let mut stream = provider
            .chat_stream(parallel_batch_request("gpt-4o"))
            .await
            .expect("stream opens");
        let mut chunks = Vec::new();
        while let Some(item) = stream.next().await {
            chunks.push(item.expect("chunk decodes"));
        }

        // What went out: the batch this API accepts.
        let sent = recorded.request_body();
        let messages = sent["messages"].as_array().expect("messages array");
        assert_batch_is_answerable(messages, 2);
        assert_eq!(messages.len(), 5, "system, user, assistant, result, result");

        // What came back: two calls, both `read_file`, told apart by id.
        let last = chunks.last().expect("a final chunk");
        assert!(last.done);
        assert_eq!(last.done_reason.as_deref(), Some("tool_calls"));
        let calls = last
            .message
            .as_ref()
            .expect("tool call message")
            .tool_calls();
        assert_eq!(calls.len(), 2, "the batch is not collapsed into one call");
        assert_eq!(calls[0].id, "call_aaa");
        assert_eq!(calls[1].id, "call_bbb");
        assert_eq!(calls[0].function.name, calls[1].function.name);
        assert_eq!(calls[0].function.arguments["path"], "a");
        assert_eq!(calls[1].function.arguments["path"], "b");
        assert_eq!(last.prompt_eval_count, Some(2048));
        assert_eq!(last.eval_count, Some(31));
    }

    /// `prompt_cache_key` is only worth sending if the cache hits, and this
    /// counter is the only evidence either way. Endpoints disagree about
    /// where it rides, and reading one spelling makes a hit read as a miss.
    #[test]
    fn cached_tokens_are_read_from_every_usage_shape() {
        let parse = |raw: &str| serde_json::from_str::<Usage>(raw).expect("usage parses");

        // OpenAI's own shape.
        let nested = parse(
            r#"{"prompt_tokens":2048,"completion_tokens":31,"prompt_tokens_details":{"cached_tokens":1920}}"#,
        );
        assert_eq!(nested.cached_tokens(), Some(1920));
        assert_eq!(
            nested.prompt_tokens,
            Some(2048),
            "cached tokens are a subset of the prompt, never an addition to it"
        );
        assert_eq!(
            nested.cache_write_tokens(),
            None,
            "OpenAI's cache is automatic and reports no write"
        );

        // The flattened shape several compatible gateways report.
        assert_eq!(
            parse(r#"{"prompt_tokens":2048,"cached_tokens":1920}"#).cached_tokens(),
            Some(1920)
        );

        // DeepSeek's hit/miss pair, with no `prompt_tokens_details` at all.
        // Its own docs state `prompt_tokens == hit + miss`, so the hit is
        // already the subset this seam wants.
        let deepseek = parse(
            r#"{"prompt_tokens":2048,"completion_tokens":31,"prompt_cache_hit_tokens":1920,"prompt_cache_miss_tokens":128}"#,
        );
        assert_eq!(deepseek.cached_tokens(), Some(1920));
        assert_eq!(
            deepseek.prompt_tokens.expect("prompt tokens"),
            1920 + 128,
            "DeepSeek's hit and miss halves sum to the prompt it reported"
        );

        // OpenRouter reports the write half beside the read half. It is the
        // only endpoint on this wire shape that has one to report, because it
        // proxies Anthropic, whose writes are billed at a premium.
        let openrouter = parse(
            r#"{"prompt_tokens":10339,"completion_tokens":60,"prompt_tokens_details":{"cached_tokens":10318,"cache_write_tokens":21}}"#,
        );
        assert_eq!(openrouter.cached_tokens(), Some(10_318));
        assert_eq!(openrouter.cache_write_tokens(), Some(21));

        // A cold prompt reports the field as zero, not as absent.
        assert_eq!(
            parse(r#"{"prompt_tokens":2048,"prompt_tokens_details":{"cached_tokens":0}}"#)
                .cached_tokens(),
            Some(0)
        );
        // A backend that reports no breakdown at all still parses.
        let bare = parse(r#"{"prompt_tokens":11,"completion_tokens":4}"#);
        assert_eq!(bare.cached_tokens(), None);
        assert_eq!(bare.cache_write_tokens(), None);
    }

    /// DeepSeek is the reason this adapter reads three spellings rather than
    /// two: its disk cache discounts a hit to about 2% of the miss rate, so a
    /// hit decoded as a miss over-bills the cached portion roughly fiftyfold,
    /// and the two shapes that existed before find nothing on its response.
    #[tokio::test]
    async fn deepseeks_hit_and_miss_pair_reaches_the_chunk() {
        let parts: Vec<Result<Vec<u8>>> = vec![Ok(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":2048,\"completion_tokens\":31,\"prompt_cache_hit_tokens\":1920,\"prompt_cache_miss_tokens\":128}}\n\ndata: [DONE]\n\n"
                .to_vec(),
        )];
        let mut chunks = decode_sse(stream::iter(parts));
        let mut last = None;
        while let Some(chunk) = chunks.next().await {
            last = Some(chunk.expect("chunk decodes"));
        }
        let last = last.expect("a final chunk");
        assert_eq!(last.prompt_eval_count, Some(2048));
        assert_eq!(
            last.cache,
            CacheTokens {
                read: 1_920,
                // DeepSeek's cache fills as a side effect of the miss and is
                // billed at the plain miss rate, so there is no write to bill.
                write: 0
            },
        );
    }

    /// OpenRouter proxying an Anthropic model is the one place on this wire
    /// shape where a cache *write* exists. Anthropic bills it at 1.25x input,
    /// so dropping it prices the write as ordinary input and under-states the
    /// turn — the one direction the cost column must never fail in.
    #[tokio::test]
    async fn openrouters_cache_write_count_reaches_the_chunk() {
        let parts: Vec<Result<Vec<u8>>> = vec![Ok(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10339,\"completion_tokens\":60,\"prompt_tokens_details\":{\"cached_tokens\":10318,\"cache_write_tokens\":21}}}\n\ndata: [DONE]\n\n"
                .to_vec(),
        )];
        let mut chunks = decode_sse(stream::iter(parts));
        let mut last = None;
        while let Some(chunk) = chunks.next().await {
            last = Some(chunk.expect("chunk decodes"));
        }
        let last = last.expect("a final chunk");
        assert_eq!(last.prompt_eval_count, Some(10_339));
        assert_eq!(
            last.cache,
            CacheTokens {
                read: 10_318,
                write: 21
            },
        );
        assert!(
            last.cache.read + last.cache.write <= last.prompt_eval_count.expect("prompt"),
            "both counts are subsets of the prompt, never additions to it"
        );
    }

    /// Collects the text of every `tracing` event emitted on this thread
    /// while a [`tracing::subscriber::DefaultGuard`] built on it is alive.
    #[derive(Clone, Default)]
    struct LogCapture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("log lock").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    impl LogCapture {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().expect("log lock")).into_owned()
        }
    }

    /// The counter reaching the end of the stream it arrived on.
    ///
    /// Cache accounting is the only way to tell a `prompt_cache_key` that is
    /// working from one that silently stopped matching, and the payload it
    /// rides on is the same one the token counts ride on, so dropping it is
    /// invisible everywhere else.
    ///
    /// Both halves are asserted: the count on the chunk, which is what stops
    /// the turn being billed as 2,048 fresh input tokens when 1,920 of them
    /// were served from the cache, and the log line, which is what says
    /// *which step* stopped hitting once the cost column has gone quiet.
    #[tokio::test]
    async fn a_cache_hit_reaches_the_end_of_the_stream_it_arrived_on() {
        use tracing_subscriber::layer::SubscriberExt as _;

        let capture = LogCapture::default();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::new("wizard=debug"))
            .with(tracing_subscriber::fmt::layer().with_writer(capture.clone()));
        let guard = tracing::subscriber::set_default(subscriber);

        let parts: Vec<Result<Vec<u8>>> = vec![Ok(PARALLEL_TOOL_BATCH_SSE.as_bytes().to_vec())];
        let mut chunks = decode_sse(stream::iter(parts));
        let mut last = None;
        while let Some(chunk) = chunks.next().await {
            last = Some(chunk.expect("chunk decodes"));
        }
        drop(guard);

        let last = last.expect("a final chunk");
        assert_eq!(last.prompt_eval_count, Some(2048));
        assert_eq!(last.eval_count, Some(31));
        assert_eq!(
            last.cache,
            CacheTokens {
                read: 1_920,
                // OpenAI's prompt cache is automatic and bills no cache
                // write, so there is no honest number to put here but zero.
                write: 0
            },
            "the cached count has to leave the adapter, not just be logged"
        );

        let logged = capture.text();
        assert!(logged.contains("prompt cache hit"), "{logged}");
        assert!(
            logged.contains("cached_tokens=1920"),
            "the cached count itself, not just that something was cached: {logged}"
        );
        assert!(
            logged.contains("prompt_tokens=Some(2048)"),
            "a cached count is only readable next to the prompt it is part of: {logged}"
        );
    }

    #[tokio::test]
    async fn a_cold_prompt_reports_no_cache_hit() {
        use tracing_subscriber::layer::SubscriberExt as _;

        // `cached_tokens: 0` is what a cold prompt reports, and it is not a
        // hit: logging one would make every first turn look cached.
        let capture = LogCapture::default();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::new("wizard=debug"))
            .with(tracing_subscriber::fmt::layer().with_writer(capture.clone()));
        let guard = tracing::subscriber::set_default(subscriber);

        let parts: Vec<Result<Vec<u8>>> = vec![Ok(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":9,\"prompt_tokens_details\":{\"cached_tokens\":0}}}\n\ndata: [DONE]\n\n"
                .to_vec(),
        )];
        let mut chunks = decode_sse(stream::iter(parts));
        let mut last = None;
        while let Some(chunk) = chunks.next().await {
            last = Some(chunk.expect("chunk decodes"));
        }
        drop(guard);

        assert!(
            !capture.text().contains("prompt cache hit"),
            "{}",
            capture.text()
        );
        assert_eq!(
            last.expect("a final chunk").cache,
            CacheTokens::NONE,
            "a cold prompt prices as all-fresh, which is what it was"
        );
    }

    #[test]
    fn reasoning_models_omit_temperature() {
        let options = Some(ChatOptions {
            temperature: Some(0.2),
            num_ctx: None,
            reasoning_effort: None,
        });
        for model in ["gpt-5", "gpt-5-mini", "o1", "o3-mini", "o4-mini", "O3"] {
            let request = ChatRequest {
                model: model.to_string(),
                messages: vec![ChatMessage::user("hi")],
                tools: Vec::new(),
                stream: true,
                options: options.clone(),
            };
            let body = provider().build_request_body(&request);
            assert!(
                body.get("temperature").is_none(),
                "{model} must not receive temperature"
            );
        }
        // Non-reasoning models keep it.
        let request = ChatRequest {
            model: "gpt-4o".to_string(),
            messages: vec![ChatMessage::user("hi")],
            tools: Vec::new(),
            stream: true,
            options,
        };
        let body = provider().build_request_body(&request);
        assert!(body.get("temperature").is_some());
    }

    #[test]
    fn reasoning_effort_is_sent_only_for_supporting_models() {
        let options = Some(ChatOptions {
            temperature: Some(0.7),
            num_ctx: None,
            reasoning_effort: Some("high".to_string()),
        });
        // Forwarded for xAI Grok 4.x and OpenAI reasoning families.
        for model in ["grok-4.5", "grok-4.3", "gpt-5", "o3-mini", "o4-mini"] {
            let request = ChatRequest {
                model: model.to_string(),
                messages: vec![ChatMessage::user("hi")],
                tools: Vec::new(),
                stream: true,
                options: options.clone(),
            };
            let body = provider().build_request_body(&request);
            assert_eq!(
                body["reasoning_effort"], "high",
                "{model} must receive reasoning_effort"
            );
        }
        // Omitted for models that would 400 on it — including non-reasoning
        // tags inside an otherwise supporting family.
        for model in [
            "gpt-4o",
            "grok-code-fast-1",
            "grok-3",
            "qwen3-8b",
            "grok-4.20-0309-non-reasoning",
            "GROK-4.20-0309-NON-REASONING",
        ] {
            let request = ChatRequest {
                model: model.to_string(),
                messages: vec![ChatMessage::user("hi")],
                tools: Vec::new(),
                stream: true,
                options: options.clone(),
            };
            let body = provider().build_request_body(&request);
            assert!(
                body.get("reasoning_effort").is_none(),
                "{model} must not receive reasoning_effort"
            );
        }
        // Absent when unset, even on a supporting model.
        let request = ChatRequest {
            model: "grok-4.5".to_string(),
            messages: vec![ChatMessage::user("hi")],
            tools: Vec::new(),
            stream: true,
            options: Some(ChatOptions {
                temperature: Some(0.7),
                num_ctx: None,
                reasoning_effort: None,
            }),
        };
        let body = provider().build_request_body(&request);
        assert!(body.get("reasoning_effort").is_none());
    }

    #[tokio::test]
    async fn http_failures_downcast_to_provider_error() {
        let provider = scripted_provider(
            "HTTP/1.1 429 Too Many Requests\r\nContent-Type: text/plain\r\nContent-Length: \
             10\r\nConnection: close\r\n\r\nslow down!",
        )
        .await;
        let err = failed_chat(&provider).await;
        let provider_err = err
            .downcast_ref::<ProviderError>()
            .expect("typed provider error");
        assert_eq!(provider_err.status, Some(429));
        assert!(provider_err.is_transient());
        assert!(provider_err.message.contains("slow down"), "body surfaces");

        let provider = scripted_provider(
            "HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain\r\nContent-Length: \
             3\r\nConnection: close\r\n\r\nbad",
        )
        .await;
        let err = failed_chat(&provider).await;
        let provider_err = err.downcast_ref::<ProviderError>().expect("typed");
        assert_eq!(provider_err.status, Some(400));
        assert!(!provider_err.is_transient());
    }

    #[tokio::test]
    async fn decodes_sse_with_split_tool_call() {
        // A content delta, then a tool call whose arguments span two fragments.
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(b"data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n".to_vec()),
            Ok(
                b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_x\",\"function\":{\"name\":\"execute\",\"arguments\":\"{\\\"command\\\":\"}}]}}]}\n\n"
                    .to_vec(),
            ),
            Ok(
                b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"ls\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4}}\n\n"
                    .to_vec(),
            ),
            Ok(b"data: [DONE]\n\n".to_vec()),
        ];
        let mut chunks = decode_sse(stream::iter(parts));

        let first = chunks.next().await.expect("content").expect("ok");
        assert!(!first.done);
        assert_eq!(first.message.expect("message").text(), "Hi");

        let last = chunks.next().await.expect("final").expect("ok");
        assert!(last.done);
        assert_eq!(last.done_reason.as_deref(), Some("tool_calls"));
        assert_eq!(last.eval_count, Some(4));
        assert_eq!(last.prompt_eval_count, Some(11));
        let message = last.message.expect("tool call message");
        assert_eq!(message.tool_calls().len(), 1);
        assert_eq!(message.tool_calls()[0].function.name, "execute");
        assert_eq!(message.tool_calls()[0].function.arguments["command"], "ls");
        assert_eq!(
            message.tool_calls()[0].id,
            "call_x",
            "the id the stream carried, sent only on the delta that opens the call"
        );

        assert!(chunks.next().await.is_none(), "stream ends after done");
    }

    /// A compatible server that streams a batch without ids.
    ///
    /// OpenAI itself always sends `call_…`, but vLLM, some proxies and the
    /// llama.cpp grammar-constrained implementation have all shipped releases
    /// that omit it, and both of these calls name the same tool. Without an id
    /// minted here the two results are indistinguishable downstream, and the
    /// `tool` messages they become carry an empty `tool_call_id`, which the
    /// strict servers in this family answer with an HTTP 400.
    #[tokio::test]
    async fn an_id_less_batch_from_a_compatible_server_gets_ids_at_the_seam() {
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(
                b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a\\\"}\"}},{\"index\":1,\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"b\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n"
                    .to_vec(),
            ),
            Ok(b"data: [DONE]\n\n".to_vec()),
        ];
        let mut chunks = decode_sse(stream::iter(parts));
        let mut last = None;
        while let Some(chunk) = chunks.next().await {
            last = Some(chunk.expect("chunk decodes"));
        }

        let message = last.expect("a final chunk").message.expect("tool calls");
        let calls = message.tool_calls();
        assert_eq!(calls.len(), 2, "the batch is not collapsed");
        assert!(!calls[0].id.is_empty(), "an id was minted: {:?}", calls[0]);
        assert!(!calls[1].id.is_empty(), "an id was minted: {:?}", calls[1]);
        assert_ne!(
            calls[0].id, calls[1].id,
            "two calls to one tool must not share an id"
        );

        // And the minted ids are what the answering messages are bound by, so
        // the round trip is answerable rather than merely non-empty.
        let mut assistant = ChatMessage::assistant("");
        for call in calls {
            assistant.push_tool_call(call.clone());
        }
        let mut results = ChatMessage::tool_result(&message.tool_calls()[0].id, "read_file", "a");
        results.push_tool_result(&message.tool_calls()[1].id, "read_file", "b");
        assert_batch_is_answerable(&build_messages(&[assistant, results]), 0);
    }

    #[tokio::test]
    async fn decodes_xai_reasoning_content_as_thinking() {
        // Real xAI grok-4.3 stream shape: `delta.reasoning_content` fragments
        // first, then the visible `delta.content`.
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(
                b"data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"grok-4.3\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"reasoning_content\":\"Weighing the \"},\"finish_reason\":null}]}\n\n"
                    .to_vec(),
            ),
            Ok(
                b"data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"grok-4.3\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"options.\"},\"finish_reason\":null}]}\n\n"
                    .to_vec(),
            ),
            Ok(
                b"data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"grok-4.3\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Done.\"},\"finish_reason\":\"stop\"}]}\n\n"
                    .to_vec(),
            ),
            Ok(b"data: [DONE]\n\n".to_vec()),
        ];
        let mut chunks = decode_sse(stream::iter(parts));

        let first = chunks.next().await.expect("reasoning").expect("ok");
        assert!(first.thinking, "reasoning delta is flagged");
        assert_eq!(first.message.expect("message").text(), "Weighing the ");

        let second = chunks.next().await.expect("reasoning").expect("ok");
        assert!(second.thinking);
        assert_eq!(second.message.expect("message").text(), "options.");

        let third = chunks.next().await.expect("content").expect("ok");
        assert!(!third.thinking, "visible text is not flagged");
        assert_eq!(third.message.expect("message").text(), "Done.");

        let last = chunks.next().await.expect("final").expect("ok");
        assert!(last.done);
        assert_eq!(last.done_reason.as_deref(), Some("stop"));
        assert!(chunks.next().await.is_none());
    }

    #[tokio::test]
    async fn reasoning_only_completion_yields_empty_final_chunk() {
        // grok-4.3 sometimes thinks and then just stops (no text, no tools).
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(
                b"data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"hmm\"},\"finish_reason\":\"stop\"}]}\n\n"
                    .to_vec(),
            ),
            Ok(b"data: [DONE]\n\n".to_vec()),
        ];
        let mut chunks = decode_sse(stream::iter(parts));

        let first = chunks.next().await.expect("reasoning").expect("ok");
        assert!(first.thinking);

        let last = chunks.next().await.expect("final").expect("ok");
        assert!(last.done);
        assert!(last.message.is_none(), "no visible message was produced");
        assert!(chunks.next().await.is_none());
    }

    #[tokio::test]
    async fn reasoning_and_content_in_one_delta_keeps_both() {
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(
                b"data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"why\",\"content\":\"Hi\"}}]}\n\n"
                    .to_vec(),
            ),
            Ok(b"data: [DONE]\n\n".to_vec()),
        ];
        let mut chunks = decode_sse(stream::iter(parts));

        let first = chunks.next().await.expect("reasoning").expect("ok");
        assert!(first.thinking);
        assert_eq!(first.message.expect("message").text(), "why");

        let second = chunks.next().await.expect("content").expect("ok");
        assert!(!second.thinking);
        assert_eq!(second.message.expect("message").text(), "Hi");

        let last = chunks.next().await.expect("final").expect("ok");
        assert!(last.done);
        assert!(chunks.next().await.is_none());
    }

    #[tokio::test]
    async fn decodes_generated_images_from_the_delta_images_array() {
        // OpenRouter's shape for image-output models: `delta.images`, each a
        // data URI. Text and image arrive in one delta; both must survive.
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"here you go\",\"images\":[{\"type\":\"image_url\",\"image_url\":{\"url\":\"data:image/png;base64,QUJD\"}}]},\"finish_reason\":\"stop\"}]}\n\n"
                    .to_vec(),
            ),
            Ok(b"data: [DONE]\n\n".to_vec()),
        ];
        let mut chunks = decode_sse(stream::iter(parts));

        let text = chunks.next().await.expect("text").expect("ok");
        assert_eq!(text.message.expect("message").text(), "here you go");
        assert!(text.images.is_empty());

        let image = chunks.next().await.expect("image").expect("ok");
        assert!(!image.done, "images stream live, like text");
        assert_eq!(image.images.len(), 1);
        assert_eq!(image.images[0].mime, "image/png");
        assert_eq!(image.images[0].b64, "QUJD");

        let last = chunks.next().await.expect("final").expect("ok");
        assert!(last.done);
        assert!(chunks.next().await.is_none());
    }

    #[tokio::test]
    async fn decodes_image_content_parts_and_b64_json_payloads() {
        // The multi-modal `delta.content` array, with both accepted image
        // shapes: an `image_url` part and a raw `b64_json` one.
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(
                b"data: {\"choices\":[{\"delta\":{\"content\":[{\"type\":\"text\",\"text\":\"two:\"},{\"type\":\"image_url\",\"image_url\":{\"url\":\"data:image/jpeg;base64,/9j/\"}},{\"b64_json\":\"R0lGODlh\",\"mime_type\":\"image/gif\"}]}}]}\n\n"
                    .to_vec(),
            ),
            Ok(b"data: [DONE]\n\n".to_vec()),
        ];
        let mut chunks = decode_sse(stream::iter(parts));

        let text = chunks.next().await.expect("text").expect("ok");
        assert_eq!(text.message.expect("message").text(), "two:");

        let images = chunks.next().await.expect("images").expect("ok");
        assert_eq!(images.images.len(), 2);
        assert_eq!(images.images[0].mime, "image/jpeg");
        assert_eq!(images.images[0].b64, "/9j/");
        assert_eq!(images.images[1].mime, "image/gif");
        assert_eq!(images.images[1].b64, "R0lGODlh");

        assert!(chunks.next().await.expect("final").expect("ok").done);
        assert!(chunks.next().await.is_none());
    }

    #[tokio::test]
    async fn a_broken_image_payload_is_dropped_not_fatal() {
        // A malformed image must not kill the stream the text is riding on.
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"still here\",\"images\":[{\"type\":\"image_url\",\"image_url\":{\"url\":\"https://example.com/cat.png\"}}]}}]}\n\n"
                    .to_vec(),
            ),
            Ok(b"data: [DONE]\n\n".to_vec()),
        ];
        let mut chunks = decode_sse(stream::iter(parts));

        let text = chunks.next().await.expect("text").expect("ok");
        assert_eq!(text.message.expect("message").text(), "still here");
        let last = chunks.next().await.expect("final").expect("ok");
        assert!(last.done, "the unusable image is dropped, the stream lives");
        assert!(chunks.next().await.is_none());
    }

    /// A stream that stops before the endpoint says the reply is over is a
    /// failure, not a short answer.
    ///
    /// Every decoder here synthesizes its own final chunk, so an EOF used to
    /// produce a clean `done: true` carrying whatever text had arrived. That
    /// is the worst available reading of a dropped connection: the agent gets
    /// a well-formed completion, ends the turn, and nothing anywhere reports a
    /// problem — which is what "it randomly stops" looks like from the user's
    /// side. It has to be typed and transient so the retry ladder climbs.
    #[tokio::test]
    async fn a_stream_cut_before_it_finished_is_a_transient_failure() {
        let parts: Vec<Result<Vec<u8>>> = vec![Ok(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"half a sen\"}}]}\n\n".to_vec(),
        )];
        let mut chunks = decode_sse(stream::iter(parts));

        // What arrived still streams: it was already on screen, and
        // `AgentEvent::StreamRetrying` is what discards it on the retry.
        let first = chunks.next().await.expect("text").expect("ok");
        assert_eq!(first.message.expect("message").text(), "half a sen");

        let err = chunks
            .next()
            .await
            .expect("an item")
            .expect_err("a cut stream is not a completed reply");
        let provider = err
            .downcast_ref::<ProviderError>()
            .expect("typed, or the ladder cannot classify it");
        assert_eq!(provider.status, None, "no status was ever received");
        assert!(
            provider.is_transient(),
            "a dropped connection is exactly what a retry is for"
        );
        assert!(chunks.next().await.is_none());
    }

    /// A `finish_reason` with no `[DONE]` after it is a complete reply. Plenty
    /// of compatible servers (vLLM behind a proxy, some gateways) never send
    /// the sentinel, and refusing those streams would turn every turn against
    /// them into an endless retry.
    #[tokio::test]
    async fn a_finish_reason_without_the_done_sentinel_ends_the_stream_cleanly() {
        let parts: Vec<Result<Vec<u8>>> = vec![Ok(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"all of it\"},\"finish_reason\":\"stop\"}]}\n\n"
                .to_vec(),
        )];
        let mut chunks = decode_sse(stream::iter(parts));
        assert_eq!(
            chunks
                .next()
                .await
                .expect("text")
                .expect("ok")
                .message
                .expect("message")
                .text(),
            "all of it"
        );
        let last = chunks.next().await.expect("final").expect("ok");
        assert!(last.done);
        assert_eq!(last.done_reason.as_deref(), Some("stop"));
        assert!(chunks.next().await.is_none());
    }

    /// An error object *inside* a 200 stream.
    ///
    /// The status line is written before the model runs, so a gateway that
    /// loses its upstream two seconds in has nowhere to put the failure but
    /// the stream body. There was no field to decode it into, so it parsed as
    /// a chunk with no choices, was ignored, and the stream ended as a
    /// successful *empty* completion — which the agent reads as the model
    /// choosing to say nothing, and does not retry.
    #[tokio::test]
    async fn an_error_object_inside_a_200_stream_fails_the_stream() {
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(b"data: {\"choices\":[{\"delta\":{\"content\":\"working\"}}]}\n\n".to_vec()),
            Ok(
                b"data: {\"error\":{\"message\":\"upstream is rate limited\",\"code\":429}}\n\n"
                    .to_vec(),
            ),
        ];
        let mut chunks = decode_sse(stream::iter(parts));

        // The text ahead of the error is still delivered.
        let first = chunks.next().await.expect("text").expect("ok");
        assert_eq!(first.message.expect("message").text(), "working");

        let err = chunks
            .next()
            .await
            .expect("an item")
            .expect_err("an in-band error is an error");
        let provider = err.downcast_ref::<ProviderError>().expect("typed");
        assert_eq!(
            provider.status,
            Some(429),
            "a relayed 429 backs off like a 429 received up front"
        );
        assert!(provider.is_transient());
        assert!(provider.message.contains("upstream is rate limited"));
    }

    /// The shapes gateways actually send an in-band error in, and what each
    /// one classifies as. The default matters most: an error with nothing
    /// numeric on it is attributed to the server, because the request had
    /// already been accepted and generation had already started, so nothing
    /// about the request is what failed.
    #[test]
    fn in_band_error_shapes_all_classify_as_something_retryable() {
        let parse = |raw: &str| {
            serde_json::from_str::<StreamChunk>(raw)
                .expect("parses")
                .error
                .expect("an error object")
        };

        // A numeric status, and the string spelling several gateways use.
        assert_eq!(
            stream_error_status(&parse(r#"{"error":{"code":502,"message":"bad gateway"}}"#)),
            502
        );
        assert_eq!(
            stream_error_status(&parse(r#"{"error":{"code":"503","message":"down"}}"#)),
            503
        );
        // A symbolic code is not a status; it must not be parsed as one.
        assert_eq!(
            stream_error_status(&parse(
                r#"{"error":{"code":"rate_limit_exceeded","message":"slow down"}}"#
            )),
            502
        );
        // Nonsense outside the status range never becomes the classification.
        assert_eq!(
            stream_error_status(&parse(r#"{"error":{"code":0,"message":"x"}}"#)),
            502
        );
        assert_eq!(
            stream_error_status(&parse(r#"{"error":{"type":"server_error"}}"#)),
            502
        );
        // Every one of them is retryable, which is the property that matters.
        for raw in [
            r#"{"error":{"code":502}}"#,
            r#"{"error":{"code":"503"}}"#,
            r#"{"error":{"type":"server_error"}}"#,
            r#"{"error":{}}"#,
        ] {
            let error = parse(raw);
            assert!(
                ProviderError::http(stream_error_status(&error), "x").is_transient(),
                "{raw}"
            );
        }
        // The type name stands in when there is no message.
        assert!(
            stream_error_message(&parse(r#"{"error":{"type":"server_error"}}"#))
                .contains("server_error")
        );
        // A 400 relayed in-band stays permanent: the request really was bad.
        assert!(
            !ProviderError::http(
                stream_error_status(&parse(r#"{"error":{"code":400,"message":"bad"}}"#)),
                "x"
            )
            .is_transient()
        );
    }

    #[tokio::test]
    async fn malformed_lines_and_comments_are_skipped() {
        // SSE keep-alive comments and unparseable payloads must not end or
        // fail the stream the real deltas are riding on.
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(b": keep-alive\n\n".to_vec()),
            Ok(b"data: {broken json\n\n".to_vec()),
            Ok(b"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n".to_vec()),
            Ok(b"data: [DONE]\n\n".to_vec()),
        ];
        let mut chunks = decode_sse(stream::iter(parts));

        let first = chunks.next().await.expect("content").expect("ok");
        assert_eq!(first.message.expect("message").text(), "ok");
        assert!(chunks.next().await.expect("final").expect("ok").done);
        assert!(chunks.next().await.is_none());
    }

    /// A provider pointed at a one-shot fixture server that answers the next
    /// request with `response` verbatim.
    async fn scripted_provider(response: &'static str) -> OpenAiProvider {
        let root = one_shot_http_server(response).await;
        OpenAiProvider::new(format!("{root}/v1"), "gpt-4o", "sk-test")
    }

    /// The error from one chat completion that must fail. Written out rather
    /// than `expect_err` because [`ChatStream`] is not `Debug`.
    async fn failed_chat(provider: &OpenAiProvider) -> anyhow::Error {
        let request = ChatRequest {
            model: "gpt-4o".to_string(),
            messages: vec![ChatMessage::user("hi")],
            tools: Vec::new(),
            stream: true,
            options: None,
        };
        match provider.chat_stream(request).await {
            Ok(_) => panic!("the scripted response must fail the request"),
            Err(err) => err,
        }
    }

    #[tokio::test]
    async fn a_rate_limited_completion_carries_the_servers_retry_after() {
        // The failure this prevents: OpenRouter (or OpenAI, or any
        // compatible endpoint) answers a chat completion `429` with
        // `Retry-After: 60`, the error reaches the agent's retry loop with
        // nothing under it, and the loop sleeps a ladder draw of a few
        // seconds instead of the minute the server asked for, re-billing the
        // prompt into another 429.
        let provider = scripted_provider(
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 60\r\nContent-Type: \
             application/json\r\nContent-Length: 24\r\nConnection: close\r\n\r\n\
             {\"error\":\"rate limited\"}",
        )
        .await;
        let err = failed_chat(&provider).await;

        let status = err
            .downcast_ref::<ProviderError>()
            .expect("the provider error is the head of the chain");
        assert_eq!(status.status, Some(429));
        assert!(status.is_transient());
        assert!(
            status.message.contains("rate limited"),
            "{}",
            status.message
        );
        assert_eq!(
            err.downcast_ref::<crate::llm::RetryAfter>()
                .map(|hint| hint.0),
            Some(std::time::Duration::from_secs(60)),
            "the server's own deadline reaches the retry loop"
        );
    }

    #[tokio::test]
    async fn a_failure_without_the_header_leaves_the_ladder_in_charge() {
        let provider = scripted_provider(
            "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: \
             5\r\nConnection: close\r\n\r\nbusy!",
        )
        .await;
        let err = failed_chat(&provider).await;
        assert_eq!(
            err.downcast_ref::<ProviderError>()
                .expect("typed")
                .status
                .unwrap(),
            503
        );
        assert!(
            err.downcast_ref::<crate::llm::RetryAfter>().is_none(),
            "nothing is invented when the server said nothing"
        );
    }

    #[test]
    fn a_local_endpoint_is_not_given_the_cloud_stall_detector() {
        // LM Studio / vLLM / text-generation-webui on loopback, configured as
        // an `openai` provider: a long prefill on weak hardware is silent for
        // minutes and must not be killed at five.
        let local = OpenAiProvider::new("http://127.0.0.1:1234/v1", "m", "");
        assert_eq!(local.read_timeout(), None);
        // Adding headers rebuilds the inner client; the policy must survive.
        let local = local.with_headers(&[("HTTP-Referer", "https://wizard.local")]);
        assert_eq!(local.read_timeout(), None);

        // A hosted endpoint keeps it: there, silence is a dead connection.
        let cloud = OpenAiProvider::new("https://openrouter.ai/api/v1", "m", "k");
        assert!(cloud.read_timeout().is_some());
        assert!(
            cloud
                .with_headers(&[("HTTP-Referer", "https://wizard.dev")])
                .read_timeout()
                .is_some()
        );
    }

    #[test]
    fn context_window_table_covers_openai_xai_and_unknowns() {
        assert_eq!(context_window("gpt-4o"), Some(128_000));
        assert_eq!(context_window("gpt-4o-mini"), Some(128_000));
        assert_eq!(context_window("gpt-4.1"), Some(1_047_576));
        assert_eq!(context_window("gpt-5"), Some(400_000));
        assert_eq!(context_window("gpt-5.6-sol"), Some(1_000_000));
        assert_eq!(context_window("o3-mini"), Some(200_000));
        assert_eq!(context_window("grok-3"), Some(131_072));
        assert_eq!(context_window("grok-4.3"), Some(1_000_000));
        assert_eq!(context_window("grok-4.20-0309-reasoning"), Some(1_000_000));
        assert_eq!(context_window("grok-4.6"), Some(500_000));
        assert_eq!(context_window("grok-4.5"), Some(500_000));
        assert_eq!(context_window("grok-build-0.1"), Some(256_000));
        assert_eq!(context_window("gemini-3.5-flash"), Some(1_048_576));
        assert_eq!(context_window("deepseek-v4-pro"), Some(1_000_000));
        assert_eq!(context_window("kimi-k3"), Some(1_000_000));
        assert_eq!(context_window("minimax-m2.7"), Some(204_800));
        assert_eq!(context_window("qwen3-8b"), None, "local tags stay unknown");
        assert_eq!(context_window(""), None);
    }
}
