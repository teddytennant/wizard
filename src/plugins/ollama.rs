//! Ollama's native `/api/chat` endpoint, as a plugin, behind `--features
//! provider-ollama`.
//!
//! The readiness hook is the half worth knowing about: `prepare` pulls a
//! missing model before the first health probe and reports bytes through
//! [`crate::progress::ByteProgress`], after asking
//! [`crate::platform::host::local_port`] whether the configured `base_url` is
//! this machine at all — Wizard does not download a multi-gigabyte model onto
//! somebody else's disk.
//!
//! Both of those used to live in `src/server.rs`, and this file is why they do
//! not any more. When the llama-server lifecycle became part of the llama.cpp
//! plugin, leaving them with it would have made "Ollama can report a model
//! pull" depend on whether a *different* backend was compiled in. They went to
//! core instead, beside their other callers, and what is left here is a plugin
//! reaching into core — the direction the boundary allows, and the same kind of
//! edge as reaching for the shared HTTP client builder.
//!
//! Thin `reqwest` wrapper — no `ollama-rs` dependency, keeping the binary
//! small. Provides a startup health probe, a native-tool-support probe, and
//! NDJSON streaming chat.
//!
//! This is the OpenAI-compatible family's odd one out: the native endpoint is
//! not block-structured and it has no tool-call ids, so
//! [`build_request_body`] flattens Wizard's blocks into Ollama's own shape and
//! [`parse_chunk_line`] mints the ids the rest of the tree correlates by.
//! Prompt caching has no counterpart either, and it degrades to nothing rather
//! than to a field on the wire: the server keeps the loaded model's KV cache
//! between requests and reuses the matching prefix on its own, so there is no
//! `prompt_cache_key` to send. Nothing is lost by leaving it off; sending it
//! would only put a key on the wire that this API never defined.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use futures_util::{Stream, StreamExt, stream};
use serde::Deserialize;

use crate::kernel::{Capability, Ctx, Plugin, PluginManifest};
use crate::llm::provider::LlmProvider;
use crate::llm::registry::{Credentials, ProviderDescriptor, ProviderKind};
use crate::llm::{ChatChunk, ChatOptions, ChatRequest, ChatStream, ProviderError};
use crate::progress::{ByteProgress, Progress};

/// Overall timeout for small control requests (`/api/tags`, `/api/show`).
/// Chat requests are exempt — generation can legitimately take minutes.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
/// Request context length when the `/api/show` probe fails. Well above
/// Ollama's silent 4096 default, which truncates agent history server-side.
const DEFAULT_NUM_CTX: u32 = 16_384;
/// Cap on the probe-derived `num_ctx`: a 128k+ token KV cache can exhaust
/// RAM on the machines Ollama typically runs on. An explicit `num_ctx` in
/// the request is passed through untouched.
const MAX_DERIVED_NUM_CTX: u32 = 32_768;

/// Errors specific to talking to Ollama, surfaced so the TUI can render
/// actionable messages (e.g. "is Ollama running?").
#[derive(Debug, thiserror::Error)]
pub enum OllamaError {
    #[error(
        "cannot reach Ollama at {host} — is the server running? Start it with `ollama serve` (or check `ollama_host` in ~/.wizard/config.toml). Cause: {source}"
    )]
    Unreachable {
        host: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("model '{0}' is not installed (try `ollama pull {0}`)")]
    ModelMissing(String),
    #[error("Ollama returned HTTP {status}: {body}")]
    Api {
        status: reqwest::StatusCode,
        body: String,
    },
}

impl OllamaError {
    /// Whether this error is transient — a retry after backoff may succeed.
    /// Connection/timeout failures and server-busy/rate-limit/5xx statuses
    /// are transient; a missing model or a 4xx (other than 408 and 429) is not.
    ///
    /// The status arm is deliberately [`ProviderError::is_transient`] rather
    /// than a second opinion about the same numbers. Ollama is rarely reached
    /// directly in a setup that has a status to classify at all: what answers
    /// is a reverse proxy, a Tailscale front end or a hosted
    /// Ollama-compatible endpoint, and those are the things that emit the 408
    /// this used to call permanent — a gateway timing out a long generation,
    /// which is the single most retryable failure there is. Two predicates
    /// over one set of statuses is how they came to disagree; there is now
    /// one, and [`typed`] already guarantees both error types are reachable
    /// from the same `anyhow` chain.
    pub fn is_transient(&self) -> bool {
        match self {
            OllamaError::Unreachable { .. } => true,
            OllamaError::ModelMissing(_) => false,
            OllamaError::Api { status, .. } => ProviderError {
                status: Some(status.as_u16()),
                message: String::new(),
            }
            .is_transient(),
        }
    }
}

/// Wrap an [`OllamaError`] so callers can classify it either way:
/// `downcast_ref::<OllamaError>()` (legacy agent retry path) or
/// `downcast_ref::<ProviderError>()` (shared provider retry contract).
/// The two classifications agree: `ModelMissing` maps to its originating
/// 404 and `Unreachable` to a transport failure.
fn typed(err: OllamaError) -> anyhow::Error {
    typed_with_retry_after(err, None)
}

/// [`typed`], plus the `Retry-After` the response carried.
///
/// The hint rides *under* both typed errors on the same `anyhow` chain
/// instead of becoming a field on either, exactly as
/// [`crate::llm::http_error_with_retry_after`] arranges it for the
/// HTTP-status providers: the head of the chain (and so the message the user
/// sees) is unchanged, and all three of `OllamaError`, `ProviderError` and
/// `RetryAfter` stay reachable by `downcast_ref`. Ollama itself does not send
/// the header, but a reverse proxy or a hosted Ollama-compatible endpoint in
/// front of it does, and a 429 that names its own deadline beats our ladder
/// guessing at one.
fn typed_with_retry_after(err: OllamaError, retry_after: Option<Duration>) -> anyhow::Error {
    let status = match &err {
        OllamaError::Unreachable { .. } => None,
        OllamaError::ModelMissing(_) => Some(404),
        OllamaError::Api { status, .. } => Some(status.as_u16()),
    };
    let provider = ProviderError {
        status,
        message: err.to_string(),
    };
    match retry_after {
        Some(delay) => anyhow::Error::new(crate::llm::RetryAfter(delay))
            .context(err)
            .context(provider),
        None => anyhow::Error::new(err).context(provider),
    }
}

/// Client bound to one Ollama host. Cheap to clone.
#[derive(Debug, Clone)]
pub struct OllamaClient {
    http: reqwest::Client,
    host: String,
    /// Per-model derived `num_ctx` (see [`OllamaClient::derived_num_ctx`]);
    /// failed probes cache the fallback so they are not retried per request.
    num_ctx_cache: Arc<Mutex<HashMap<String, u32>>>,
}

impl OllamaClient {
    /// Create a client for `host` (e.g. `http://127.0.0.1:11434`). Trailing
    /// slashes are trimmed.
    pub fn new(host: impl Into<String>) -> Self {
        let host = host.into().trim_end_matches('/').to_string();
        // The same builder every other chat backend uses, and for the reason
        // this one used to be the exception for: Ollama is "the local one", so
        // it was given a hand-rolled client with a connect timeout and nothing
        // else. But `WIZARD_OLLAMA_HOST` points at another machine as often as
        // not — a GPU box on the LAN, a Tailscale address, an SSH tunnel — and
        // against those a connection that is accepted and then goes silent had
        // no read timeout at all. `pull_model` and the status probes wait on
        // that stream, so a half-open NAT binding or a suspended host hung them
        // with nothing to classify and no retry to make.
        //
        // `client_read_timeout_for` is what keeps the local case unchanged: a
        // loopback or LAN host still gets `None`, because a local model that is
        // simply thinking slowly must not be killed for it.
        let http = crate::llm::chat_http_builder(crate::llm::client_read_timeout_for(&host))
            .build()
            // Builder construction only fails when the TLS backend cannot
            // initialize; fall back to the default client rather than panic.
            .unwrap_or_default();
        Self {
            http,
            host,
            num_ctx_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Base URL this client talks to.
    pub fn host(&self) -> &str {
        &self.host
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.host, path)
    }

    /// Map a transport-level `reqwest` failure into an actionable error.
    /// Connection refusals and timeouts become [`OllamaError::Unreachable`],
    /// which tells the user to run `ollama serve`.
    fn transport_error(&self, source: reqwest::Error) -> anyhow::Error {
        if source.is_connect() || source.is_timeout() {
            typed(OllamaError::Unreachable {
                host: self.host.clone(),
                source,
            })
        } else {
            let message = format!("HTTP request to {} failed: {source}", self.host);
            anyhow::Error::new(source).context(ProviderError::transport(message))
        }
    }

    /// Read the body of a non-success response and convert it into
    /// [`OllamaError::ModelMissing`] (404 mentioning the model) or
    /// [`OllamaError::Api`].
    async fn status_error(
        &self,
        response: reqwest::Response,
        model: Option<&str>,
    ) -> anyhow::Error {
        let status = response.status();
        // Before `text()`, which consumes the response along with its headers.
        let retry_after = crate::llm::retry_after_from_headers(response.headers());
        let body = response.text().await.unwrap_or_default();
        if let Some(model) = model
            && status == reqwest::StatusCode::NOT_FOUND
            && body.contains("not found")
        {
            return typed(OllamaError::ModelMissing(model.to_string()));
        }
        typed_with_retry_after(OllamaError::Api { status, body }, retry_after)
    }

    /// Startup health probe: `GET /api/tags`. Errors with
    /// [`OllamaError::Unreachable`] when the server is down.
    pub async fn health(&self) -> Result<()> {
        let response = self
            .http
            .get(self.url("/api/tags"))
            .timeout(PROBE_TIMEOUT)
            .send()
            .await
            .map_err(|e| self.transport_error(e))?;
        if !response.status().is_success() {
            return Err(self.status_error(response, None).await);
        }
        Ok(())
    }

    /// List locally installed model tags (`GET /api/tags`).
    pub async fn list_models(&self) -> Result<Vec<String>> {
        let response = self
            .http
            .get(self.url("/api/tags"))
            .timeout(PROBE_TIMEOUT)
            .send()
            .await
            .map_err(|e| self.transport_error(e))?;
        if !response.status().is_success() {
            return Err(self.status_error(response, None).await);
        }
        let tags: TagsResponse = response
            .json()
            .await
            .context("failed to parse /api/tags response")?;
        Ok(tags.models.into_iter().map(|m| m.name).collect())
    }

    /// Probe whether `model` supports native tool calling
    /// (`POST /api/show`, inspect `capabilities` for `"tools"`). When this
    /// returns `false` the agent loop falls back to a prompt-based JSON tool
    /// protocol (see `docs/byom.md`).
    pub async fn supports_native_tools(&self, model: &str) -> Result<bool> {
        let response = self
            .http
            .post(self.url("/api/show"))
            .timeout(PROBE_TIMEOUT)
            .json(&serde_json::json!({ "model": model }))
            .send()
            .await
            .map_err(|e| self.transport_error(e))?;
        if !response.status().is_success() {
            return Err(self.status_error(response, Some(model)).await);
        }
        let info: ShowResponse = response
            .json()
            .await
            .context("failed to parse /api/show response")?;
        let supported = info
            .capabilities
            .as_deref()
            .is_some_and(|caps| caps.iter().any(|c| c == "tools"));
        if !supported {
            tracing::debug!(
                model,
                capabilities = ?info.capabilities,
                "model does not advertise native tool support; \
                 the agent loop will use the JSON tool protocol"
            );
        }
        Ok(supported)
    }

    /// Effective `num_ctx` for `model` when the request does not set one:
    /// the model's trained context length from `/api/show` (capped at
    /// [`MAX_DERIVED_NUM_CTX`]), or [`DEFAULT_NUM_CTX`] when the probe
    /// fails. Probed once per model per client.
    async fn derived_num_ctx(&self, model: &str) -> u32 {
        if let Some(&cached) = self.num_ctx_cache.lock().unwrap().get(model) {
            return cached;
        }
        let derived = self
            .model_context_length(model)
            .await
            .map(|n| n.min(MAX_DERIVED_NUM_CTX))
            .unwrap_or(DEFAULT_NUM_CTX);
        self.num_ctx_cache
            .lock()
            .unwrap()
            .insert(model.to_string(), derived);
        derived
    }

    /// `POST /api/show` → the `"<arch>.context_length"` entry of
    /// `model_info`. Any failure yields `None`.
    async fn model_context_length(&self, model: &str) -> Option<u32> {
        let response = self
            .http
            .post(self.url("/api/show"))
            .timeout(PROBE_TIMEOUT)
            .json(&serde_json::json!({ "model": model }))
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let info: ShowResponse = response.json().await.ok()?;
        context_length_from_model_info(info.model_info.as_ref()?)
    }

    /// Make sure `model` is available locally: a no-op when `/api/tags`
    /// already lists it (a bare `name` counts as `name:latest`), otherwise
    /// pull it with [`OllamaClient::pull_model`]. The setup paths call this
    /// so a freshly onboarded tag materializes on first run, mirroring how a
    /// missing GGUF is downloaded for llama.cpp.
    pub async fn ensure_model(&self, model: &str, progress: &dyn Progress) -> Result<()> {
        let installed = self.list_models().await?;
        if model_installed(model, &installed) {
            return Ok(());
        }
        self.pull_model(model, progress).await
    }

    /// Pull `model` through Ollama's native streaming API (`POST /api/pull`,
    /// NDJSON progress lines), rendering layer downloads as byte-counted
    /// bars. Fails on transport errors, non-success statuses, and in-band
    /// `{"error": ...}` lines (e.g. an unknown tag). Interrupted pulls
    /// resume server-side on the next attempt.
    pub async fn pull_model(&self, model: &str, progress: &dyn Progress) -> Result<()> {
        progress.status(&format!(
            "model '{model}' is not pulled yet — pulling it now (one-time)…"
        ));
        let response = self
            .http
            .post(self.url("/api/pull"))
            .json(&serde_json::json!({ "model": model }))
            .send()
            .await
            .map_err(|e| self.transport_error(e))?;
        if !response.status().is_success() {
            return Err(self.status_error(response, Some(model)).await);
        }

        let mut render = PullRender::new(progress, model);
        let mut done = false;
        let mut buf: Vec<u8> = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| {
                anyhow!(e).context(format!(
                    "the pull of '{model}' was interrupted — re-run to resume"
                ))
            })?;
            buf.extend_from_slice(&chunk);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                done |= apply_pull_line(&mut render, model, &String::from_utf8_lossy(&line))?;
            }
        }
        // Flush a trailing line without a newline at EOF.
        done |= apply_pull_line(&mut render, model, &String::from_utf8_lossy(&buf))?;
        render.close();

        // No explicit success line: trust the server's model list over the
        // transcript before declaring failure.
        if !done && !model_installed(model, &self.list_models().await.unwrap_or_default()) {
            bail!("the pull of '{model}' ended without success — re-run to resume");
        }
        progress.status(&format!("pulled {model}"));
        Ok(())
    }

    /// Start a streaming chat completion (`POST /api/chat`, NDJSON).
    /// Yields [`ChatChunk`]s until one with `done == true`; the caller
    /// accumulates `message.content` deltas and collects `tool_calls`.
    pub async fn chat_stream(&self, mut request: ChatRequest) -> Result<ChatStream> {
        let model = request.model.clone();
        // Ollama defaults num_ctx to 4096 and silently truncates the prompt
        // server-side, so always send an explicit value.
        let options = request.options.get_or_insert_with(ChatOptions::default);
        if options.num_ctx.is_none() {
            options.num_ctx = Some(self.derived_num_ctx(&model).await);
        }
        let body = build_request_body(&request)?;
        let response = self
            .http
            .post(self.url("/api/chat"))
            .json(&body)
            .send()
            .await
            .map_err(|e| self.transport_error(e))?;
        if !response.status().is_success() {
            return Err(self.status_error(response, Some(&model)).await);
        }
        let bytes = response
            .bytes_stream()
            .map(|item| match item {
                Ok(chunk) => Ok(chunk.to_vec()),
                Err(e) => Err(anyhow!(e).context(ProviderError::transport(
                    "Ollama response stream was interrupted",
                ))),
            })
            .boxed();
        Ok(decode_ndjson(bytes))
    }
}

#[async_trait]
impl LlmProvider for OllamaClient {
    async fn health(&self) -> Result<()> {
        OllamaClient::health(self).await
    }

    async fn supports_native_tools(&self, model: &str) -> Result<bool> {
        OllamaClient::supports_native_tools(self, model).await
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        OllamaClient::list_models(self).await
    }

    async fn chat_stream(&self, request: ChatRequest) -> Result<ChatStream> {
        OllamaClient::chat_stream(self, request).await
    }

    /// The server truncates the prompt at `num_ctx`, so the effective window
    /// is the value chat requests will carry (probe-derived and capped),
    /// not the model's full trained context length.
    async fn context_window(&self, model: &str) -> Option<u32> {
        Some(self.derived_num_ctx(model).await)
    }

    fn label(&self) -> String {
        self.host().to_string()
    }
}

/// Translate a native [`ChatRequest`] into Ollama's `/api/chat` body.
///
/// The top-level shape is the request's own serde shape; the messages are
/// rebuilt, because Ollama's native endpoint is the one backend that is *not*
/// block-structured. It wants a flat string `content`, a sibling `tool_calls`
/// array, and a sibling array of bare base64 image strings (it sniffs the
/// media type itself), where Wizard carries [`ContentBlock`]s.
///
/// Images on an *assistant* message, ones the model generated, are named in
/// its text instead: an assistant turn is not image input, and a vision model
/// handed its own output back as input would only be confused by it.
///
/// Tool-call ids have nowhere to go here. Ollama correlates results by
/// position, so a `tool`-role message's blocks are emitted as one message per
/// result, in order, which is what the model that made the calls expects.
fn build_request_body(request: &ChatRequest) -> Result<serde_json::Value> {
    use crate::llm::Role;
    use serde_json::{Value, json};

    let mut body = serde_json::to_value(request).context("serializing chat request")?;
    let mut messages: Vec<Value> = Vec::with_capacity(request.messages.len());
    for message in &request.messages {
        let role = match message.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        if message.role == Role::Tool {
            for result in message.tool_results() {
                messages.push(json!({
                    "role": role,
                    "content": result.content,
                    "tool_name": result.name,
                }));
            }
            continue;
        }
        let content = if message.role == Role::Assistant {
            crate::llm::assistant_content(message)
        } else {
            message.text()
        };
        let mut wire = json!({ "role": role, "content": content });
        let tool_calls = message.tool_calls();
        if !tool_calls.is_empty() {
            wire["tool_calls"] = Value::Array(
                tool_calls
                    .iter()
                    .map(|call| {
                        json!({ "function": {
                            "name": call.function.name,
                            "arguments": call.function.arguments,
                        }})
                    })
                    .collect(),
            );
        }
        // Only *input* images reach the wire; an assistant's own are already
        // named in `content` above.
        let images = message.images();
        if !images.is_empty() && message.role != Role::Assistant {
            wire["images"] = Value::Array(
                images
                    .iter()
                    .map(|image| Value::String(image.b64.clone()))
                    .collect(),
            );
        }
        messages.push(wire);
    }
    body["messages"] = Value::Array(messages);
    Ok(body)
}

/// `GET /api/tags` response body (subset we care about).
#[derive(Debug, Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<ModelTag>,
}

#[derive(Debug, Deserialize)]
struct ModelTag {
    name: String,
}

/// Whether `tag` is present in `installed` (the `/api/tags` names). A tag
/// without an explicit version means `:latest` — matching how Ollama
/// resolves bare names — so `"llama3"` matches an installed
/// `"llama3:latest"` and vice versa, while `"qwen3.5"` does *not* match
/// `"qwen3.5:9b"`.
pub fn model_installed(tag: &str, installed: &[String]) -> bool {
    fn canonical(tag: &str) -> std::borrow::Cow<'_, str> {
        // The version separator is the colon after the last `/`, so a
        // registry port (`host:port/name`) is not mistaken for a version.
        let name = tag.rsplit('/').next().unwrap_or(tag);
        if name.contains(':') {
            std::borrow::Cow::Borrowed(tag)
        } else {
            std::borrow::Cow::Owned(format!("{tag}:latest"))
        }
    }
    let want = canonical(tag);
    installed.iter().any(|have| canonical(have) == want)
}

/// One NDJSON progress line from `POST /api/pull`. Layer downloads carry
/// `digest`/`total`/`completed`; milestones ("pulling manifest", "verifying
/// sha256 digest", "success") carry only `status`; failures carry `error`.
#[derive(Debug, Default, PartialEq, Deserialize)]
struct PullLine {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    digest: Option<String>,
    #[serde(default)]
    total: Option<u64>,
    #[serde(default)]
    completed: Option<u64>,
    #[serde(default)]
    error: Option<String>,
}

/// Parse one `/api/pull` NDJSON line.
fn parse_pull_line(line: &str) -> Result<PullLine> {
    serde_json::from_str(line).with_context(|| {
        let preview: String = line.chars().take(200).collect();
        format!("unparseable line from Ollama pull: {preview}")
    })
}

/// Feed one raw pull line into `render`. Returns whether it was the final
/// `"success"` line; blank lines are skipped; in-band errors bail.
fn apply_pull_line(render: &mut PullRender, model: &str, raw: &str) -> Result<bool> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(false);
    }
    let line = parse_pull_line(raw)?;
    if let Some(error) = line.error {
        bail!("Ollama could not pull '{model}': {error}");
    }
    let success = line.status.as_deref() == Some("success");
    if !success {
        render.apply(&line);
    }
    Ok(success)
}

/// Renders pull progress onto a [`Progress`] sink: one byte-counted bar per
/// layer digest (Ollama reports each blob separately), plain status lines
/// for the milestones in between.
struct PullRender<'a> {
    sink: &'a dyn Progress,
    model: &'a str,
    bar: Option<PullBar>,
    last_status: String,
}

/// The open byte bar for one layer digest.
struct PullBar {
    digest: String,
    guard: Box<dyn ByteProgress>,
    completed: u64,
}

impl<'a> PullRender<'a> {
    fn new(sink: &'a dyn Progress, model: &'a str) -> Self {
        Self {
            sink,
            model,
            bar: None,
            last_status: String::new(),
        }
    }

    /// Advance the display for one progress line.
    fn apply(&mut self, line: &PullLine) {
        match (line.digest.as_deref(), line.total) {
            (Some(digest), Some(total)) => {
                if self.bar.as_ref().is_none_or(|bar| bar.digest != digest) {
                    self.close();
                    let label = format!("pulling {} ({})", self.model, short_digest(digest));
                    self.bar = Some(PullBar {
                        digest: digest.to_string(),
                        guard: self.sink.bytes(&label, Some(total)),
                        completed: 0,
                    });
                }
                let bar = self.bar.as_mut().expect("bar was just ensured");
                let completed = line.completed.unwrap_or(0).min(total);
                if completed > bar.completed {
                    bar.guard.inc(completed - bar.completed);
                    bar.completed = completed;
                }
            }
            // A layer line before its size is known — wait for totals.
            (Some(_), None) => {}
            (None, _) => {
                if let Some(status) = line.status.as_deref()
                    && status != self.last_status
                {
                    self.close();
                    self.last_status = status.to_string();
                    self.sink.status(status);
                }
            }
        }
    }

    /// Finish the open byte bar, if any.
    fn close(&mut self) {
        if let Some(bar) = self.bar.take() {
            bar.guard.finish("");
        }
    }
}

/// Compact display form of a layer digest: `sha256:ab12cd34…`.
///
/// Cut by characters, not bytes. The digest is whatever the server put in the
/// pull stream — hex in practice, but nothing here verifies that — and
/// `&hex[..8]` panics outright when byte 8 lands mid-codepoint. A remote
/// Ollama, or anything answering on its port, could bring the process down
/// with a nine-character digest containing one accented letter.
fn short_digest(digest: &str) -> String {
    match digest.split_once(':') {
        Some((algo, hex)) if hex.chars().count() > 8 => {
            let head: String = hex.chars().take(8).collect();
            format!("{algo}:{head}…")
        }
        _ => digest.to_string(),
    }
}

/// `POST /api/show` response body (subset we care about). Older Ollama
/// versions omit `capabilities` entirely; we treat that as "no native
/// tools" and use the JSON protocol fallback.
#[derive(Debug, Deserialize)]
struct ShowResponse {
    #[serde(default)]
    capabilities: Option<Vec<String>>,
    #[serde(default)]
    model_info: Option<serde_json::Value>,
}

/// Find the `"<architecture>.context_length"` entry in `/api/show`'s
/// `model_info` map (the key is prefixed by the model architecture,
/// e.g. `"llama.context_length"` or `"qwen3.context_length"`).
fn context_length_from_model_info(model_info: &serde_json::Value) -> Option<u32> {
    model_info.as_object()?.iter().find_map(|(key, value)| {
        if !key.ends_with(".context_length") {
            return None;
        }
        value.as_u64().and_then(|n| u32::try_from(n).ok())
    })
}

/// In-band error line Ollama can emit mid-stream: `{"error": "..."}`.
#[derive(Debug, Deserialize)]
struct ErrorLine {
    error: String,
}

/// Type an in-band `{"error": ...}` line so the retry ladder can classify it.
///
/// This used to be a bare `bail!`, which put an untyped `anyhow` on the wire.
/// [`crate::agent::error_is_transient`] *defaults* an error it does not
/// recognize to transient — the right default, since ending a run over an
/// unfamiliar error is worse than retrying one — so every mid-stream Ollama
/// error was retried regardless of what it said. That was survivable while the
/// breaker ended a continuous run outright; now that such a run waits an open
/// breaker out, a permanent one would be waited on for hours before the
/// patience ceiling stopped it. A missing model is never going to appear by
/// itself, so it is named as the permanent condition it is.
///
/// Anything else stays retryable, but *typed* — a deliberate classification
/// rather than a fall-through, so the ladder's bound is the one meant for
/// outages rather than the one meant for the unknown.
fn classify_stream_error(message: &str) -> anyhow::Error {
    let lower = message.to_ascii_lowercase();
    let missing_model =
        (lower.contains("not found") || lower.contains("no such model")) && lower.contains("model");
    if missing_model {
        return typed(OllamaError::ModelMissing(message.to_string()));
    }
    anyhow::Error::new(ProviderError::transport(format!("Ollama error: {message}")))
}

/// Parse one NDJSON line into a [`ChatChunk`], surfacing Ollama's in-band
/// `{"error": ...}` lines as errors.
fn parse_chunk_line(line: &str) -> Result<ChatChunk> {
    match serde_json::from_str::<ChatChunk>(line) {
        Ok(mut chunk) => {
            // Ollama's native `tool_calls` carry no ids at all: it pairs
            // results by position. Everything downstream correlates by id, so
            // one is minted here, at the seam, rather than special-cased in
            // the agent loop.
            if let Some(message) = chunk.message.as_mut() {
                let mut calls = message.take_tool_calls();
                crate::llm::ensure_tool_call_ids(&mut calls);
                for call in calls {
                    message.push_tool_call(call);
                }
            }
            Ok(chunk)
        }
        Err(parse_err) => {
            if let Ok(err) = serde_json::from_str::<ErrorLine>(line) {
                return Err(classify_stream_error(&err.error));
            }
            let preview: String = line.chars().take(200).collect();
            Err(anyhow!(parse_err).context(format!("unparseable line from Ollama: {preview}")))
        }
    }
}

/// Decoder state for [`decode_ndjson`].
struct NdjsonState<S> {
    bytes: S,
    buf: Vec<u8>,
    finished: bool,
    /// The bytes ran out before any line said `done: true`. Raised on the
    /// *next* poll rather than immediately, so a trailing line the peer never
    /// newline-terminated is still delivered first.
    ///
    /// This is the state [`NdjsonState::finished`] used to swallow. Both a
    /// completed generation and a connection cut mid-generation reach EOF; the
    /// second used to end the stream with `Ok(None)`, handing the agent every
    /// token that had arrived and no `done` chunk at all, which is
    /// indistinguishable from a turn that finished normally.
    cut: bool,
}

/// Turn a raw byte stream into a [`ChatStream`] by splitting on newlines and
/// parsing each line as a [`ChatChunk`]. The stream ends after the chunk
/// with `done == true` (or on transport EOF / error).
fn decode_ndjson<S>(bytes: S) -> ChatStream
where
    S: Stream<Item = Result<Vec<u8>>> + Send + Unpin + 'static,
{
    let state = NdjsonState {
        bytes,
        buf: Vec::new(),
        finished: false,
        cut: false,
    };
    stream::try_unfold(state, |mut state| async move {
        if state.finished {
            return Ok(None);
        }
        if state.cut {
            return Err(crate::llm::stream_ended_early("the Ollama stream"));
        }
        loop {
            // Drain any complete lines already buffered.
            while let Some(pos) = state.buf.iter().position(|&b| b == b'\n') {
                let line_bytes: Vec<u8> = state.buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line_bytes);
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let chunk = parse_chunk_line(line)?;
                if chunk.done {
                    state.finished = true;
                }
                return Ok(Some((chunk, state)));
            }
            match state.bytes.next().await {
                Some(Ok(data)) => state.buf.extend_from_slice(&data),
                Some(Err(e)) => return Err(e),
                None => {
                    // EOF: flush a trailing line without a newline (also the
                    // whole body when the caller requested `stream: false`).
                    // Whatever that line says decides which of the two endings
                    // this was; an empty buffer means the peer stopped talking
                    // without ever saying the reply was over.
                    let rest = String::from_utf8_lossy(&state.buf).trim().to_string();
                    state.buf.clear();
                    if rest.is_empty() {
                        return Err(crate::llm::stream_ended_early("the Ollama stream"));
                    }
                    let chunk = parse_chunk_line(&rest)?;
                    if chunk.done {
                        state.finished = true;
                    } else {
                        state.cut = true;
                    }
                    return Ok(Some((chunk, state)));
                }
            }
        }
    })
    .boxed()
}

/// How `kind = "ollama"` is registered.
///
/// [`Credentials::Local`] but *not* [`ProviderDescriptor::with_local_server`]:
/// Ollama runs on this machine, so its tokens are free, but Wizard neither
/// spawns nor stops it and `/server` has to keep saying so.
pub fn descriptor() -> ProviderDescriptor {
    ProviderDescriptor::new(
        ProviderKind::OLLAMA,
        "Ollama",
        Credentials::Local,
        |config| Ok(Arc::new(OllamaClient::new(config.base_url.clone()))),
    )
    .with_prepare(|config, model| async move {
        // Loopback hosts only. The analog of llama.cpp's spawn is pulling a
        // configured tag that is not on the server yet (onboarding's BYOM
        // pick, a hand-written config) — but Wizard never downloads models
        // onto somebody else's machine, so a remote Ollama is left alone.
        if crate::platform::host::local_port(&config.base_url).is_none() {
            return Ok(());
        }
        let wait =
            crate::progress::ServerSpinner::start_with("Checking the local model…", "model ready");
        let outcome = OllamaClient::new(config.base_url.clone())
            .ensure_model(&model, &wait)
            .await;
        wait.finish(outcome.is_ok());
        outcome
    })
}

/// Ollama as a kernel plugin.
///
/// The readiness hook is the interesting half: `prepare` pulls a missing
/// model before the first health probe, reporting bytes through
/// [`crate::progress::ByteProgress`] and only when
/// [`crate::platform::host::local_port`] says the server is on this machine.
/// Both are core, and this plugin is the reason they are — see the module
/// docs.
///
/// `network` is declared because that is what this plugin does, even though
/// the capability set only gates the Lua host bridge today. A manifest that
/// under-declares is the failure mode worth avoiding: the grant prompt is
/// generated from it.
pub struct OllamaPlugin {
    manifest: PluginManifest,
}

impl OllamaPlugin {
    pub fn new() -> Self {
        Self {
            manifest: PluginManifest {
                name: "ollama".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                description: "Local Ollama, native /api/chat".to_string(),
                capabilities: vec![Capability::Network],
                optional_deps: Vec::new(),
                profiles: vec![
                    "pi".to_string(),
                    "server".to_string(),
                    "default".to_string(),
                    "full".to_string(),
                ],
            },
        }
    }
}

impl Default for OllamaPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for OllamaPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn apply(&self, ctx: &mut Ctx) -> anyhow::Result<()> {
        ctx.provider(descriptor())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::Role;

    #[test]
    fn host_trailing_slash_is_trimmed() {
        let client = OllamaClient::new("http://127.0.0.1:11434///");
        assert_eq!(client.host(), "http://127.0.0.1:11434");
        assert_eq!(client.url("/api/tags"), "http://127.0.0.1:11434/api/tags");
    }

    /// A digest from the pull stream cannot panic the process.
    ///
    /// The cut was `&hex[..8]`, on a string this program never validates: it
    /// is whatever the server put in its progress JSON. Byte 8 landing inside
    /// a multi-byte codepoint is an immediate panic, so any host answering on
    /// the Ollama port could take Wizard down mid-pull with one accented
    /// character.
    #[test]
    fn a_digest_is_shortened_by_characters_not_bytes() {
        assert_eq!(short_digest("sha256:ab12cd34ef"), "sha256:ab12cd34…");
        // Exactly eight characters, and fewer: nothing to cut.
        assert_eq!(short_digest("sha256:ab12cd34"), "sha256:ab12cd34");
        assert_eq!(short_digest("sha256:ab"), "sha256:ab");
        assert_eq!(short_digest("no-colon"), "no-colon");
        // Nine two-byte characters: byte 8 lands mid-codepoint, which is
        // where the old cut panicked.
        assert_eq!(short_digest("sha256:ééééééééé"), "sha256:éééééééé…");
        // Three four-byte characters: under the limit by count, over it by
        // bytes, so the old cut panicked here too.
        assert_eq!(short_digest("sha256:🧙🧙🧙"), "sha256:🧙🧙🧙");
    }

    /// Ollama's client is built by the shared chat builder, so it carries the
    /// same read timeout policy as every other backend.
    ///
    /// It was the one hand-rolled client in the tree: a connect timeout and
    /// nothing else. `WIZARD_OLLAMA_HOST` regularly points at another machine,
    /// and against one of those a connection that is accepted and then goes
    /// quiet hung `pull_model` and the status probes with no timeout to end it.
    /// Grep, because a read timeout is not readable back off a `reqwest::Client`.
    #[test]
    fn the_ollama_client_uses_the_shared_chat_builder() {
        let source = include_str!("ollama.rs");
        let (production, _) = source
            .split_once("#[cfg(test)]\nmod tests {")
            .expect("this module ends with its test module");
        assert!(
            production
                .contains("crate::llm::chat_http_builder(crate::llm::client_read_timeout_for("),
            "the client must come from the shared builder with the locality-aware read timeout"
        );
        assert!(
            !production.contains("reqwest::Client::builder()"),
            "no second, timeout-less client policy in this module"
        );
    }

    fn request(messages: Vec<crate::llm::ChatMessage>) -> ChatRequest {
        ChatRequest {
            model: "qwen3-vl".to_string(),
            messages,
            tools: Vec::new(),
            stream: true,
            options: None,
        }
    }

    /// Ollama's native `/api/chat` is the one backend that is not
    /// block-structured: it wants a flat string `content` and a sibling
    /// `tool_calls` array, so the block list has to be flattened rather than
    /// serialized straight through.
    ///
    /// It also has no place for a tool-call id (it pairs results by
    /// position), so a batch's one `tool`-role message expands to one wire
    /// message per result, in call order.
    #[test]
    fn blocks_flatten_to_the_native_string_and_sibling_shape() {
        let mut assistant = crate::llm::ChatMessage::assistant("running both");
        assistant.push_tool_call(crate::llm::ToolCall::new(
            "read_file",
            serde_json::json!({ "path": "a" }),
        ));
        assistant.push_tool_call(crate::llm::ToolCall::new(
            "read_file",
            serde_json::json!({ "path": "b" }),
        ));
        let ids: Vec<String> = assistant
            .tool_calls()
            .iter()
            .map(|call| call.id.clone())
            .collect();
        let mut results = crate::llm::ChatMessage::tool_result(&ids[0], "read_file", "a body");
        results.push_tool_result(&ids[1], "read_file", "b body");

        let body = build_request_body(&request(vec![
            crate::llm::ChatMessage::user("read both"),
            assistant,
            results,
        ]))
        .expect("body");
        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 4, "one wire message per result");
        assert_eq!(messages[0]["content"], "read both");
        assert_eq!(
            messages[1]["content"], "running both",
            "content is a plain string, not a block array"
        );
        assert_eq!(
            messages[1]["tool_calls"][0]["function"]["name"],
            "read_file"
        );
        assert_eq!(
            messages[1]["tool_calls"][0]["function"]["arguments"]["path"],
            "a"
        );
        assert_eq!(
            messages[1]["tool_calls"][1]["function"]["arguments"]["path"],
            "b"
        );
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["content"], "a body");
        assert_eq!(messages[2]["tool_name"], "read_file");
        assert_eq!(messages[3]["content"], "b body");
    }

    /// A recorded two-call parallel batch from `/api/chat`, **both calls
    /// naming the same tool**. Transcribed from a `qwen3` stream: Ollama
    /// sends a whole batch on one line, with no ids anywhere.
    const PARALLEL_TOOL_BATCH_NDJSON: &str = concat!(
        r#"{"model":"qwen3","message":{"role":"assistant","content":"reading both","tool_calls":[{"function":{"name":"read_file","arguments":{"path":"a"}}},{"function":{"name":"read_file","arguments":{"path":"b"}}}]},"done":false}"#,
        "\n",
        r#"{"model":"qwen3","message":{"role":"assistant","content":""},"done":true,"done_reason":"stop","prompt_eval_count":2048,"eval_count":31}"#,
        "\n",
    );

    /// A two-call parallel batch, driven through the real client to a
    /// recorded Ollama stream.
    ///
    /// Ollama is the one backend in this family that cannot carry a
    /// `tool_call_id`: it pairs a result to a call by the order the `tool`
    /// messages arrive in. That makes the wire order load-bearing here in a
    /// way it is nowhere else, and a batch answered out of order is not an
    /// error the server reports — it is the wrong file's contents handed back
    /// as the answer to the other read.
    #[tokio::test]
    async fn a_parallel_batch_reaches_ollama_in_call_order() {
        use crate::llm::test_support::{Recorded, parallel_batch_request};

        let recorded = Recorded::replay(PARALLEL_TOOL_BATCH_NDJSON).await;
        let client = OllamaClient::new(recorded.root.as_str());

        // `num_ctx` is set so the request goes straight out: an unset one
        // sends a `/api/show` probe first, and the recording answers exactly
        // one connection.
        let mut request = parallel_batch_request("qwen3");
        request.options = Some(ChatOptions {
            temperature: None,
            num_ctx: Some(16_384),
            reasoning_effort: None,
        });

        let mut stream = client.chat_stream(request).await.expect("stream opens");
        let mut chunks = Vec::new();
        while let Some(chunk) = stream.next().await {
            chunks.push(chunk.expect("chunk decodes"));
        }

        let sent = recorded.request_body();
        let messages = sent["messages"].as_array().expect("messages");
        assert_eq!(
            messages.len(),
            5,
            "system, user, assistant, then one wire message per result: {sent}"
        );
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[4]["role"], "tool");
        assert_eq!(
            (
                messages[3]["content"].as_str(),
                messages[4]["content"].as_str()
            ),
            (Some("contents of a"), Some("contents of b")),
            "the results follow the order of the calls that produced them"
        );
        assert_eq!(
            messages[2]["tool_calls"][0]["function"]["arguments"]["path"],
            "a"
        );
        assert_eq!(
            messages[2]["tool_calls"][1]["function"]["arguments"]["path"],
            "b"
        );
        assert!(
            !sent.to_string().contains("tool_call_id"),
            "the native API defines no such field: {sent}"
        );

        // Coming back, the batch survives as two calls with two distinct ids,
        // minted at this seam because Ollama sent none. Both name the same
        // tool, so an id that repeated would leave the second call
        // unanswerable everywhere downstream.
        let decoded: Vec<(String, String)> = chunks
            .iter()
            .filter_map(|chunk| chunk.message.as_ref())
            .flat_map(|message| {
                message
                    .tool_calls()
                    .into_iter()
                    .map(|call| (call.id.clone(), call.function.name.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(decoded.len(), 2, "the batch is not collapsed: {decoded:?}");
        assert_eq!(decoded[0].1, "read_file");
        assert_eq!(decoded[1].1, "read_file");
        assert!(!decoded[0].0.is_empty(), "an id was minted");
        assert_ne!(decoded[0].0, decoded[1].0, "and the two differ");

        let last = chunks.last().expect("a final chunk");
        assert!(last.done);
        assert_eq!(last.prompt_eval_count, Some(2048));
        assert_eq!(last.eval_count, Some(31));
    }

    /// Ollama sends no ids at all, so one is minted at the seam: everything
    /// downstream correlates a result to its call by id, and an empty one
    /// would collapse a batch into a single unanswerable call.
    #[test]
    fn decoded_tool_calls_get_an_id_ollama_never_sent() {
        let chunk = parse_chunk_line(
            r#"{"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"read_file","arguments":{"path":"a"}}},{"function":{"name":"read_file","arguments":{"path":"b"}}}]},"done":false}"#,
        )
        .expect("parses");
        let message = chunk.message.expect("message");
        let calls = message.tool_calls();
        assert_eq!(calls.len(), 2);
        assert!(!calls[0].id.is_empty(), "an id was minted");
        assert_ne!(
            calls[0].id, calls[1].id,
            "two calls to the same tool must not share an id"
        );
    }

    #[test]
    fn user_images_flatten_to_the_native_base64_array() {
        let body = build_request_body(&request(vec![
            crate::llm::ChatMessage::user("plain"),
            crate::llm::ChatMessage::user_with_images(
                "what is this?",
                vec![
                    crate::llm::Image::new("QUJD", "image/png"),
                    crate::llm::Image::new("REVG", "image/webp"),
                ],
            ),
        ]))
        .expect("body");

        assert!(
            body["messages"][0].get("images").is_none(),
            "text-only messages are untouched"
        );
        // Ollama's native shape: bare base64 strings, no media type (it sniffs).
        assert_eq!(body["messages"][1]["content"], "what is this?");
        assert_eq!(body["messages"][1]["images"][0], "QUJD");
        assert_eq!(body["messages"][1]["images"][1], "REVG");
    }

    #[test]
    fn the_on_disk_path_never_reaches_the_wire() {
        // `Image::path` is bookkeeping for replaying a transcript, not content:
        // no provider sees it, including the one whose body is serde-derived.
        let image = crate::llm::Image::new("QUJD", "image/png")
            .at_path(std::path::PathBuf::from("/home/u/.wizard/images/s/abc.png"));
        let body = build_request_body(&request(vec![crate::llm::ChatMessage::user_with_images(
            "look",
            vec![image],
        )]))
        .expect("body");
        assert!(
            !body.to_string().contains(".wizard/images"),
            "no local path on the wire: {body}"
        );
    }

    #[test]
    fn assistant_images_are_named_in_the_text_not_sent_back_as_input() {
        let mut assistant = crate::llm::ChatMessage::assistant("here it is");
        assistant.push_image(crate::llm::Image::new("QUJD", "image/png"));
        let body = build_request_body(&request(vec![assistant])).expect("body");
        let content = body["messages"][0]["content"].as_str().expect("content");
        assert!(content.contains("here it is"));
        assert!(
            content.contains("generated 1 image(s) (image/png)"),
            "{content}"
        );
        assert!(body["messages"][0].get("images").is_none());
    }

    #[test]
    fn parses_content_delta_chunk() {
        let chunk = parse_chunk_line(
            r#"{"model":"m","message":{"role":"assistant","content":"hel"},"done":false}"#,
        )
        .expect("valid chunk");
        assert!(!chunk.done);
        let message = chunk.message.expect("message present");
        assert_eq!(message.role, Role::Assistant);
        assert_eq!(message.text(), "hel");
    }

    #[test]
    fn parses_tool_call_chunk() {
        let chunk = parse_chunk_line(
            r#"{"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"read_file","arguments":{"path":"src/main.rs"}}}]},"done":false}"#,
        )
        .expect("valid chunk");
        let message = chunk.message.expect("message present");
        assert_eq!(message.tool_calls().len(), 1);
        assert_eq!(message.tool_calls()[0].function.name, "read_file");
        assert_eq!(
            message.tool_calls()[0].function.arguments["path"],
            "src/main.rs"
        );
    }

    #[test]
    fn surfaces_in_band_error_line() {
        let err = parse_chunk_line(r#"{"error":"model 'x' not found"}"#)
            .expect_err("error line must fail");
        assert!(err.to_string().contains("model 'x' not found"));
    }

    #[test]
    fn an_in_band_error_is_typed_rather_than_left_to_the_transient_default() {
        // A model that is not installed will not install itself, and a
        // continuous run now *waits out* anything it is told is transient.
        let missing = classify_stream_error("model 'qwen3.6:27b' not found, try pulling it first");
        assert!(
            !crate::agent::error_is_transient(&missing),
            "a missing model must not be waited on: {missing:#}"
        );
        assert!(missing.downcast_ref::<OllamaError>().is_some());

        // Anything else stays retryable, but as a typed transport failure
        // rather than an unrecognized error that merely defaults that way.
        let busy = classify_stream_error("server busy, try again");
        assert!(crate::agent::error_is_transient(&busy), "{busy:#}");
        assert!(
            busy.downcast_ref::<ProviderError>().is_some(),
            "the classification must be deliberate, not a fall-through: {busy:#}"
        );
    }

    #[test]
    fn transient_classification() {
        let status = |code: u16| reqwest::StatusCode::from_u16(code).expect("valid status");
        assert!(
            OllamaError::Api {
                status: status(503),
                body: String::new(),
            }
            .is_transient()
        );
        assert!(
            OllamaError::Api {
                status: status(429),
                body: String::new(),
            }
            .is_transient()
        );
        assert!(
            !OllamaError::Api {
                status: status(400),
                body: String::new(),
            }
            .is_transient()
        );
        assert!(!OllamaError::ModelMissing("m".to_string()).is_transient());

        // The two classifications are one table, not two opinions of it. A
        // gateway in front of Ollama — a reverse proxy, a Tailscale front end,
        // a hosted Ollama-compatible endpoint — answers a long generation with
        // a 408, and this predicate used to call that permanent while
        // `ProviderError` called it transient, so which one ran decided
        // whether the run continued.
        for code in [408, 429, 500, 502, 503, 504, 529] {
            let api = OllamaError::Api {
                status: status(code),
                body: String::new(),
            };
            assert!(api.is_transient(), "HTTP {code} must be transient");
            assert_eq!(
                api.is_transient(),
                ProviderError::http(code, "x").is_transient(),
                "the two classifications must agree on HTTP {code}"
            );
        }
        for code in [400, 401, 403, 404, 413, 422] {
            let api = OllamaError::Api {
                status: status(code),
                body: String::new(),
            };
            assert!(!api.is_transient(), "HTTP {code} must not be transient");
            assert_eq!(
                api.is_transient(),
                ProviderError::http(code, "x").is_transient(),
                "the two classifications must agree on HTTP {code}"
            );
        }
    }

    #[test]
    fn typed_errors_downcast_to_both_error_types() {
        let status = |code: u16| reqwest::StatusCode::from_u16(code).expect("valid status");
        let err = typed(OllamaError::Api {
            status: status(503),
            body: "busy".to_string(),
        });
        let ollama = err.downcast_ref::<OllamaError>().expect("legacy type");
        let provider = err.downcast_ref::<ProviderError>().expect("shared type");
        assert_eq!(provider.status, Some(503));
        assert_eq!(ollama.is_transient(), provider.is_transient());
        assert!(provider.message.contains("busy"), "body surfaces");

        // ModelMissing carries its originating 404 so both classifications
        // agree that it is not retryable.
        let err = typed(OllamaError::ModelMissing("m".to_string()));
        let provider = err.downcast_ref::<ProviderError>().expect("shared type");
        assert_eq!(provider.status, Some(404));
        assert!(!provider.is_transient());
    }

    #[test]
    fn model_installed_treats_a_bare_name_as_latest() {
        let installed = vec![
            "llama3:latest".to_string(),
            "qwen3.5:9b".to_string(),
            "myuser/coder".to_string(),
        ];
        assert!(model_installed("llama3", &installed), "bare = :latest");
        assert!(model_installed("llama3:latest", &installed));
        assert!(model_installed("qwen3.5:9b", &installed), "exact tag");
        assert!(
            model_installed("myuser/coder:latest", &installed),
            "installed side normalizes too"
        );
        assert!(
            !model_installed("qwen3.5", &installed),
            "a bare name never matches a versioned tag"
        );
        assert!(!model_installed("qwen3.6:27b", &installed));
        assert!(!model_installed("llama3", &[]));
    }

    #[test]
    fn pull_lines_parse_layers_milestones_and_errors() {
        let layer = parse_pull_line(
            r#"{"status":"pulling ab12","digest":"sha256:ab12","total":100,"completed":25}"#,
        )
        .expect("layer line");
        assert_eq!(layer.digest.as_deref(), Some("sha256:ab12"));
        assert_eq!(layer.total, Some(100));
        assert_eq!(layer.completed, Some(25));

        let milestone = parse_pull_line(r#"{"status":"verifying sha256 digest"}"#).expect("status");
        assert_eq!(milestone.status.as_deref(), Some("verifying sha256 digest"));
        assert_eq!(milestone.digest, None);

        let error = parse_pull_line(r#"{"error":"pull model manifest: file does not exist"}"#)
            .expect("error line");
        assert_eq!(
            error.error.as_deref(),
            Some("pull model manifest: file does not exist")
        );

        assert!(parse_pull_line("not json").is_err());
    }

    /// [`Progress`] sink that records every call as a plain string.
    #[derive(Default)]
    struct Recording(Arc<Mutex<Vec<String>>>);

    impl Progress for Recording {
        fn status(&self, line: &str) {
            self.0.lock().unwrap().push(format!("status:{line}"));
        }
        fn bytes(&self, label: &str, total: Option<u64>) -> Box<dyn crate::progress::ByteProgress> {
            self.0
                .lock()
                .unwrap()
                .push(format!("bar:{label}:{}", total.unwrap_or(0)));
            Box::new(RecordingBar(Arc::clone(&self.0)))
        }
    }

    struct RecordingBar(Arc<Mutex<Vec<String>>>);

    impl ByteProgress for RecordingBar {
        fn inc(&self, n: u64) {
            self.0.lock().unwrap().push(format!("inc:{n}"));
        }
        fn finish(self: Box<Self>, _msg: &str) {
            self.0.lock().unwrap().push("finish".to_string());
        }
    }

    #[test]
    fn pull_render_opens_one_bar_per_layer_and_ticks_deltas() {
        let sink = Recording::default();
        let mut render = PullRender::new(&sink, "my-model");
        let lines = [
            r#"{"status":"pulling manifest"}"#,
            // First layer: two progress lines — one bar, delta-ticked.
            r#"{"status":"pulling ab","digest":"sha256:abcdef012345","total":100,"completed":40}"#,
            r#"{"status":"pulling ab","digest":"sha256:abcdef012345","total":100,"completed":100}"#,
            // Second layer: a new bar.
            r#"{"status":"pulling cd","digest":"sha256:cd","total":10,"completed":10}"#,
            r#"{"status":"verifying sha256 digest"}"#,
            r#"{"status":"writing manifest"}"#,
        ];
        let mut done = false;
        for line in lines {
            done |= apply_pull_line(&mut render, "my-model", line).expect("line applies");
        }
        assert!(!done, "no success line yet");
        done = apply_pull_line(&mut render, "my-model", r#"{"status":"success"}"#).expect("ok");
        assert!(done, "success line reported");
        render.close();

        let events = sink.0.lock().unwrap().clone();
        assert_eq!(
            events,
            vec![
                "status:pulling manifest",
                "bar:pulling my-model (sha256:abcdef01…):100",
                "inc:40",
                "inc:60",
                "finish", // first layer bar closed by the second layer
                "bar:pulling my-model (sha256:cd):10",
                "inc:10",
                "finish", // second layer bar closed by the milestone
                "status:verifying sha256 digest",
                "status:writing manifest",
            ]
        );
    }

    #[test]
    fn pull_render_skips_blank_lines_and_regressions() {
        let sink = Recording::default();
        let mut render = PullRender::new(&sink, "m");
        assert!(!apply_pull_line(&mut render, "m", "  \n").expect("blank ok"));
        // completed going backwards (Ollama re-verifying) never underflows.
        for line in [
            r#"{"digest":"sha256:ab","total":100,"completed":50}"#,
            r#"{"digest":"sha256:ab","total":100,"completed":30}"#,
        ] {
            apply_pull_line(&mut render, "m", line).expect("applies");
        }
        render.close();
        let events = sink.0.lock().unwrap().clone();
        assert_eq!(
            events,
            vec!["bar:pulling m (sha256:ab):100", "inc:50", "finish"]
        );
    }

    #[test]
    fn in_band_pull_errors_bail_with_the_model_name() {
        let sink = Recording::default();
        let mut render = PullRender::new(&sink, "bogus:tag");
        let err = apply_pull_line(
            &mut render,
            "bogus:tag",
            r#"{"error":"pull model manifest: file does not exist"}"#,
        )
        .expect_err("error line must fail");
        assert!(err.to_string().contains("bogus:tag"));
        assert!(err.to_string().contains("file does not exist"));
    }

    #[test]
    fn context_length_is_read_from_model_info() {
        let info = serde_json::json!({
            "general.architecture": "qwen3",
            "qwen3.context_length": 40_960,
            "qwen3.embedding_length": 1024,
        });
        assert_eq!(context_length_from_model_info(&info), Some(40_960));
        assert_eq!(
            context_length_from_model_info(&serde_json::json!({"general.architecture": "x"})),
            None
        );
        assert_eq!(
            context_length_from_model_info(&serde_json::Value::Null),
            None
        );
    }

    #[tokio::test]
    async fn derived_num_ctx_falls_back_and_caches_when_probe_fails() {
        // Port 1 on localhost: connection refused immediately, no server needed.
        let client = OllamaClient::new("http://127.0.0.1:1");
        assert_eq!(client.derived_num_ctx("m").await, DEFAULT_NUM_CTX);
        assert_eq!(
            client.num_ctx_cache.lock().unwrap().get("m"),
            Some(&DEFAULT_NUM_CTX),
            "fallback is cached so the probe is not retried per request"
        );
        assert_eq!(client.context_window("m").await, Some(DEFAULT_NUM_CTX));
    }

    #[tokio::test]
    async fn decodes_split_ndjson_lines() {
        // One line split across two network reads, plus a final done line.
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(br#"{"message":{"role":"assistant","#.to_vec()),
            Ok(br#""content":"hi"},"done":false}"#.to_vec()),
            Ok(b"\n".to_vec()),
            Ok(
                br#"{"message":{"role":"assistant","content":""},"done":true,"done_reason":"stop","eval_count":7}"#
                    .to_vec(),
            ),
        ];
        let mut chunks = decode_ndjson(stream::iter(parts));
        let first = chunks
            .next()
            .await
            .expect("first chunk")
            .expect("first chunk ok");
        assert_eq!(first.message.expect("message").text(), "hi");
        assert!(!first.done);
        let last = chunks
            .next()
            .await
            .expect("final chunk")
            .expect("final chunk ok");
        assert!(last.done);
        assert_eq!(last.done_reason.as_deref(), Some("stop"));
        assert_eq!(last.eval_count, Some(7));
        assert!(chunks.next().await.is_none(), "stream ends after done");
    }

    #[tokio::test]
    async fn stops_after_done_chunk_even_with_trailing_data() {
        let parts: Vec<Result<Vec<u8>>> = vec![Ok(
            b"{\"done\":true}\n{\"message\":{\"role\":\"assistant\",\"content\":\"x\"},\"done\":false}\n".to_vec(),
        )];
        let mut chunks = decode_ndjson(stream::iter(parts));
        let first = chunks.next().await.expect("chunk").expect("ok");
        assert!(first.done);
        assert!(chunks.next().await.is_none());
    }

    #[tokio::test]
    async fn flushes_trailing_line_without_newline_at_eof() {
        let parts: Vec<Result<Vec<u8>>> = vec![Ok(
            br#"{"message":{"role":"assistant","content":"all"},"done":true}"#.to_vec(),
        )];
        let mut chunks = decode_ndjson(stream::iter(parts));
        let only = chunks.next().await.expect("chunk").expect("ok");
        assert!(only.done);
        assert_eq!(only.message.expect("message").text(), "all");
        assert!(chunks.next().await.is_none());
    }

    /// A stream that stops before any line said `done: true` is a failure,
    /// not a shorter reply.
    ///
    /// This used to end with `Ok(None)`: the agent got every token that had
    /// arrived and no final chunk at all, which is what a turn that finished
    /// normally also looks like from there. `ollama serve` being OOM-killed
    /// mid-generation, or a tunnel to a remote box dropping, both land here,
    /// and both are worth another attempt.
    #[tokio::test]
    async fn a_stream_that_stops_before_done_is_a_transient_failure() {
        let parts: Vec<Result<Vec<u8>>> = vec![Ok(
            b"{\"message\":{\"role\":\"assistant\",\"content\":\"half a sen\"},\"done\":false}\n"
                .to_vec(),
        )];
        let mut chunks = decode_ndjson(stream::iter(parts));

        let first = chunks.next().await.expect("chunk").expect("ok");
        assert_eq!(first.message.expect("message").text(), "half a sen");

        let err = chunks
            .next()
            .await
            .expect("an item")
            .expect_err("a cut stream is not a completed reply");
        let provider = err
            .downcast_ref::<ProviderError>()
            .expect("typed, or the ladder cannot classify it");
        assert_eq!(provider.status, None);
        assert!(provider.is_transient());

        // The same for a stream that produced nothing at all: a 200 whose body
        // never arrived is a failure, not an empty answer.
        let empty: Vec<Result<Vec<u8>>> = vec![Ok(Vec::new())];
        let err = decode_ndjson(stream::iter(empty))
            .next()
            .await
            .expect("an item")
            .expect_err("an empty body is not an empty reply");
        assert!(
            err.downcast_ref::<ProviderError>()
                .expect("typed")
                .is_transient()
        );
    }

    /// The trailing-line case must still be able to *end* the stream: a
    /// `done: true` that arrived without its newline is a complete reply, and
    /// refusing it would turn every such response into an endless retry.
    #[tokio::test]
    async fn a_done_line_without_its_newline_still_ends_the_stream_cleanly() {
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(
                b"{\"message\":{\"role\":\"assistant\",\"content\":\"hi\"},\"done\":false}\n"
                    .to_vec(),
            ),
            Ok(br#"{"done":true,"done_reason":"stop"}"#.to_vec()),
        ];
        let mut chunks = decode_ndjson(stream::iter(parts));
        assert!(!chunks.next().await.expect("text").expect("ok").done);
        let last = chunks.next().await.expect("final").expect("ok");
        assert!(last.done);
        assert_eq!(last.done_reason.as_deref(), Some("stop"));
        assert!(chunks.next().await.is_none());
    }

    #[tokio::test]
    async fn propagates_transport_errors() {
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(b"{\"done\":false}\n".to_vec()),
            Err(anyhow!("connection reset")),
        ];
        let mut chunks = decode_ndjson(stream::iter(parts));
        assert!(chunks.next().await.expect("chunk").is_ok());
        let err = chunks.next().await.expect("item").expect_err("error");
        assert!(err.to_string().contains("connection reset"));
    }

    #[tokio::test]
    async fn a_rate_limited_response_carries_the_retry_after_under_both_typed_errors() {
        // Ollama itself does not rate-limit, but a reverse proxy or a hosted
        // Ollama-compatible endpoint in front of it does, and the header is
        // the only thing that stops the agent's ladder from guessing. All
        // three classifications have to survive on the one chain: the legacy
        // `OllamaError` path, the shared `ProviderError` contract, and the
        // wait itself.
        let host = crate::llm::test_support::one_shot_http_server(
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 12\r\nContent-Length: \
             9\r\nConnection: close\r\n\r\nslow down",
        )
        .await;
        let client = OllamaClient::new(host);
        let err = client.health().await.expect_err("429 is not healthy");

        let provider = err
            .downcast_ref::<ProviderError>()
            .expect("the shared provider classification");
        assert_eq!(provider.status, Some(429));
        assert!(provider.is_transient());
        let ollama = err
            .downcast_ref::<OllamaError>()
            .expect("the Ollama-specific classification is still reachable");
        assert!(ollama.is_transient());
        assert_eq!(
            err.downcast_ref::<crate::llm::RetryAfter>()
                .map(|hint| hint.0),
            Some(Duration::from_secs(12)),
            "the proxy's own deadline reaches the retry loop"
        );
        // The message users see is still the provider's, not the hint's.
        assert!(err.to_string().contains("HTTP 429"), "{err}");
    }
}
