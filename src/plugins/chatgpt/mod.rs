//! A ChatGPT **subscription**, as a plugin, behind `--features
//! provider-chatgpt`: the Responses API at `chatgpt.com/backend-api/codex`,
//! reached with OAuth tokens from [`oauth`] rather than an API key.
//!
//! Transport and sign-in ship together, unlike xAI's, because nothing outside
//! this plugin reads the ChatGPT token store: no tool authenticates against
//! OpenAI's non-chat APIs with it, so the store has exactly one consumer and
//! belongs with it. What is shared with xAI is the loopback redirect the
//! browser comes back on, and that stayed in [`crate::llm::oauth_callback`].
//!
//! This is not the OpenAI Chat Completions API: the request is the Responses
//! shape (`instructions` + `input` items), the stream is Responses SSE
//! (`response.output_text.delta`, `response.output_item.done`, …), and every
//! call carries the ChatGPT account id and the Codex client identity that the
//! endpoint requires. Wizard's native [`ChatRequest`] is translated in and the
//! SSE is decoded back into Wizard's [`ChatChunk`] stream, so the agent core
//! sees the same interface as every other provider.
//!
//! One consequence of the Responses shape is worth stating up front: this
//! client sends `store: false`, so the endpoint remembers *nothing* between
//! requests and the whole conversation is re-sent every step, reasoning
//! included. That is why [`build_input`] replays a `reasoning` item for every
//! thinking block it is handed, and why [`decode_sse`] keeps the encrypted
//! reasoning the request asks for in `include`. Without both halves a
//! reasoning model re-derives its entire chain of thought on every step of a
//! multi-step turn, and is billed for it every time.

pub mod oauth;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use futures_util::{Stream, StreamExt, stream};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::kernel::{Capability, Ctx, Plugin, PluginManifest};
use crate::llm::provider::LlmProvider;
use crate::llm::{
    CacheTokens, ChatChunk, ChatMessage, ChatRequest, ChatStream, ContentBlock, FunctionCall,
    ProviderError, Role, ThinkingBlock, ToolCall,
};
use oauth::StoredTokens;

/// Static fallback model list; the live list comes from `GET /models`.
const FALLBACK_MODELS: &[&str] = &["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna", "gpt-5.5"];

/// Manages the stored OAuth tokens: hands out the bearer + account id, and
/// refreshes proactively (near expiry) or after a 401.
#[derive(Debug)]
pub struct ChatgptTokens {
    path: PathBuf,
    /// The tokens and when they were last replaced, under one lock that is
    /// held *across* the refresh call. See [`ChatgptTokens::refresh`].
    cache: Mutex<TokenCache>,
}

/// The cached tokens plus the moment they were last refreshed.
#[derive(Debug, Default)]
struct TokenCache {
    tokens: Option<StoredTokens>,
    /// When [`ChatgptTokens::refresh`] last succeeded; `None` before the first
    /// one. Read by [`ChatgptTokens::refresh_after_unauthorized`] to tell a
    /// 401 that still needs answering from one somebody else has already
    /// answered.
    refreshed_at: Option<std::time::Instant>,
}

impl ChatgptTokens {
    pub fn new() -> Result<Self> {
        Ok(Self {
            path: oauth::token_path()?,
            cache: Mutex::new(TokenCache::default()),
        })
    }

    /// Load the tokens into `cache` on first use and hand back a copy.
    ///
    /// "Not signed in" is a permanent 401 rather than a bare message: no
    /// backoff produces a token file that is not there, and an untyped error
    /// takes [`error_is_transient`](crate::agent::error_is_transient)'s
    /// permissive fallback, which spends the whole retry ladder and a
    /// circuit-breaker trip before showing the user the one line that helps.
    fn ensure_loaded(&self, cache: &mut TokenCache) -> Result<StoredTokens> {
        if cache.tokens.is_none() {
            cache.tokens = oauth::load_tokens(&self.path)?;
        }
        cache.tokens.clone().ok_or_else(|| {
            anyhow::Error::new(ProviderError::http(
                401,
                "not signed in to ChatGPT; run `wizard --login chatgpt` first",
            ))
        })
    }

    /// The `(access_token, account_id)` to authorize a request, refreshing the
    /// access token first if it is close to expiry.
    async fn credentials(&self) -> Result<(String, Option<String>)> {
        let mut cache = self.cache.lock().await;
        let tokens = self.ensure_loaded(&mut cache)?;
        if oauth::expires_soon(&tokens.access_token)
            && let Some(refreshed) = self.refresh(&mut cache).await?
        {
            return Ok((refreshed.access_token, refreshed.account_id));
        }
        Ok((tokens.access_token, tokens.account_id))
    }

    /// Force a refresh after a 401 — unless somebody already did.
    ///
    /// The 401 being reacted to happened strictly before this call started, so
    /// a refresh that *completed* after it started has already replaced the
    /// rejected token and there is nothing left to do. Skipping the second
    /// refresh is not an optimization: OpenAI rotates the refresh token, so a
    /// burst of parallel subagents meeting one expiry would queue N refreshes
    /// here, each spending a grant the previous one had just superseded, and
    /// the first to be judged stale comes back as a [`RevokedGrant`] — which
    /// *deletes the token file* and signs the user out of a session that was
    /// perfectly valid.
    ///
    /// The timestamp is read before the lock deliberately: the wait for the
    /// lock is exactly the window in which another task's refresh lands.
    async fn refresh_after_unauthorized(&self) -> Result<bool> {
        let entered = std::time::Instant::now();
        let mut cache = self.cache.lock().await;
        if cache
            .refreshed_at
            .is_some_and(|refreshed| refreshed >= entered)
        {
            return Ok(true);
        }
        Ok(self.refresh(&mut cache).await?.is_some())
    }

    /// Exchange the refresh token for new tokens, persist, and cache them.
    /// `None` when there is no refresh token to use.
    ///
    /// Takes the locked cache rather than acquiring it, so the whole
    /// read-refresh-write is one critical section. It used to drop the lock
    /// around the HTTP call, which let two callers read the same single-use
    /// refresh token and spend it twice.
    async fn refresh(&self, cache: &mut TokenCache) -> Result<Option<StoredTokens>> {
        let current = self.ensure_loaded(cache)?;
        let Some(refresh_token) = current.refresh_token.clone() else {
            return Ok(None);
        };
        let response = match oauth::refresh(&refresh_token).await {
            Ok(response) => response,
            // A revoked/expired grant never refreshes again: forget the stored
            // tokens so the next run re-prompts for sign-in (as xai_oauth does).
            Err(err) if err.is::<oauth::RevokedGrant>() => {
                let _ = oauth::clear_tokens(&self.path);
                cache.tokens = None;
                return Err(err);
            }
            Err(err) => return Err(err),
        };
        // A refresh may omit the refresh token or id_token; keep what we had.
        let id_token = response.id_token.or(current.id_token);
        let account_id = id_token
            .as_deref()
            .and_then(oauth::account_id_from_id_token)
            .or(current.account_id);
        let merged = StoredTokens {
            access_token: response.access_token,
            refresh_token: response.refresh_token.or(current.refresh_token),
            id_token,
            account_id,
        };
        // Persisted before it is cached, so a re-exec or a second Wizard picks
        // up the token this one just minted rather than replaying the spent
        // refresh token still on disk.
        oauth::save_tokens(&self.path, &merged)?;
        cache.tokens = Some(merged.clone());
        cache.refreshed_at = Some(std::time::Instant::now());
        Ok(Some(merged))
    }
}

/// Client for one ChatGPT-subscription account.
#[derive(Debug)]
pub struct ChatgptProvider {
    http: reqwest::Client,
    base_url: String,
    model: String,
    tokens: Arc<ChatgptTokens>,
    /// A stable id for this process's requests (the endpoint expects one).
    session_id: String,
}

impl ChatgptProvider {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Result<Self> {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        let http = crate::llm::chat_http_builder(crate::llm::client_read_timeout_for(&base_url))
            .build()
            .unwrap_or_default();
        Ok(Self {
            http,
            base_url,
            model: model.into(),
            tokens: Arc::new(ChatgptTokens::new()?),
            session_id: session_id(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// Attach the auth + Codex-client headers a subscription request needs.
    async fn authed(&self, builder: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder> {
        let (access, account) = self.tokens.credentials().await?;
        let mut builder = builder
            .header("Authorization", format!("Bearer {access}"))
            .header("originator", oauth::API_ORIGINATOR)
            .header("User-Agent", user_agent())
            .header("session-id", &self.session_id)
            .header("OpenAI-Beta", "responses=experimental");
        if let Some(account) = account {
            builder = builder.header("ChatGPT-Account-ID", account);
        }
        Ok(builder)
    }

    fn build_request_body(&self, request: &ChatRequest) -> Value {
        let (instructions, input) = build_input(&request.messages);
        let mut body = json!({
            "model": request.model,
            "input": input,
            "tool_choice": "auto",
            "parallel_tool_calls": true,
            // The subscription endpoint is stateless per call for this client.
            "store": false,
            "stream": true,
            "include": ["reasoning.encrypted_content"],
        });
        if !instructions.is_empty() {
            body["instructions"] = Value::String(instructions);
        }
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|spec| {
                json!({
                    "type": "function",
                    "name": spec.function.name,
                    "description": spec.function.description,
                    "parameters": spec.function.parameters,
                })
            })
            .collect();
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
        if let Some(options) = &request.options
            && let Some(effort) = &options.reasoning_effort
        {
            body["reasoning"] = json!({ "effort": effort, "summary": "auto" });
        }
        body
    }

    async fn post_responses(&self, request: &ChatRequest) -> Result<reqwest::Response> {
        let body = self.build_request_body(request);
        let send = || async {
            self.authed(self.http.post(self.url("/responses")).json(&body))
                .await?
                .header("Accept", "text/event-stream")
                .send()
                .await
                .with_context(|| format!("chat request to {} failed", self.base_url))
        };
        let mut response = send().await?;
        // One refresh-and-retry on 401, exactly as the keyed OpenAI client does.
        if response.status() == reqwest::StatusCode::UNAUTHORIZED
            && self
                .tokens
                .refresh_after_unauthorized()
                .await
                .unwrap_or(false)
        {
            response = send().await?;
        }
        Ok(response)
    }

    /// Error for a non-success HTTP response. Takes the response whole so the
    /// server's `Retry-After` is read before `text()` consumes it: a plan
    /// usage limit is exactly the case where the endpoint knows the wait and
    /// our ladder does not.
    async fn http_failure(&self, response: reqwest::Response) -> anyhow::Error {
        let status = response.status();
        let retry_after = crate::llm::retry_after_from_headers(response.headers());
        let body = response.text().await.unwrap_or_default();
        let hint = if status == reqwest::StatusCode::UNAUTHORIZED {
            " — sign in again from Settings"
        } else if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            " — your ChatGPT plan's usage limit was reached"
        } else {
            ""
        };
        crate::llm::http_error_with_retry_after(
            status.as_u16(),
            format!("ChatGPT returned HTTP {status}{hint}: {body}"),
            retry_after,
        )
    }
}

#[async_trait]
impl LlmProvider for ChatgptProvider {
    async fn health(&self) -> Result<()> {
        let response = self
            .authed(self.http.get(self.url("/models")))
            .await?
            .send()
            .await
            .with_context(|| format!("cannot reach {}", self.base_url))?;
        if response.status().is_success() {
            return Ok(());
        }
        Err(self.http_failure(response).await)
    }

    async fn supports_native_tools(&self, _model: &str) -> Result<bool> {
        Ok(true)
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        let response = match self.authed(self.http.get(self.url("/models"))).await {
            Ok(builder) => builder.send().await,
            Err(_) => return Ok(fallback_models()),
        };
        let Ok(response) = response else {
            return Ok(fallback_models());
        };
        if !response.status().is_success() {
            return Ok(fallback_models());
        }
        match response.json::<ModelsResponse>().await {
            Ok(models) if !models.data.is_empty() => {
                Ok(models.data.into_iter().map(|m| m.id).collect())
            }
            _ => Ok(fallback_models()),
        }
    }

    async fn chat_stream(&self, request: ChatRequest) -> Result<ChatStream> {
        let response = self.post_responses(&request).await?;
        if !response.status().is_success() {
            return Err(self.http_failure(response).await);
        }
        let bytes = response
            .bytes_stream()
            .map(|item| match item {
                Ok(chunk) => Ok(chunk.to_vec()),
                Err(e) => Err(anyhow!(e).context(ProviderError::transport(
                    "ChatGPT response stream was interrupted",
                ))),
            })
            .boxed();
        Ok(decode_sse(bytes))
    }

    async fn context_window(&self, model: &str) -> Option<u32> {
        crate::llm::wire::context_window(model)
    }

    fn label(&self) -> String {
        format!("chatgpt:{}", self.model)
    }
}

fn fallback_models() -> Vec<String> {
    FALLBACK_MODELS.iter().map(|m| m.to_string()).collect()
}

/// A per-process request id. `getrandom` avoids the `Date`/`rand` bans in some
/// build contexts and is already a dependency.
fn session_id() -> String {
    let mut bytes = [0u8; 16];
    let _ = getrandom::fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn user_agent() -> String {
    format!("codex_cli_rs/{} (wizard)", env!("CARGO_PKG_VERSION"))
}

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    data: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
struct ModelEntry {
    id: String,
}

/// Translate native messages into `(instructions, input)`: system messages join
/// into `instructions`; user/assistant/tool turns become Responses `input`
/// items. Assistant tool calls become `function_call` items carrying the
/// `call_id` the provider itself issued, and each `tool_result` block becomes
/// a `function_call_output` item bound to that id, so a parallel batch
/// yields N consecutive outputs with nothing interleaved between them.
///
/// An assistant turn's reasoning is replayed too, as a `reasoning` item
/// ahead of the text and the calls it produced. That is not a nicety. The
/// request carries `store: false`, so the endpoint holds no state between
/// calls; a reasoning model handed back its own tool results but *not* its
/// own reasoning has to re-derive the whole chain of thought before it can
/// use them, on every step of every multi-step turn, and the account is
/// billed for those tokens every time. The request already asks for
/// `reasoning.encrypted_content`; this is the half that spends it.
fn build_input(messages: &[ChatMessage]) -> (String, Vec<Value>) {
    let mut instructions: Vec<String> = Vec::new();
    let mut input: Vec<Value> = Vec::new();

    for message in messages {
        match message.role {
            Role::System => {
                let text = message.text();
                if !text.is_empty() {
                    instructions.push(text);
                }
            }
            Role::User => {
                let mut content = vec![json!({ "type": "input_text", "text": message.text() })];
                // Responses takes images as `input_image` parts carrying a data
                // URI: the same base64 the other providers get, differently
                // wrapped.
                for image in message.images() {
                    content.push(json!({
                        "type": "input_image",
                        "image_url": image.data_uri(),
                    }));
                }
                input.push(json!({
                    "type": "message",
                    "role": "user",
                    "content": content,
                }));
            }
            Role::Assistant => {
                // What the turn *said*, assembled before its reasoning is
                // prepended: the API accepts a replayed `reasoning` item only
                // when the item it produced still follows it, so a turn with
                // nothing after the reasoning replays no reasoning either.
                let mut turn: Vec<Value> = Vec::new();
                // An assistant turn carries no image content on this API
                // either: images the model generated are named in its text.
                let text = crate::llm::assistant_content(message);
                if !text.is_empty() {
                    turn.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": text }],
                    }));
                }
                for call in message.tool_calls() {
                    // Responses wants arguments as a JSON string, not an object.
                    let arguments = match &call.function.arguments {
                        Value::Null => "{}".to_string(),
                        other => other.to_string(),
                    };
                    turn.push(json!({
                        "type": "function_call",
                        "name": call.function.name,
                        "arguments": arguments,
                        "call_id": call.id,
                    }));
                }
                if turn.is_empty() {
                    continue;
                }
                // Reasoning first, which is the order the model emitted it in:
                // it thinks, then it answers or calls a tool. The agent loop
                // builds the assistant message the same way round.
                for block in &message.content {
                    match block {
                        ContentBlock::Thinking(thinking) => {
                            input.extend(reasoning_item(thinking));
                        }
                        ContentBlock::Text(_)
                        | ContentBlock::Image(_)
                        | ContentBlock::ToolUse(_)
                        | ContentBlock::ToolResult(_) => {}
                    }
                }
                input.append(&mut turn);
            }
            Role::Tool => {
                for result in message.tool_results() {
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": result.tool_use_id,
                        "output": result.content,
                    }));
                }
            }
        }
    }
    (instructions.join("\n\n"), input)
}

/// The `reasoning` input item that replays one thinking block, or `None` when
/// there is nothing this endpoint can take back.
///
/// The reasoning itself is opaque: the endpoint hands it over encrypted (the
/// request's `include: ["reasoning.encrypted_content"]`) and only it can read
/// it back. A block without that payload is a summary with no reasoning
/// behind it (an Anthropic-signed thinking block from a session that changed
/// providers, or a stream where `include` was refused), and replaying it
/// would spend context on prose the model gains nothing from.
///
/// The item id rides in [`ThinkingBlock::signature`], which is the field for
/// "the opaque token this provider needs echoed back verbatim"; on Anthropic
/// the same field carries a `thinking.signature`. It is omitted rather than
/// invented when the stream did not carry one.
fn reasoning_item(block: &ThinkingBlock) -> Option<Value> {
    let encrypted = block.data.as_deref()?;
    // The summary is what the stream also emitted as `reasoning_summary_text`
    // deltas; it goes back in the shape it arrived in, and an item that never
    // had one goes back with an empty list rather than an empty string part.
    let summary = if block.thinking.is_empty() {
        Vec::new()
    } else {
        vec![json!({ "type": "summary_text", "text": block.thinking })]
    };
    let mut item = json!({
        "type": "reasoning",
        "encrypted_content": encrypted,
        "summary": summary,
    });
    if let Some(id) = block.signature.as_deref().filter(|id| !id.is_empty()) {
        item["id"] = Value::String(id.to_string());
    }
    Some(item)
}

/* --- SSE decoding --------------------------------------------------------- */

/// One decoded Responses SSE event (the subset Wizard acts on).
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum Event {
    #[serde(rename = "response.output_text.delta")]
    TextDelta { delta: String },
    #[serde(rename = "response.reasoning_summary_text.delta")]
    ReasoningDelta { delta: String },
    #[serde(rename = "response.output_item.done")]
    ItemDone { item: OutputItem },
    #[serde(rename = "response.completed")]
    Completed { response: CompletedResponse },
    /// A reply the endpoint stopped early (the output-token ceiling, the
    /// context window). Carries the same payload as `response.completed` plus
    /// `incomplete_details.reason`, and it is the *only* place this API says
    /// so, which is why it cannot be lumped in with the unknown events.
    #[serde(rename = "response.incomplete")]
    Incomplete { response: CompletedResponse },
    #[serde(rename = "response.failed")]
    Failed { response: FailedResponse },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum OutputItem {
    #[serde(rename = "function_call")]
    FunctionCall {
        name: String,
        /// Arguments as a JSON string, per the Responses wire format.
        #[serde(default)]
        arguments: String,
        /// The id the Responses API issued for this call. It comes straight
        /// back as the `function_call_output`'s `call_id`; it used to be
        /// dropped and re-invented, which is what made a parallel batch
        /// impossible to answer.
        #[serde(default)]
        call_id: String,
    },
    /// The model's reasoning for this turn. The request asks for
    /// `reasoning.encrypted_content`, so the payload arrives encrypted and
    /// is kept verbatim to be replayed on the next step (see
    /// [`reasoning_item`]); dropping it is what made a `store: false`
    /// reasoning model start every step from a blank slate.
    #[serde(rename = "reasoning")]
    Reasoning {
        /// The `rs_…` id the endpoint issued for this item.
        #[serde(default)]
        id: String,
        /// The reasoning itself, opaque and readable only by the endpoint.
        #[serde(default)]
        encrypted_content: Option<String>,
        /// The readable summary the stream also emitted as
        /// `response.reasoning_summary_text` deltas.
        #[serde(default)]
        summary: Vec<ReasoningSummary>,
    },
    #[serde(other)]
    Other,
}

/// One `summary_text` part of a reasoning item.
#[derive(Debug, Deserialize)]
struct ReasoningSummary {
    #[serde(default)]
    text: String,
}

#[derive(Debug, Deserialize)]
struct CompletedResponse {
    #[serde(default)]
    usage: Option<Usage>,
    /// Why the reply stopped short, on a `response.incomplete`.
    #[serde(default)]
    incomplete_details: Option<IncompleteDetails>,
}

/// Why the endpoint ended a reply early: `max_output_tokens`, or a context
/// window that ran out.
#[derive(Debug, Deserialize)]
struct IncompleteDetails {
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    /// The Responses API's nesting for the cached-prefix counter.
    #[serde(default)]
    input_tokens_details: Option<InputTokensDetails>,
}

/// `usage.input_tokens_details` (subset): the breakdown of `input_tokens`.
#[derive(Debug, Deserialize)]
struct InputTokensDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

impl Usage {
    /// Prompt tokens the endpoint served from its cache.
    ///
    /// A **subset** of `input_tokens`, the way OpenAI counts everywhere: the
    /// cached part is billed at a discount but still counted, so the two are
    /// never summed. This client sends `store: false` and re-sends the whole
    /// conversation every step, so on a multi-step turn this is most of the
    /// prompt and it is the difference between a turn priced honestly and one
    /// priced at up to 10x.
    fn cached_tokens(&self) -> u64 {
        self.input_tokens_details
            .as_ref()
            .and_then(|details| details.cached_tokens)
            .unwrap_or(0)
    }
}

#[derive(Debug, Deserialize)]
struct FailedResponse {
    #[serde(default)]
    error: Option<FailedError>,
}

#[derive(Debug, Deserialize)]
struct FailedError {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    code: Option<String>,
}

struct SseState<S> {
    bytes: S,
    buf: Vec<u8>,
    tool_calls: Vec<ToolCall>,
    /// Reasoning items seen this response, in arrival order, for replay on
    /// the next request.
    reasoning: Vec<ThinkingBlock>,
    prompt_tokens: Option<u64>,
    output_tokens: Option<u64>,
    /// Prompt tokens the endpoint served from its cache (a subset of
    /// `prompt_tokens`). See [`Usage::cached_tokens`].
    cached_prompt_tokens: u64,
    done: bool,
    /// The endpoint *said* the response was over: `response.completed`,
    /// `response.incomplete`, `response.failed`, or the `[DONE]` sentinel.
    /// EOF also sets [`SseState::done`], and separating the two is what stops
    /// a connection cut mid-generation from being handed to the agent as a
    /// complete, shorter reply — a success it has no reason to retry.
    terminated: bool,
    /// Why the reply ended, when the endpoint said it ended early.
    done_reason: Option<String>,
    emitted_final: bool,
    failure: Option<String>,
}

/// Decode a Responses SSE byte stream into a [`ChatStream`]: text and reasoning
/// summary deltas are emitted live; completed `reasoning` and `function_call`
/// items are accumulated and flushed in one synthesized `done: true` chunk at
/// the end.
pub(crate) fn decode_sse<S>(bytes: S) -> ChatStream
where
    S: Stream<Item = Result<Vec<u8>>> + Send + Unpin + 'static,
{
    let state = SseState {
        bytes,
        buf: Vec::new(),
        tool_calls: Vec::new(),
        reasoning: Vec::new(),
        prompt_tokens: None,
        output_tokens: None,
        cached_prompt_tokens: 0,
        done: false,
        terminated: false,
        done_reason: None,
        emitted_final: false,
        failure: None,
    };
    stream::try_unfold(state, |mut state| async move {
        loop {
            if state.emitted_final {
                return Ok(None);
            }
            while let Some(pos) = state.buf.iter().position(|&b| b == b'\n') {
                let line_bytes: Vec<u8> = state.buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line_bytes);
                let line = line.trim();
                let Some(payload) = line.strip_prefix("data:") else {
                    continue;
                };
                let payload = payload.trim();
                if payload == "[DONE]" {
                    state.done = true;
                    state.terminated = true;
                    continue;
                }
                let event: Event = match serde_json::from_str(payload) {
                    Ok(event) => event,
                    Err(_) => continue,
                };
                match event {
                    Event::TextDelta { delta } if !delta.is_empty() => {
                        return Ok(Some((text_chunk(delta, false), state)));
                    }
                    Event::ReasoningDelta { delta } if !delta.is_empty() => {
                        return Ok(Some((text_chunk(delta, true), state)));
                    }
                    Event::ItemDone {
                        item:
                            OutputItem::FunctionCall {
                                name,
                                arguments,
                                call_id,
                            },
                    } => {
                        let arguments = serde_json::from_str(&arguments).unwrap_or(Value::Null);
                        state.tool_calls.push(ToolCall {
                            id: call_id,
                            function: FunctionCall { name, arguments },
                        });
                    }
                    Event::ItemDone {
                        item:
                            OutputItem::Reasoning {
                                id,
                                encrypted_content,
                                summary,
                            },
                    } => {
                        let thinking: String = summary.into_iter().map(|part| part.text).collect();
                        // Kept even when the payload is missing: what makes a
                        // block replayable is `reasoning_item`'s call, and a
                        // session file that records what the model thought is
                        // worth more than one that records nothing.
                        state.reasoning.push(ThinkingBlock {
                            thinking,
                            signature: (!id.is_empty()).then_some(id),
                            data: encrypted_content,
                        });
                    }
                    Event::Completed { response } | Event::Incomplete { response } => {
                        if let Some(usage) = response.usage {
                            state.prompt_tokens = usage.input_tokens;
                            state.output_tokens = usage.output_tokens;
                            state.cached_prompt_tokens = usage.cached_tokens();
                        }
                        // A reply the endpoint cut short has to reach the agent
                        // as a finish reason, not as an ordinary stop: when the
                        // ceiling lands mid `function_call`, the arguments
                        // string is half-written and decodes to nothing usable,
                        // and dispatching that runs a *different* action than
                        // the model asked for (see
                        // `agent::turn::truncated_tool_call`).
                        if let Some(reason) = response
                            .incomplete_details
                            .and_then(|details| details.reason)
                        {
                            state.done_reason = Some(reason);
                        }
                        state.done = true;
                        state.terminated = true;
                    }
                    Event::Failed { response } => {
                        let error = response.error;
                        let message = error
                            .as_ref()
                            .and_then(|e| e.message.clone())
                            .or_else(|| error.as_ref().and_then(|e| e.code.clone()))
                            .unwrap_or_else(|| "the response failed".to_string());
                        state.failure = Some(message);
                        state.done = true;
                        state.terminated = true;
                    }
                    _ => {}
                }
            }
            if state.done {
                if let Some(message) = state.failure.take() {
                    return Err(anyhow!(ProviderError::http(502, message)));
                }
                if !state.terminated {
                    return Err(crate::llm::stream_ended_early("the ChatGPT stream"));
                }
                state.emitted_final = true;
                return Ok(Some((build_final(&mut state), state)));
            }
            match state.bytes.next().await {
                Some(Ok(data)) => state.buf.extend_from_slice(&data),
                Some(Err(e)) => return Err(e),
                None => {
                    if !state.buf.is_empty() && state.buf.last() != Some(&b'\n') {
                        state.buf.push(b'\n');
                    }
                    state.done = true;
                }
            }
        }
    })
    .boxed()
}

fn text_chunk(text: String, thinking: bool) -> ChatChunk {
    ChatChunk {
        message: Some(ChatMessage::assistant(text)),
        images: Vec::new(),
        thinking,
        done: false,
        done_reason: None,
        eval_count: None,
        prompt_eval_count: None,
        cache: CacheTokens::NONE,
    }
}

fn build_final<S>(state: &mut SseState<S>) -> ChatChunk {
    let mut tool_calls = std::mem::take(&mut state.tool_calls);
    // The Responses API always sends a `call_id`; this only covers a proxy
    // that does not.
    crate::llm::ensure_tool_call_ids(&mut tool_calls);
    // Reasoning ahead of the calls it produced: that is the order the endpoint
    // emitted them in, and the order `build_input` has to replay them in.
    let mut content: Vec<ContentBlock> = std::mem::take(&mut state.reasoning)
        .into_iter()
        .map(ContentBlock::Thinking)
        .collect();
    content.extend(tool_calls.into_iter().map(ContentBlock::ToolUse));
    ChatChunk {
        message: Some(ChatMessage::new(Role::Assistant, content)),
        images: Vec::new(),
        thinking: false,
        done: true,
        // Only an early ending is reported; anything else stopped normally.
        done_reason: Some(
            state
                .done_reason
                .take()
                .unwrap_or_else(|| "stop".to_string()),
        ),
        eval_count: state.output_tokens,
        prompt_eval_count: state.prompt_tokens,
        // No write count: like the Chat Completions endpoints, the Responses
        // prompt cache is automatic and bills no separate cache write, so
        // zero is the only honest reading here.
        cache: CacheTokens {
            read: state.cached_prompt_tokens,
            write: 0,
        },
    }
}

/// ChatGPT as a kernel plugin.
///
/// Transport and sign-in in one plugin, unlike xAI, because nothing outside
/// it reads the ChatGPT token store: no tool authenticates against
/// OpenAI's non-chat APIs with it, so the store has exactly one consumer
/// and belongs with it. The loopback redirect the browser comes back on
/// is shared with xAI's sign-in and stays in [`crate::llm::oauth_callback`].
///
/// A build without this feature has no `--login chatgpt` and no
/// `kind = \"chatgptoauth\"`, and says so at both.
///
/// `network` is declared because that is what this plugin does, even though
/// the capability set only gates the Lua host bridge today. A manifest that
/// under-declares is the failure mode worth avoiding: the grant prompt is
/// generated from it.
pub struct ChatGptPlugin {
    manifest: PluginManifest,
}

impl ChatGptPlugin {
    pub fn new() -> Self {
        Self {
            manifest: PluginManifest {
                name: "chatgpt".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                description: "ChatGPT subscription via account sign-in".to_string(),
                capabilities: vec![Capability::Network],
                optional_deps: Vec::new(),
                profiles: vec!["full".to_string(), "server".to_string()],
            },
        }
    }
}

impl Default for ChatGptPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for ChatGptPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn apply(&self, ctx: &mut Ctx) -> anyhow::Result<()> {
        ctx.provider(oauth::descriptor())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolSpec;

    fn request(messages: Vec<ChatMessage>, tools: Vec<ToolSpec>) -> ChatRequest {
        ChatRequest {
            model: "gpt-5.2".to_string(),
            messages,
            tools,
            stream: true,
            options: None,
        }
    }

    fn provider() -> ChatgptProvider {
        // No tokens needed to test request translation.
        ChatgptProvider {
            http: reqwest::Client::new(),
            base_url: oauth::BASE_URL.to_string(),
            model: "gpt-5.2".to_string(),
            tokens: Arc::new(ChatgptTokens {
                path: PathBuf::from("/nonexistent"),
                cache: Mutex::new(TokenCache::default()),
            }),
            session_id: "test".to_string(),
        }
    }

    #[test]
    fn translates_messages_to_responses_input() {
        let mut assistant = ChatMessage::assistant("Reading it.");
        assistant.push_tool_call(ToolCall::new("read_file", json!({ "path": "src/main.rs" })));
        let call_id = assistant.tool_calls()[0].id.clone();
        let body = provider().build_request_body(&request(
            vec![
                ChatMessage::system("You are Wizard."),
                ChatMessage::user("read it"),
                assistant,
                ChatMessage::tool_result(&call_id, "read_file", "fn main() {}"),
            ],
            vec![ToolSpec::function(
                "read_file",
                "Read a file.",
                json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
            )],
        ));

        assert_eq!(body["model"], "gpt-5.2");
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert_eq!(body["instructions"], "You are Wizard.");

        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");

        // assistant text, then its function_call
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["name"], "read_file");
        assert_eq!(
            input[2]["call_id"], call_id,
            "the provider's own id, verbatim"
        );
        // arguments are a JSON *string*
        assert_eq!(input[2]["arguments"], "{\"path\":\"src/main.rs\"}");

        // the tool result, correlated back to the call by id
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], call_id);
        assert_eq!(input[3]["output"], "fn main() {}");

        // tools use the flat Responses shape
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert_eq!(body["tools"][0]["parameters"]["type"], "object");
    }

    /// A two-call batch: `parallel_tool_calls` is on in every request this
    /// client sends, so this is the ordinary case, and there was no test for
    /// it. Each result becomes its own `function_call_output` item, all of
    /// them consecutive, each bound to its call by the id the API issued.
    #[test]
    fn a_parallel_batch_becomes_consecutive_function_call_outputs() {
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

        let (_instructions, input) = build_input(&[assistant, results]);
        assert_eq!(input.len(), 4, "two calls, then one output per result");
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[0]["call_id"], ids[0]);
        assert_eq!(input[1]["call_id"], ids[1]);
        // Both calls name the same tool: only the ids tell them apart.
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["call_id"], ids[0]);
        assert_eq!(input[2]["output"], "contents of a");
        assert_eq!(input[3]["call_id"], ids[1]);
        assert_eq!(input[3]["output"], "contents of b");
    }

    /// The same two-call batch as above, but starting from the wire rather
    /// than from hand-built messages: a recorded Responses SSE frame set with
    /// two `function_call` items for the *same* tool, decoded and then replayed.
    ///
    /// This is the half [`a_parallel_batch_becomes_consecutive_function_call_outputs`]
    /// cannot see. Two calls to one tool are indistinguishable by name, so if
    /// the decoder drops or reuses `call_id` the batch is unanswerable, and
    /// the replay above would still pass because it was handed ids that a test
    /// invented. Here every id in the request comes from the stream.
    #[tokio::test]
    async fn a_two_call_batch_from_the_wire_keeps_both_ids_apart() {
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(b"data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"encrypted_content\":\"gAAAAAB-opaque\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"read both\"}]}}\n\n".to_vec()),
            Ok(b"data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\"}\",\"call_id\":\"call_a\"}}\n\n".to_vec()),
            Ok(b"data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"b.rs\\\"}\",\"call_id\":\"call_b\"}}\n\n".to_vec()),
            Ok(b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":12,\"output_tokens\":8}}}\n\n".to_vec()),
        ];
        let mut out = decode_sse(stream::iter(parts).boxed());
        let mut assistant = None;
        while let Some(chunk) = out.next().await {
            let chunk = chunk.expect("ok");
            if chunk.done {
                assistant = chunk.message;
            }
        }
        let assistant = assistant.expect("the final chunk carries the turn");
        let calls = assistant.tool_calls();
        assert_eq!(calls.len(), 2, "both calls survived the stream");
        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[1].id, "call_b");
        assert_eq!(calls[0].function.arguments["path"], "a.rs");
        assert_eq!(calls[1].function.arguments["path"], "b.rs");

        // The batch is answered on one `tool` message, the shape the agent
        // loop accumulates (see `agent::turn`), and replayed.
        let mut results = ChatMessage::tool_result("call_a", "read_file", "contents of a");
        results.push_tool_result("call_b", "read_file", "contents of b");
        let (_instructions, input) = build_input(&[assistant, results]);

        assert_eq!(
            input.len(),
            5,
            "reasoning, two calls, two outputs: {input:?}"
        );
        assert_eq!(input[0]["type"], "reasoning");
        assert_eq!(input[0]["encrypted_content"], "gAAAAAB-opaque");
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "call_a");
        assert_eq!(input[2]["call_id"], "call_b");
        // Nothing is interleaved between the two outputs, and each is bound to
        // its own call by an id neither this test nor the adapter invented.
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "call_a");
        assert_eq!(input[3]["output"], "contents of a");
        assert_eq!(input[4]["type"], "function_call_output");
        assert_eq!(input[4]["call_id"], "call_b");
        assert_eq!(input[4]["output"], "contents of b");
    }

    #[test]
    fn user_images_become_input_image_parts() {
        let body = provider().build_request_body(&request(
            vec![ChatMessage::user_with_images(
                "look",
                vec![crate::llm::Image::new("QUJD", "image/png")],
            )],
            Vec::new(),
        ));
        let content = &body["input"][0]["content"];
        assert_eq!(content[0]["type"], "input_text");
        assert_eq!(content[0]["text"], "look");
        assert_eq!(content[1]["type"], "input_image");
        assert_eq!(content[1]["image_url"], "data:image/png;base64,QUJD");
    }

    #[test]
    fn reasoning_effort_rides_in_the_reasoning_field() {
        let mut with_effort = request(vec![ChatMessage::user("hi")], Vec::new());
        with_effort.options = Some(crate::llm::ChatOptions {
            temperature: None,
            num_ctx: None,
            reasoning_effort: Some("high".to_string()),
        });
        let body = provider().build_request_body(&with_effort);
        assert_eq!(body["reasoning"]["effort"], "high");

        let without =
            provider().build_request_body(&request(vec![ChatMessage::user("hi")], Vec::new()));
        assert!(without.get("reasoning").is_none());
    }

    /// A result whose call is not in the slice (a compacted history, a
    /// resumed session) still goes back bound to the id it recorded: nothing
    /// here re-derives a `call_id` from the tool name or from position.
    #[test]
    fn a_tool_result_carries_its_own_call_id_with_no_call_in_sight() {
        let (_instructions, input) = build_input(&[ChatMessage::tool_result(
            "call_read_file",
            "read_file",
            "out",
        )]);
        assert_eq!(input[0]["type"], "function_call_output");
        assert_eq!(input[0]["call_id"], "call_read_file");
    }

    /// The reasoning round trip, end to end: what `decode_sse` pulls off the
    /// stream is what `build_input` hands back on the next step.
    ///
    /// This client sends `store: false`, so nothing is remembered server-side.
    /// Before the `reasoning` item existed here, the encrypted payload the
    /// request paid to `include` was decoded and dropped, and every step after
    /// the first made the model re-derive its whole chain of thought: wrong
    /// answers on long turns, and the same reasoning billed once per step.
    #[tokio::test]
    async fn reasoning_is_replayed_on_the_next_step_rather_than_regenerated() {
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(b"data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"read it first\"}\n\n".to_vec()),
            Ok(b"data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"encrypted_content\":\"gAAAAAB-opaque\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"read it first\"}]}}\n\n".to_vec()),
            Ok(b"data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\"}\",\"call_id\":\"call_1\"}}\n\n".to_vec()),
            Ok(b"data: {\"type\":\"response.completed\",\"response\":{}}\n\n".to_vec()),
        ];
        let mut out = decode_sse(stream::iter(parts).boxed());
        let mut assistant = None;
        while let Some(chunk) = out.next().await {
            let chunk = chunk.expect("ok");
            if chunk.done {
                assistant = chunk.message;
            }
        }
        let assistant = assistant.expect("the final chunk carries the turn");

        // Step two of the same turn: the assistant turn goes back with the
        // answer to the call it made.
        let (_instructions, input) = build_input(&[
            ChatMessage::user("read it"),
            assistant,
            ChatMessage::tool_result("call_1", "read_file", "fn main() {}"),
        ]);

        assert_eq!(input.len(), 4, "user, reasoning, call, output");
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(
            input[1]["encrypted_content"], "gAAAAAB-opaque",
            "the encrypted reasoning goes back verbatim"
        );
        assert_eq!(input[1]["id"], "rs_1", "and so does the item id");
        assert_eq!(input[1]["summary"][0]["type"], "summary_text");
        assert_eq!(input[1]["summary"][0]["text"], "read it first");
        // Order matters: the item a reasoning block produced has to follow it.
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "call_1");
    }

    /// A thinking block with no encrypted payload is a summary with no
    /// reasoning behind it (an Anthropic-signed block from a session that
    /// changed providers, say). Replaying it would spend context on prose the
    /// endpoint cannot decrypt and the model cannot resume from.
    #[test]
    fn a_thinking_block_with_no_encrypted_payload_is_not_replayed() {
        let mut assistant = ChatMessage::new(
            Role::Assistant,
            vec![crate::llm::ContentBlock::thinking(
                "mused about it",
                Some("anthropic-signature".to_string()),
            )],
        );
        assistant.push_tool_call(ToolCall::new("read_file", json!({ "path": "a" })));

        let (_instructions, input) = build_input(&[assistant]);
        assert_eq!(input.len(), 1, "only the call survives");
        assert_eq!(input[0]["type"], "function_call");
    }

    /// The API rejects a `reasoning` item that is not followed by the item it
    /// produced, so a turn that said nothing at all replays nothing at all.
    #[test]
    fn reasoning_with_nothing_after_it_is_left_out_entirely() {
        let assistant = ChatMessage::new(
            Role::Assistant,
            vec![ContentBlock::Thinking(ThinkingBlock {
                thinking: String::new(),
                signature: Some("rs_1".to_string()),
                data: Some("gAAAAAB-opaque".to_string()),
            })],
        );
        let (_instructions, input) = build_input(&[assistant]);
        assert!(
            input.is_empty(),
            "an orphaned reasoning item is a 400, not a saving: {input:?}"
        );
    }

    /// `response.incomplete` is the only place this API says a reply stopped
    /// at a limit, and it has to reach the agent as the finish reason: the
    /// half-written `arguments` string of the call that was in flight decodes
    /// to nothing usable, and dispatching that runs a different action.
    #[tokio::test]
    async fn an_output_token_cutoff_is_reported_as_the_finish_reason() {
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(b"data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"name\":\"execute\",\"arguments\":\"{\\\"command\\\": \\\"rm -rf \",\"call_id\":\"call_1\"}}\n\n".to_vec()),
            Ok(b"data: {\"type\":\"response.incomplete\",\"response\":{\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\"usage\":{\"input_tokens\":9,\"output_tokens\":4}}}\n\n".to_vec()),
        ];
        let mut out = decode_sse(stream::iter(parts).boxed());
        let mut final_chunk = None;
        while let Some(chunk) = out.next().await {
            let chunk = chunk.expect("ok");
            if chunk.done {
                final_chunk = Some(chunk);
            }
        }
        let final_chunk = final_chunk.expect("a final chunk");
        assert_eq!(
            final_chunk.done_reason.as_deref(),
            Some("max_output_tokens")
        );
        assert!(
            crate::llm::is_length_cutoff(final_chunk.done_reason.as_deref().expect("a reason")),
            "the agent's refusal keys off this predicate"
        );
        // Usage still lands, so the tokens the cut-off attempt cost are billed.
        assert_eq!(final_chunk.prompt_eval_count, Some(9));
        // And this is the call that must never be dispatched: the arguments
        // did not survive.
        let calls = final_chunk.message.expect("message").take_tool_calls();
        assert_eq!(calls[0].function.arguments, Value::Null);
    }

    /// A reply that ended normally still reports `stop`, so the cutoff check
    /// stays a cutoff check.
    #[tokio::test]
    async fn a_completed_response_still_reports_stop() {
        let parts: Vec<Result<Vec<u8>>> = vec![Ok(
            b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n".to_vec(),
        )];
        let mut out = decode_sse(stream::iter(parts).boxed());
        let final_chunk = out.next().await.expect("final").expect("ok");
        assert_eq!(final_chunk.done_reason.as_deref(), Some("stop"));
    }

    /// The cached share of the prompt has to leave the adapter, because this
    /// client sends `store: false` and re-sends the whole conversation on
    /// every step of every turn. That re-send is the cached prefix, so on a
    /// multi-step turn most of the input is a cache read — and billing it as
    /// fresh input is the difference between a plausible cost column and one
    /// several times too large.
    #[tokio::test]
    async fn the_cached_share_of_the_prompt_leaves_the_adapter() {
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n".to_vec()),
            Ok(b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":48000,\"output_tokens\":120,\"input_tokens_details\":{\"cached_tokens\":46080}}}}\n\n".to_vec()),
        ];
        let mut out = decode_sse(stream::iter(parts).boxed());
        let mut last = None;
        while let Some(chunk) = out.next().await {
            last = Some(chunk.expect("chunk decodes"));
        }
        let last = last.expect("a final chunk");
        assert_eq!(
            last.prompt_eval_count,
            Some(48_000),
            "the Responses API counts cached tokens inside input_tokens, so \
             the prompt size is the reported number and nothing is summed"
        );
        assert_eq!(
            last.cache,
            CacheTokens {
                read: 46_080,
                write: 0
            }
        );
    }

    /// A stream that reports no `input_tokens_details` — an older API
    /// version, a proxy — prices as all-fresh rather than guessing.
    #[tokio::test]
    async fn a_response_without_a_cache_breakdown_prices_as_all_fresh() {
        let parts: Vec<Result<Vec<u8>>> = vec![Ok(
            b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":11,\"output_tokens\":3}}}\n\n"
                .to_vec(),
        )];
        let mut out = decode_sse(stream::iter(parts).boxed());
        let mut last = None;
        while let Some(chunk) = out.next().await {
            last = Some(chunk.expect("chunk decodes"));
        }
        assert_eq!(last.expect("a final chunk").cache, CacheTokens::NONE);
    }

    #[tokio::test]
    async fn decodes_text_and_a_tool_call() {
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello \"}\n\n".to_vec()),
            Ok(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"world\"}\n\n".to_vec()),
            Ok(b"data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\"}\",\"call_id\":\"call_1\"}}\n\n".to_vec()),
            Ok(b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":11,\"output_tokens\":3}}}\n\n".to_vec()),
        ];
        let bytes = stream::iter(parts).boxed();
        let mut out = decode_sse(bytes);

        let mut text = String::new();
        let mut final_chunk = None;
        while let Some(chunk) = out.next().await {
            let chunk = chunk.unwrap();
            if chunk.done {
                final_chunk = Some(chunk);
            } else if let Some(msg) = &chunk.message {
                text.push_str(&msg.text());
            }
        }
        assert_eq!(text, "Hello world");
        let final_chunk = final_chunk.expect("a final chunk");
        assert_eq!(final_chunk.prompt_eval_count, Some(11));
        assert_eq!(final_chunk.eval_count, Some(3));
        let calls = final_chunk.message.unwrap().take_tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "read_file");
        assert_eq!(
            calls[0].id, "call_1",
            "the call_id the stream carried, not one we invented"
        );
        assert_eq!(calls[0].function.arguments["path"], "a.rs");
    }

    #[tokio::test]
    async fn reasoning_deltas_are_flagged_thinking() {
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(
                b"data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"hmm\"}\n\n"
                    .to_vec(),
            ),
            Ok(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\n".to_vec()),
            Ok(b"data: {\"type\":\"response.completed\",\"response\":{}}\n\n".to_vec()),
        ];
        let mut out = decode_sse(stream::iter(parts).boxed());

        let first = out.next().await.expect("reasoning").expect("ok");
        assert!(first.thinking, "reasoning delta is flagged");
        assert_eq!(first.message.expect("message").text(), "hmm");
        let second = out.next().await.expect("text").expect("ok");
        assert!(!second.thinking, "visible text is not flagged");
        assert!(out.next().await.expect("final").expect("ok").done);
        assert!(out.next().await.is_none());
    }

    #[tokio::test]
    async fn split_frames_and_a_trailing_line_without_a_newline_still_decode() {
        // A frame split mid-JSON and a last event with no trailing newline:
        // the decoder must reassemble across the read boundary and flush what
        // the peer never terminated. Unparseable function_call arguments
        // become null rather than failing the stream.
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(b"data: {\"type\":\"response.outpu".to_vec()),
            Ok(b"t_text.delta\",\"delta\":\"Hi\"}\n\n".to_vec()),
            Ok(
                b"data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"name\":\"go\",\"arguments\":\"not json\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{}}"
                    .to_vec(),
            ),
        ];
        let mut out = decode_sse(stream::iter(parts).boxed());

        let first = out.next().await.expect("text").expect("ok");
        assert_eq!(first.message.expect("message").text(), "Hi");
        let last = out.next().await.expect("final").expect("ok");
        assert!(last.done);
        let calls = last.message.expect("message").take_tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "go");
        assert_eq!(calls[0].function.arguments, Value::Null);
        assert!(out.next().await.is_none());
    }

    /// A stream that stops before the endpoint says the response is over is a
    /// failure, not a short answer.
    ///
    /// It used to be flushed into a clean `done: true` carrying whatever had
    /// arrived — a well-formed completion the agent ends the turn on, with no
    /// error anywhere. That is what "it randomly stops" looks like from the
    /// user's side, so the cut has to be typed and transient instead.
    #[tokio::test]
    async fn a_stream_cut_before_response_completed_is_a_transient_failure() {
        let parts: Vec<Result<Vec<u8>>> = vec![Ok(
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"half a sen\"}\n\n"
                .to_vec(),
        )];
        let mut out = decode_sse(stream::iter(parts).boxed());

        let first = out.next().await.expect("text").expect("ok");
        assert_eq!(first.message.expect("message").text(), "half a sen");

        let err = out
            .next()
            .await
            .expect("an item")
            .expect_err("a cut stream is not a completed reply");
        let provider = err
            .downcast_ref::<ProviderError>()
            .expect("typed, or the ladder cannot classify it");
        assert_eq!(provider.status, None);
        assert!(provider.is_transient());
    }

    #[tokio::test]
    async fn malformed_and_unknown_events_are_skipped() {
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(b"data: {broken json\n\n".to_vec()),
            Ok(b"data: {\"type\":\"response.created\"}\n\n".to_vec()),
            Ok(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n".to_vec()),
            Ok(b"data: [DONE]\n\n".to_vec()),
        ];
        let mut out = decode_sse(stream::iter(parts).boxed());

        let first = out.next().await.expect("text").expect("ok");
        assert_eq!(first.message.expect("message").text(), "ok");
        assert!(out.next().await.expect("final").expect("ok").done);
        assert!(out.next().await.is_none());
    }

    #[tokio::test]
    async fn a_failed_response_becomes_an_error() {
        let parts: Vec<Result<Vec<u8>>> = vec![Ok(
            b"data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"rate_limit_exceeded\",\"message\":\"slow down\"}}}\n\n".to_vec(),
        )];
        let bytes = stream::iter(parts).boxed();
        let mut out = decode_sse(bytes);
        let mut saw_error = false;
        while let Some(chunk) = out.next().await {
            if let Err(err) = chunk {
                saw_error = true;
                assert!(format!("{err:#}").contains("slow down"));
            }
        }
        assert!(saw_error);
    }
}
