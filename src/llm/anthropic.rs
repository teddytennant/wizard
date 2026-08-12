//! Streaming HTTP client for the Anthropic **Messages** API
//! (`POST {base_url}/v1/messages`).
//!
//! Thin `reqwest` wrapper with manual SSE parsing. Wizard's native
//! [`ChatRequest`] is translated to the Messages request shape (block-array
//! `system`, content-block `messages`, `thinking` / `tool_use` / `tool_result`
//! blocks) and the SSE event stream is decoded back into Wizard's [`ChatChunk`]
//! stream.
//!
//! # Prompt caching
//!
//! Anthropic prices a cache *read* at 0.1x input against a 1.25x *write*, and
//! Wizard's signature modes are the worst possible case for having no caching:
//! `/ultra` fans N subagents that each re-send the full uncached prefix, and
//! `--continuous` re-sends charter plus skills plus memory plus tool schemas
//! every step forever. So this adapter marks up to four `cache_control`
//! breakpoints (the API's hard cap), at the boundaries the render order
//! (`tools` -> `system` -> `messages`) makes stable: the tool-schema tail, the
//! system tail, and the last *two* stable history messages (see
//! [`build_messages`]).
//!
//! Caching is a *prefix* match, and a read only happens where a breakpoint in
//! *this* request sits at the end of a byte-identical prefix some earlier
//! request already wrote. Two rules here exist to make that hold from one
//! agent step to the next, and neither is cosmetic. Getting either wrong does
//! not merely fail to cache: it writes an entry nothing will ever read, which
//! pays the 1.25x premium for nothing and is strictly worse than sending no
//! breakpoint at all.
//!
//! * only the *leading* run of [`Role::System`] messages is hoisted into the
//!   top-level `system` blocks. A system message that arrives mid-conversation
//!   (a background note, a subagent report) stays where it is, as a user turn.
//! * a breakpoint is only ever placed on an assistant or tool-result message.
//!   Those are the only two the agent never retracts, and being retracted is
//!   the whole problem: `turn.rs` pushes the context-pressure signal as a
//!   trailing *user* message carrying a live token count, sends the request,
//!   and pops it again. A breakpoint on that note would write a prefix ending
//!   in a number that never appears again, so every step would write and no
//!   step would ever read. Anchoring behind the trailing run of user turns
//!   costs one message worth of coverage, which the next step's breakpoint
//!   picks up anyway.
//!
//! The second (older) history breakpoint is what makes the read survive a
//! parallel tool batch. A read walks back at most 20 content blocks from a
//! breakpoint looking for an entry, and one agent step appends an assistant
//! turn of `1 + calls` blocks *plus* a result message of `calls` blocks. With
//! only the newest anchor marked, the previous step's entry is `1 + 2 * calls`
//! blocks away and drops out of reach past nine parallel calls; with the
//! assistant turn marked as well the nearest breakpoint is `1 + calls` blocks
//! away, which holds to nineteen. That is the placement Anthropic documents
//! for long turns: put a marker on a block within 20 of the previous turn's
//! last cached block.
//!
//! The four are not all kept for the same length of time. The preamble is
//! written once a session and read for the rest of it, so it takes the
//! one-hour TTL and survives the pause that would otherwise cost a cold
//! write; the two history breakpoints are rewritten every step and stay on
//! the five-minute default. [`CacheTtl`] has the arithmetic.

use std::collections::BTreeMap;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use futures_util::{Stream, StreamExt, stream};
use serde::Deserialize;
use serde_json::{Value, json};

use super::provider::LlmProvider;
use super::{
    CacheTokens, ChatChunk, ChatMessage, ChatOptions, ChatRequest, ChatStream, ContentBlock,
    FunctionCall, ProviderError, Role, ThinkingBlock, ToolCall,
};

/// Anthropic API version pinned in the `anthropic-version` header.
const API_VERSION: &str = "2023-06-01";
/// Static fallback model list when `GET /v1/models` is unavailable.
const FALLBACK_MODELS: &[&str] = &[
    "claude-fable-5",
    "claude-opus-4-8",
    "claude-sonnet-5",
    "claude-haiku-4-5",
];

/// Client bound to one Anthropic-compatible endpoint.
#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    http: reqwest::Client,
    /// Base URL without the `/v1` suffix, e.g. `https://api.anthropic.com`.
    /// Trailing slashes are trimmed.
    base_url: String,
    /// Default model tag (used for [`LlmProvider::label`]).
    model: String,
    /// API key sent in the `x-api-key` header; empty surfaces a 401 at runtime.
    api_key: String,
}

impl AnthropicProvider {
    /// Build a client for `base_url` (defaults to `https://api.anthropic.com`).
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        // Anthropic's own API is hosted, but this client is also how a local
        // Messages-compatible proxy is reached, so the locality comes from
        // the configured address rather than from the provider kind.
        let http = crate::llm::chat_http_builder(crate::llm::client_read_timeout_for(&base_url))
            .build()
            .unwrap_or_default();
        Self {
            http,
            base_url,
            model: model.into(),
            api_key: api_key.into(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// Typed error for a transport-level failure reaching the API.
    fn transport_error(&self, source: reqwest::Error) -> anyhow::Error {
        let message = format!("cannot reach {}: {source}", self.base_url);
        anyhow::Error::new(source).context(ProviderError::transport(message))
    }

    /// Typed error for a non-success HTTP response, body included.
    ///
    /// The `Retry-After` is read off the headers before `text()` consumes the
    /// response: Anthropic sends one on 429 and on the 529 overload status,
    /// and it is what keeps the agent's backoff from guessing.
    async fn status_error(&self, response: reqwest::Response) -> anyhow::Error {
        let status = response.status();
        let retry_after = crate::llm::retry_after_from_headers(response.headers());
        let body = response.text().await.unwrap_or_default();
        crate::llm::http_error_with_retry_after(
            status.as_u16(),
            format!("{} returned HTTP {status}: {body}", self.base_url),
            retry_after,
        )
    }

    /// Attach the Anthropic auth + version headers.
    fn headers(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        builder
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
    }

    /// Translate a native [`ChatRequest`] into a Messages API request body.
    fn build_request_body(&self, request: &ChatRequest) -> Value {
        let (system, mut messages) = build_messages(&request.messages);
        let max_tokens = crate::llm::anthropic_max_output_tokens(&request.model);
        let reasoning = reasoning_config(&request.model, request.options.as_ref(), max_tokens);
        // A replayed `thinking` block is only legal on a request that has
        // thinking switched on. History outlives a `/model` switch, so a turn
        // taken on a reasoning model can be replayed to one that takes no
        // `thinking` parameter at all; the blocks come back out here rather
        // than 400-ing the turn.
        if reasoning.is_none() {
            strip_thinking_blocks(&mut messages);
        }
        // `max_tokens` is required on every Messages request and has no
        // implicit cap, and a value above the *requested model's* own ceiling
        // is a 400, which is permanent, so the turn dies with no retry. One
        // number cannot serve a fleet spanning model generations, so the
        // ceiling is looked up per request from the shared table.
        let mut body = json!({
            "model": request.model,
            "max_tokens": max_tokens,
            "messages": messages,
            "stream": true,
        });
        if !system.is_empty() {
            body["system"] = Value::Array(system);
        }
        let mut tools: Vec<Value> = request
            .tools
            .iter()
            .map(|spec| {
                json!({
                    "name": spec.function.name,
                    "description": spec.function.description,
                    "input_schema": spec.function.parameters,
                })
            })
            .collect();
        // Tools render first, so the breakpoint on the tail of the schema
        // list is the one that survives a system-prompt edit (a `/mode`
        // switch, a charter reload): the schemas stay cached even when
        // everything after them has to be written again.
        if let Some(last) = tools.last_mut() {
            last["cache_control"] = cache_control(CacheTtl::Hour);
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
        match reasoning {
            Some(reasoning) => {
                body["thinking"] = reasoning.thinking;
                // `effort` lives inside `output_config`, not at the top level.
                if let Some(effort) = reasoning.effort {
                    body["output_config"] = json!({ "effort": effort });
                }
                // Sampling parameters are removed on every model that takes
                // adaptive thinking (they are a 400 on Opus 4.7 and newer),
                // and extended thinking on the older line requires the
                // default temperature. Either way `temperature` cannot ride
                // along with a `thinking` block, so it is dropped rather than
                // sent and rejected.
            }
            None => {
                if let Some(options) = &request.options
                    && let Some(temperature) = options.temperature
                {
                    body["temperature"] = json!(temperature);
                }
            }
        }
        body
    }
}

/// How long one breakpoint's entry lives. The choice is per breakpoint
/// because the two kinds of prefix Wizard caches have opposite economics.
///
/// A read costs 0.1x input either way. A write costs 1.25x at five minutes
/// and 2x at an hour, so the five-minute entry pays for itself on the second
/// request and the one-hour entry needs a third. What decides it is not how
/// often the entry is read but how often it is *rewritten*:
///
/// * The preamble (tool schemas, then the system prompt) is byte-identical
///   from the first step of a session to the last. It is written once and
///   read on every request after that, so the only thing that ever ends it is
///   the clock — and a user who reads a diff, takes a call, or thinks for six
///   minutes drops a five-minute entry and pays a full cold write to get it
///   back. That is the case the extra 0.75x buys out, and the preamble is
///   also the largest of the four prefixes, so it is the one where a cold
///   write hurts most.
/// * The two history breakpoints move every turn by construction: the newest
///   anchor is a message further along each step, so each request writes a
///   *new* entry and the old one is dead the moment the next step starts. An
///   hour of TTL on something with a life measured in seconds is 2x for
///   nothing, and 20-odd such writes per turn is not a rounding error.
///
/// So: [`CacheTtl::Hour`] on the preamble, [`CacheTtl::Minutes`] on the
/// history. No beta header is needed for either; `ttl` is a plain field on
/// `cache_control`, and omitting it means five minutes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CacheTtl {
    /// The API default, five minutes. Written at 1.25x.
    Minutes,
    /// One hour. Written at 2x.
    Hour,
}

/// One `cache_control` breakpoint at the given TTL. See [`CacheTtl`].
fn cache_control(ttl: CacheTtl) -> Value {
    match ttl {
        // Sent bare rather than as `"ttl": "5m"`: it is the default, and the
        // shorter form is one less field on every request.
        CacheTtl::Minutes => json!({ "type": "ephemeral" }),
        CacheTtl::Hour => json!({ "type": "ephemeral", "ttl": "1h" }),
    }
}

/// How a model takes its thinking configuration. Substring-matched on the
/// lowercased tag, most specific first, exactly like
/// [`crate::llm::anthropic_max_output_tokens`], so vendor-prefixed tags
/// (`anthropic.claude-opus-5`) and dated snapshots resolve to the same entry
/// as the bare alias.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThinkingSupport {
    /// No `thinking` parameter at all: the Claude 3 and 3.5 generations, and
    /// any tag this table does not recognize. Sending one would be a 400, so
    /// an unknown model gets a request that reasons less than it could rather
    /// than one that fails outright.
    None,
    /// `{"type": "enabled", "budget_tokens": N}`: Claude 3.7 through the 4.5
    /// line. `budget_tokens` must be at least 1024 and strictly below
    /// `max_tokens`.
    Budget,
    /// `{"type": "adaptive"}` plus `output_config.effort`: the 4.6 line and
    /// newer.
    ///
    /// `since_4_7` splits that line in two. The 4.7 release moved the request
    /// surface in two places at once, and both halves are a 400 when sent to
    /// the 4.6 pair, so one flag decides both:
    ///
    /// * `thinking.display` arrived with 4.7, and its default there is
    ///   `"omitted"`. Without an explicit `"summarized"` those models still
    ///   stream `thinking` blocks, but every one of them is empty, so Wizard's
    ///   reasoning pane would show a long pause and nothing else. On 4.6 the
    ///   field does not exist at all, and an unknown field is a 400.
    /// * the `xhigh` effort level arrived with 4.7 too, between `high` and
    ///   `max`. Sent to Opus 4.6 or Sonnet 4.6 it is an invalid enum value,
    ///   which is a 400, which is permanent, so the turn dies with no retry.
    Adaptive { since_4_7: bool },
}

fn thinking_support(model: &str) -> ThinkingSupport {
    let model = model.to_ascii_lowercase();
    // The 4.7-and-newer line: reasoning summaries are suppressed by default,
    // and `xhigh` is a level this model knows.
    if model.contains("fable")
        || model.contains("mythos")
        || model.contains("opus-5")
        || model.contains("opus-4-8")
        || model.contains("opus-4-7")
        || model.contains("sonnet-5")
    {
        return ThinkingSupport::Adaptive { since_4_7: true };
    }
    // The 4.6 pair: summaries are already on by default, and neither
    // `thinking.display` nor the `xhigh` effort level exists yet.
    if model.contains("opus-4-6") || model.contains("sonnet-4-6") {
        return ThinkingSupport::Adaptive { since_4_7: false };
    }
    // The fixed-budget line. `sonnet-4-6` is matched above, so the bare
    // `sonnet-4` arm below cannot swallow it.
    if model.contains("opus-4-5")
        || model.contains("sonnet-4-5")
        || model.contains("haiku-4-5")
        || model.contains("sonnet-4")
        || model.contains("opus-4")
        || model.contains("3-7-sonnet")
    {
        return ThinkingSupport::Budget;
    }
    ThinkingSupport::None
}

/// The reasoning half of a request body: the `thinking` block, and the
/// `output_config.effort` level when the model takes one.
struct Reasoning {
    thinking: Value,
    effort: Option<String>,
}

/// Translate `/effort` into whatever this model's generation calls it.
///
/// Returns `None` when nothing should be sent: a model with no `thinking`
/// parameter, or a fixed-budget model with no configured effort (there is no
/// budget to pick, and enabling thinking uninvited would change both the cost
/// and the shape of every reply).
///
/// An adaptive model always gets `thinking` even with no configured effort:
/// that is the mode Anthropic recommends for the whole 4.6-and-newer line,
/// and it is what makes the reasoning stream Wizard already decodes actually
/// arrive. `/effort` then rides on top as `output_config.effort`.
fn reasoning_config(
    model: &str,
    options: Option<&ChatOptions>,
    max_tokens: u32,
) -> Option<Reasoning> {
    let effort = options
        .and_then(|options| options.reasoning_effort.as_deref())
        .filter(|effort| {
            // Anything else is a 400. `ChatOptions::reasoning_effort` is a
            // free-form string (it is shared with the OpenAI-compatible
            // adapters), so an unrecognized level is dropped rather than
            // forwarded.
            matches!(*effort, "low" | "medium" | "high" | "xhigh" | "max")
        });
    match thinking_support(model) {
        ThinkingSupport::None => None,
        ThinkingSupport::Adaptive { since_4_7 } => {
            let thinking = if since_4_7 {
                json!({ "type": "adaptive", "display": "summarized" })
            } else {
                json!({ "type": "adaptive" })
            };
            Some(Reasoning {
                thinking,
                // Second pass over the level, now that the model is known:
                // `xhigh` arrived with 4.7 and is an invalid enum value on the
                // 4.6 pair. Dropping it lands the request on the API's own
                // default, which is `high`, the next level down, rather than
                // on a 400 that kills the turn.
                effort: effort
                    .filter(|level| since_4_7 || *level != "xhigh")
                    .map(str::to_string),
            })
        }
        ThinkingSupport::Budget => effort.map(|effort| Reasoning {
            thinking: json!({
                "type": "enabled",
                "budget_tokens": thinking_budget(effort, max_tokens),
            }),
            effort: None,
        }),
    }
}

/// Fixed thinking budget for one effort level, clamped into the range the API
/// accepts: at least 1024 tokens, and strictly below `max_tokens` (the budget
/// is spent *out of* the output allowance, so a budget at or above it is a
/// 400). The headroom keeps room for an answer after the reasoning.
fn thinking_budget(effort: &str, max_tokens: u32) -> u32 {
    let want = match effort {
        "low" => 4_096,
        "medium" => 16_384,
        _ => 32_768,
    };
    want.clamp(1_024, max_tokens.saturating_sub(1_024).max(1_024))
}

/// Drop every `thinking` / `redacted_thinking` block from an already-built
/// message list, leaving an empty text block behind if that emptied a turn
/// (the API rejects a message with no content).
///
/// A `cache_control` marker the removed block was carrying moves to whatever
/// block ends up last. [`build_messages`] puts the breakpoint on the last
/// block of an anchor message, and an assistant turn that reasoned but said
/// nothing and called nothing has a reasoning block there, so dropping it
/// naively would silently delete a breakpoint on the way to a model that
/// cannot take `thinking` at all.
fn strip_thinking_blocks(messages: &mut [Value]) {
    for message in messages {
        let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        let mut breakpoint: Option<Value> = None;
        content.retain(|block| {
            if !matches!(
                block.get("type").and_then(Value::as_str),
                Some("thinking" | "redacted_thinking")
            ) {
                return true;
            }
            if let Some(marker) = block.get("cache_control") {
                breakpoint = Some(marker.clone());
            }
            false
        });
        if content.is_empty() {
            content.push(json!({ "type": "text", "text": "" }));
        }
        if let Some(marker) = breakpoint
            && let Some(block) = content.last_mut()
            && block.get("cache_control").is_none()
        {
            block["cache_control"] = marker;
        }
    }
}

/// The `input` a replayed tool call goes back out with.
///
/// Anthropic requires `tool_use.input` to be a JSON **object**, and one
/// upstream path does not guarantee it: [`build_final`] keeps a tool call
/// whose streamed arguments never parsed by degrading them to
/// `Value::String(raw)`, so the model can at least see what it tried to send.
/// Passing that straight through produced `"input": "not json"`, which is a
/// hard 400 from the Messages API — classified permanent, so the turn died
/// with no retry — and because the assistant message was already persisted,
/// it came back on every `/resume` of that session. `openai.rs` has handled
/// exactly this since its own `arguments` became a `Value`.
///
/// A string that really is an encoded object is decoded (a provider or an
/// imported transcript that double-encoded it). Anything else becomes `{}`:
/// the tool then rejects the call for missing arguments, which is an ordinary
/// tool error the model reads and retries, rather than an HTTP status that
/// ends the conversation.
fn tool_use_input(arguments: &Value) -> Value {
    if arguments.is_object() {
        return arguments.clone();
    }
    if let Value::String(raw) = arguments
        && let Ok(parsed) = serde_json::from_str::<Value>(raw)
        && parsed.is_object()
    {
        return parsed;
    }
    json!({})
}

/// Translate native messages into `(system, messages)`.
///
/// `system` comes back as a **block array**, not a string. The Messages API
/// takes either, but only the array form has anywhere to hang a
/// `cache_control` breakpoint, and the system prompt is the largest stable
/// prefix Wizard sends. An empty array means "no system prompt" and is left
/// off the body.
///
/// Only the *leading* run of [`Role::System`] messages becomes that prefix.
/// Wizard injects system messages mid-conversation too (background notes,
/// subagent reports), and hoisting those would splice per-request text into
/// the front of the cached prefix, invalidating it while still paying the
/// write premium: strictly worse than not caching at all. A mid-conversation
/// note is therefore emitted in place, as a user turn wrapped in
/// `<system-reminder>` (the shape Anthropic documents for models without a
/// mid-conversation `system` role, which is most of the fleet).
///
/// User and assistant turns become content-block messages: assistant
/// reasoning becomes `thinking` / `redacted_thinking` blocks *first* (the API
/// requires that order and rejects a replayed block whose signature is not
/// byte-identical), tool calls become `tool_use` blocks carrying the id the
/// provider itself issued, and a `tool`-role message becomes one user message
/// whose content is *all* of that message's `tool_result` blocks, each bound
/// to its call by `tool_use_id`.
///
/// The single-message rule is not a nicety. Anthropic requires every result
/// for one assistant turn to arrive in the message immediately following it,
/// so splitting a two-call batch across two user messages is an HTTP 400 and
/// the turn dies with no retry.
fn build_messages(messages: &[ChatMessage]) -> (Vec<Value>, Vec<Value>) {
    let mut system_parts: Vec<String> = Vec::new();
    let mut out: Vec<Value> = Vec::new();
    // Indices into `out` of the last two *anchors*: messages this session can
    // never retract, which is exactly the assistant turns and the tool-result
    // messages. A user turn is deliberately not an anchor. The agent appends
    // and then pops per-request user notes (the context-pressure signal, the
    // empty-completion nudge), and a breakpoint on one of those would write a
    // prefix ending in text that never appears in another request, so every
    // step would pay the write and no step would ever read.
    let mut previous_anchor: Option<usize> = None;
    let mut anchor: Option<usize> = None;

    for message in messages {
        match message.role {
            Role::System => {
                let text = message.text();
                if text.is_empty() {
                    continue;
                }
                if out.is_empty() {
                    system_parts.push(text);
                } else {
                    // Deliberately not an anchor: a mid-conversation note is
                    // the volatile tail the breakpoint has to stay in front of.
                    out.push(json!({
                        "role": "user",
                        "content": [{ "type": "text", "text": system_reminder(&text) }],
                    }));
                }
            }
            Role::User => {
                let text = message.text();
                let images = message.images();
                let mut content: Vec<Value> = Vec::new();
                // The API rejects an empty text block, so an image-only user
                // message carries no text part at all. A text-only message
                // still always has one, empty or not.
                if !text.is_empty() || images.is_empty() {
                    content.push(json!({ "type": "text", "text": text }));
                }
                // Images ride along as base64 `image` blocks after the text
                // (the Anthropic vision format), each with its own media type.
                for image in images {
                    content.push(image_block(image));
                }
                // Not an anchor: see `previous_anchor`. The turn the user just
                // typed is picked up by the next step's anchor anyway, once an
                // assistant turn has been appended behind it.
                out.push(json!({ "role": "user", "content": content }));
            }
            Role::Assistant => {
                let mut blocks: Vec<Value> = Vec::new();
                // Reasoning first: Anthropic accepts a replayed `thinking`
                // block only at the head of the assistant turn.
                for block in &message.content {
                    if let ContentBlock::Thinking(thinking) = block
                        && let Some(block) = thinking_block(thinking)
                    {
                        blocks.push(block);
                    }
                }
                // An assistant turn takes no image blocks here either: images
                // the model generated are named in its text instead.
                let text = super::assistant_content(message);
                if !text.is_empty() {
                    blocks.push(json!({ "type": "text", "text": text }));
                }
                for call in message.tool_calls() {
                    let input = tool_use_input(&call.function.arguments);
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call.id,
                        "name": call.function.name,
                        "input": input,
                    }));
                }
                if blocks.is_empty() {
                    blocks.push(json!({ "type": "text", "text": "" }));
                }
                previous_anchor = anchor;
                anchor = Some(out.len());
                out.push(json!({ "role": "assistant", "content": blocks }));
            }
            Role::Tool => {
                let content: Vec<Value> = message
                    .tool_results()
                    .into_iter()
                    .map(|result| {
                        json!({
                            "type": "tool_result",
                            "tool_use_id": result.tool_use_id,
                            "content": result.content,
                        })
                    })
                    .collect();
                previous_anchor = anchor;
                anchor = Some(out.len());
                out.push(json!({ "role": "user", "content": content }));
            }
        }
    }

    let mut system: Vec<Value> = system_parts
        .into_iter()
        .map(|text| json!({ "type": "text", "text": text }))
        .collect();
    // Breakpoint 2 of 4. `tools` render before `system`, so this one covers
    // both: the whole fixed preamble Wizard re-sends on every step. It is
    // written once a session and read from then on, which is what the
    // one-hour TTL is for (see [`CacheTtl`]).
    if let Some(last) = system.last_mut() {
        last["cache_control"] = cache_control(CacheTtl::Hour);
    }
    // Breakpoints 3 and 4 of 4: the conversation so far, minus the volatile
    // tail. The newest anchor writes an entry one turn longer than the last
    // one, so coverage grows with the session instead of decaying; the older
    // anchor halves the distance a lookback has to walk to find the previous
    // step's entry, which is what keeps a parallel tool batch from silently
    // pushing that entry outside the 20-block window.
    //
    // Both stay at the default five minutes: an entry written here is
    // superseded by the next step's, seconds later, so paying 2x to keep it
    // alive for an hour buys nothing (see [`CacheTtl`]).
    for index in [previous_anchor, anchor].into_iter().flatten() {
        if let Some(blocks) = out[index].get_mut("content").and_then(Value::as_array_mut)
            && let Some(block) = blocks.last_mut()
        {
            block["cache_control"] = cache_control(CacheTtl::Minutes);
        }
    }
    (system, out)
}

/// Wrap a mid-conversation system note for delivery on a user turn.
///
/// The tag is the convention Anthropic documents for operator text that is not
/// the system prompt, and it is what keeps the note legible as an instruction
/// rather than as something the user typed.
fn system_reminder(text: &str) -> String {
    format!("<system-reminder>\n{text}\n</system-reminder>")
}

/// One replayed reasoning block, or `None` when the block cannot legally be
/// replayed.
///
/// Anthropic verifies `thinking.signature` and rejects the whole request when
/// it does not match, so an unsigned block (one Wizard synthesized, or one
/// decoded from a proxy that dropped the signature) is dropped rather than
/// sent: losing the reasoning costs the model some continuity, sending it
/// costs the turn.
fn thinking_block(block: &ThinkingBlock) -> Option<Value> {
    // The redacted form carries an opaque payload instead of text and has no
    // separate signature; it is replayed verbatim.
    if let Some(data) = &block.data {
        return Some(json!({ "type": "redacted_thinking", "data": data }));
    }
    let signature = block.signature.as_ref()?;
    Some(json!({
        "type": "thinking",
        "thinking": block.thinking,
        "signature": signature,
    }))
}

/// One base64 `image` source block, the Anthropic vision shape.
fn image_block(image: &crate::llm::Image) -> Value {
    json!({
        "type": "image",
        "source": {
            "type": "base64",
            "media_type": image.mime,
            "data": image.b64,
        },
    })
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn health(&self) -> Result<()> {
        let response = self
            .headers(self.http.get(self.url("/v1/models")))
            .send()
            .await
            .map_err(|source| self.transport_error(source))?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(anyhow::Error::new(ProviderError::http(
                401,
                format!(
                    "{} rejected the API key (HTTP 401), check the configured API key env var",
                    self.base_url
                ),
            )));
        }
        if !response.status().is_success() {
            return Err(self.status_error(response).await);
        }
        Ok(())
    }

    async fn supports_native_tools(&self, _model: &str) -> Result<bool> {
        Ok(true)
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        let fallback = || FALLBACK_MODELS.iter().map(|m| m.to_string()).collect();
        let response = match self
            .headers(self.http.get(self.url("/v1/models")))
            .send()
            .await
        {
            Ok(response) => response,
            Err(err) => {
                tracing::warn!("listing Anthropic models failed: {err}; using fallback list");
                return Ok(fallback());
            }
        };
        if !response.status().is_success() {
            tracing::warn!(
                "Anthropic /v1/models returned {}; using fallback list",
                response.status()
            );
            return Ok(fallback());
        }
        match response.json::<ModelsResponse>().await {
            Ok(models) => Ok(models.data.into_iter().map(|m| m.id).collect()),
            Err(err) => {
                tracing::warn!("parsing Anthropic models failed: {err}; using fallback list");
                Ok(fallback())
            }
        }
    }

    async fn context_window(&self, model: &str) -> Option<u32> {
        context_window(model)
    }

    async fn chat_stream(&self, request: ChatRequest) -> Result<ChatStream> {
        let body = self.build_request_body(&request);
        let response = self
            .headers(self.http.post(self.url("/v1/messages")))
            .json(&body)
            .send()
            .await
            .map_err(|source| self.transport_error(source))?;
        if !response.status().is_success() {
            return Err(self.status_error(response).await);
        }
        let bytes = response
            .bytes_stream()
            .map(|item| match item {
                Ok(chunk) => Ok(chunk.to_vec()),
                Err(e) => Err(anyhow!(e).context(ProviderError::transport(
                    "Anthropic response stream was interrupted",
                ))),
            })
            .boxed();
        Ok(decode_sse(bytes))
    }

    fn label(&self) -> String {
        format!("anthropic:{}", self.model)
    }
}

/// Context-window table for Anthropic models. The current generation
/// (Fable 5 / Mythos 5, Opus 4.8, Sonnet 5) has a 1M window, as do older
/// variants flagged `1m` in the model name; every other `claude-*` model has
/// 200k. Unknown (non-claude) tags report `None`.
fn context_window(model: &str) -> Option<u32> {
    let model = model.to_ascii_lowercase();
    if !model.starts_with("claude") {
        return None;
    }
    if model.contains("1m")
        || model.contains("fable")
        || model.contains("mythos")
        || model.starts_with("claude-opus-4-8")
        || model.starts_with("claude-sonnet-5")
    {
        Some(1_000_000)
    } else {
        Some(200_000)
    }
}

/// `GET /v1/models` response (subset).
#[derive(Debug, Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    data: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
struct ModelEntry {
    id: String,
}

/// One SSE `data: {...}` event from the Messages stream (subset). The JSON's
/// own `type` field selects the variant.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum Event {
    #[serde(rename = "message_start")]
    MessageStart { message: MessageStartBody },
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        index: u64,
        content_block: BlockStart,
    },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta { index: u64, delta: BlockDelta },
    #[serde(rename = "message_delta")]
    MessageDelta {
        #[serde(default)]
        delta: Option<MessageDeltaBody>,
        #[serde(default)]
        usage: Option<Usage>,
    },
    #[serde(rename = "message_stop")]
    MessageStop,
    /// A failure the API reported *inside* an HTTP 200 stream.
    ///
    /// The status line goes out before the model runs, so a capacity problem
    /// that shows up two seconds into a generation cannot be an HTTP status
    /// any more; Anthropic documents `event: error` with an
    /// `overloaded_error` payload for exactly that, and it is the single most
    /// common way a Claude turn fails under load. Falling into
    /// [`Event::Other`] made it invisible: the stream then ran to EOF and the
    /// decoder synthesized a *successful* completion holding whatever text had
    /// arrived, which the agent has no reason to retry and every reason to
    /// treat as the model's final answer.
    #[serde(rename = "error")]
    Error {
        #[serde(default)]
        error: Option<StreamError>,
    },
    #[serde(other)]
    Other,
}

/// The payload of an in-stream [`Event::Error`].
#[derive(Debug, Deserialize)]
struct StreamError {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

impl StreamError {
    /// The HTTP status this in-band failure is classified as.
    ///
    /// Anthropic's error `type`s map onto the statuses the same conditions
    /// would have produced before the first byte, which is what keeps one
    /// condition classified one way whether it arrives early or late:
    /// `overloaded_error` is the 529 that a pre-stream overload returns,
    /// `rate_limit_error` the 429, and everything else is attributed to the
    /// server, because the request had already been accepted.
    fn status(&self) -> u16 {
        match self.kind.as_deref() {
            Some("overloaded_error") => 529,
            Some("rate_limit_error") => 429,
            Some("invalid_request_error") | Some("authentication_error") => 400,
            _ => 500,
        }
    }

    /// The message the user sees, naming the condition when Anthropic did.
    fn describe(&self) -> String {
        let detail = self
            .message
            .clone()
            .or_else(|| self.kind.clone())
            .unwrap_or_else(|| "no detail given".to_string());
        format!("the Anthropic stream reported an error mid-generation: {detail}")
    }
}

#[derive(Debug, Deserialize)]
struct MessageStartBody {
    #[serde(default)]
    usage: Option<Usage>,
}

/// The `usage` object, as it rides on both `message_start` and
/// `message_delta`. Every field is optional because each event carries only
/// the half it knows.
///
/// `input_tokens` counts *only* the tokens that were billed at full price:
/// anything served from or written to the prompt cache is reported separately.
/// Summing the three is the only way to recover the real prompt size, and
/// getting that wrong is not a cosmetic error: Wizard drives context pressure
/// and mid-turn compaction off the reported prompt size, so a cache hit would
/// otherwise read as "the context just emptied" and compaction would never
/// fire.
#[derive(Debug, Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: Option<u64>,
    /// Tokens served from the prompt cache, billed at ~0.1x.
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    /// Tokens written to the prompt cache, billed at ~1.25x.
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum BlockStart {
    #[serde(rename = "tool_use")]
    ToolUse {
        /// The `toolu_…` id Anthropic issued. It rides through history on the
        /// [`ToolCall`] and comes back as the answering block's
        /// `tool_use_id`; it used to be dropped here and re-invented on the
        /// way out, which is why parallel calls could not be answered.
        #[serde(default)]
        id: String,
        name: String,
    },
    /// Opens an extended-thinking block. The text arrives as
    /// [`BlockDelta::Thinking`] and the signature as
    /// [`BlockDelta::Signature`]; both are usually empty here.
    #[serde(rename = "thinking")]
    Thinking {
        #[serde(default)]
        thinking: String,
        #[serde(default)]
        signature: Option<String>,
    },
    /// Reasoning the API withheld: no text, one opaque payload, replayed
    /// verbatim.
    #[serde(rename = "redacted_thinking")]
    RedactedThinking {
        #[serde(default)]
        data: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum BlockDelta {
    #[serde(rename = "text_delta")]
    Text { text: String },
    /// Extended-thinking reasoning fragment.
    #[serde(rename = "thinking_delta")]
    Thinking { thinking: String },
    /// The signature over a finished thinking block. Anthropic verifies it on
    /// replay, so it has to survive the decode byte for byte.
    #[serde(rename = "signature_delta")]
    Signature { signature: String },
    #[serde(rename = "input_json_delta")]
    InputJson { partial_json: String },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct MessageDeltaBody {
    /// "end_turn", "max_tokens", "tool_use", "stop_sequence", ...
    #[serde(default)]
    stop_reason: Option<String>,
}

/// Per-index accumulator for a streamed `tool_use` block.
#[derive(Debug, Default)]
struct ToolAccum {
    id: String,
    name: String,
    input: String,
}

/// Per-index accumulator for a streamed `thinking` / `redacted_thinking`
/// block.
#[derive(Debug, Default)]
struct ThinkingAccum {
    text: String,
    signature: String,
    /// Opaque payload of a `redacted_thinking` block, which has no text.
    data: Option<String>,
}

/// The prompt-side token counts from one response's `usage`.
#[derive(Debug, Default)]
struct PromptTokens {
    /// Tokens billed at full price.
    input: Option<u64>,
    /// Tokens served from the prompt cache (the number that says whether any
    /// of the caching worked).
    cache_read: Option<u64>,
    /// Tokens written to the prompt cache.
    cache_creation: Option<u64>,
}

impl PromptTokens {
    /// Fold in whichever fields this event carried. A later event only
    /// overwrites what it actually reports.
    fn absorb(&mut self, usage: &Usage) {
        if usage.input_tokens.is_some() {
            self.input = usage.input_tokens;
        }
        if usage.cache_read_input_tokens.is_some() {
            self.cache_read = usage.cache_read_input_tokens;
        }
        if usage.cache_creation_input_tokens.is_some() {
            self.cache_creation = usage.cache_creation_input_tokens;
        }
    }

    /// The real prompt size: uncached plus cache-read plus cache-write. `None`
    /// when the stream reported no prompt usage at all, which is what
    /// [`ChatChunk::prompt_eval_count`] means by "unknown".
    fn total(&self) -> Option<u64> {
        if self.input.is_none() && self.cache_read.is_none() && self.cache_creation.is_none() {
            return None;
        }
        Some(
            self.input
                .unwrap_or(0)
                .saturating_add(self.cache_read.unwrap_or(0))
                .saturating_add(self.cache_creation.unwrap_or(0)),
        )
    }

    /// The same three numbers as the split [`CacheTokens`] reports: subsets of
    /// [`Self::total`], which is the form every consumer downstream expects
    /// (see [`CacheTokens`] on why the wire shape here is the odd one out).
    fn split(&self) -> CacheTokens {
        CacheTokens {
            read: self.cache_read.unwrap_or(0),
            write: self.cache_creation.unwrap_or(0),
        }
    }
}

/// Decoder state for [`decode_sse`].
struct SseState<S> {
    bytes: S,
    buf: Vec<u8>,
    tool_calls: BTreeMap<u64, ToolAccum>,
    /// Reasoning blocks by stream index, so a replay can carry the signature
    /// back.
    thinking: BTreeMap<u64, ThinkingAccum>,
    prompt_tokens: PromptTokens,
    eval_count: Option<u64>,
    /// `stop_reason` from `message_delta` ("end_turn", "max_tokens", ...).
    done_reason: Option<String>,
    /// Saw `message_stop` or EOF: drain, then emit the final chunk.
    saw_stop: bool,
    /// The API *said* the message was over — `message_stop`, or the
    /// `stop_reason` that rides on `message_delta`. EOF also sets
    /// [`SseState::saw_stop`], and telling the two apart is what stops a
    /// connection cut mid-generation from being handed to the agent as a
    /// complete, shorter reply.
    terminated: bool,
    /// An `event: error` the stream carried, raised after the buffer has been
    /// drained so the text that preceded it still reaches the agent.
    failure: Option<StreamError>,
    emitted_final: bool,
}

/// Build the final `done: true` chunk from the accumulated `thinking` and
/// `tool_use` blocks.
///
/// Reasoning leads the message, which is the order Anthropic requires on the
/// way back in: a caller that keeps this message in history can replay the
/// turn verbatim, signature included.
fn build_final<S>(state: &SseState<S>) -> ChatChunk {
    // The cache split is the only evidence that any of the breakpoints hit,
    // and the only thing that keeps the turn from being billed as all-fresh
    // input: it rides out on the chunk (see `ChatChunk::cache`) and is logged
    // besides, because a run whose `cache_read` stays at zero across steps
    // has a silent invalidator in the prefix, and a cost column alone does not
    // say which step stopped hitting.
    if state.prompt_tokens.cache_read.is_some() || state.prompt_tokens.cache_creation.is_some() {
        tracing::debug!(
            uncached_input_tokens = state.prompt_tokens.input.unwrap_or(0),
            cache_read_tokens = state.prompt_tokens.cache_read.unwrap_or(0),
            cache_creation_tokens = state.prompt_tokens.cache_creation.unwrap_or(0),
            "anthropic prompt cache",
        );
    }
    let thinking: Vec<ContentBlock> = state
        .thinking
        .values()
        // A block with nothing but a signature still has to survive: with
        // `display: "omitted"` the reasoning text never arrives, and the
        // signature is the whole of what a replay needs.
        .filter(|accum| {
            !accum.text.is_empty() || accum.data.is_some() || !accum.signature.is_empty()
        })
        .map(|accum| {
            ContentBlock::Thinking(super::ThinkingBlock {
                thinking: accum.text.clone(),
                signature: (!accum.signature.is_empty()).then(|| accum.signature.clone()),
                data: accum.data.clone(),
            })
        })
        .collect();
    let mut tool_calls: Vec<ToolCall> = state
        .tool_calls
        .values()
        .filter(|accum| !accum.name.is_empty())
        .map(|accum| {
            let arguments = if accum.input.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str::<Value>(&accum.input)
                    .unwrap_or_else(|_| Value::String(accum.input.clone()))
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
    // Anthropic always sends an id; this only covers a proxy that does not.
    crate::llm::ensure_tool_call_ids(&mut tool_calls);
    let message = (!tool_calls.is_empty() || !thinking.is_empty()).then(|| {
        let mut content = thinking;
        content.extend(tool_calls.into_iter().map(ContentBlock::ToolUse));
        ChatMessage::new(Role::Assistant, content)
    });
    ChatChunk {
        message,
        images: Vec::new(),
        thinking: false,
        done: true,
        done_reason: state.done_reason.clone(),
        eval_count: state.eval_count,
        prompt_eval_count: state.prompt_tokens.total(),
        cache: state.prompt_tokens.split(),
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

/// Decode an Anthropic Messages SSE byte stream into a [`ChatStream`]: text
/// and thinking deltas are emitted live; `tool_use` blocks are accumulated and
/// emitted in a single synthesized `done: true` chunk at the end.
pub(crate) fn decode_sse<S>(bytes: S) -> ChatStream
where
    S: Stream<Item = Result<Vec<u8>>> + Send + Unpin + 'static,
{
    let state = SseState {
        bytes,
        buf: Vec::new(),
        tool_calls: BTreeMap::new(),
        thinking: BTreeMap::new(),
        prompt_tokens: PromptTokens::default(),
        eval_count: None,
        done_reason: None,
        saw_stop: false,
        terminated: false,
        failure: None,
        emitted_final: false,
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
                let event: Event = match serde_json::from_str(payload) {
                    Ok(event) => event,
                    Err(_) => continue,
                };
                match event {
                    Event::MessageStart { message } => {
                        // Prompt-side counts only: `message_start` carries a
                        // placeholder `output_tokens` that `message_delta`
                        // supersedes, and reading it here would leave a
                        // truncated stream reporting the placeholder as the
                        // reply's size.
                        if let Some(usage) = message.usage {
                            state.prompt_tokens.absorb(&usage);
                        }
                    }
                    Event::ContentBlockStart {
                        index,
                        content_block,
                    } => match content_block {
                        BlockStart::ToolUse { id, name } => {
                            let accum = state.tool_calls.entry(index).or_default();
                            accum.id = id;
                            accum.name = name;
                        }
                        BlockStart::Thinking {
                            thinking,
                            signature,
                        } => {
                            let accum = state.thinking.entry(index).or_default();
                            accum.text.push_str(&thinking);
                            if let Some(signature) = signature {
                                accum.signature = signature;
                            }
                        }
                        BlockStart::RedactedThinking { data } => {
                            state.thinking.entry(index).or_default().data = Some(data);
                        }
                        BlockStart::Other => {}
                    },
                    Event::ContentBlockDelta { index, delta } => match delta {
                        BlockDelta::Text { text } => {
                            if !text.is_empty() {
                                return Ok(Some((text_chunk(text, false), state)));
                            }
                        }
                        BlockDelta::Thinking { thinking } => {
                            if !thinking.is_empty() {
                                state
                                    .thinking
                                    .entry(index)
                                    .or_default()
                                    .text
                                    .push_str(&thinking);
                                return Ok(Some((text_chunk(thinking, true), state)));
                            }
                        }
                        BlockDelta::Signature { signature } => {
                            state
                                .thinking
                                .entry(index)
                                .or_default()
                                .signature
                                .push_str(&signature);
                        }
                        BlockDelta::InputJson { partial_json } => {
                            state
                                .tool_calls
                                .entry(index)
                                .or_default()
                                .input
                                .push_str(&partial_json);
                        }
                        BlockDelta::Other => {}
                    },
                    Event::MessageDelta { delta, usage } => {
                        if let Some(reason) = delta.and_then(|d| d.stop_reason) {
                            state.done_reason = Some(reason);
                            // A `stop_reason` is the API stating why the reply
                            // ended; a `message_stop` that never arrives after
                            // one is a formality, not a truncation.
                            state.terminated = true;
                        }
                        if let Some(usage) = usage {
                            state.prompt_tokens.absorb(&usage);
                            if let Some(output) = usage.output_tokens {
                                state.eval_count = Some(output);
                            }
                        }
                    }
                    Event::MessageStop => {
                        state.saw_stop = true;
                        state.terminated = true;
                    }
                    Event::Error { error } => {
                        // Stop reading, but drain what is buffered first: the
                        // deltas ahead of the error were already shown.
                        state.failure = Some(error.unwrap_or(StreamError {
                            kind: None,
                            message: None,
                        }));
                        state.saw_stop = true;
                        state.terminated = true;
                    }
                    Event::Other => {}
                }
            }
            if state.saw_stop {
                if let Some(error) = state.failure.take() {
                    return Err(crate::llm::http_error_with_retry_after(
                        error.status(),
                        error.describe(),
                        None,
                    ));
                }
                if !state.terminated {
                    return Err(crate::llm::stream_ended_early("the Anthropic stream"));
                }
                state.emitted_final = true;
                let final_chunk = build_final(&state);
                return Ok(Some((final_chunk, state)));
            }
            match state.bytes.next().await {
                Some(Ok(data)) => state.buf.extend_from_slice(&data),
                Some(Err(e)) => return Err(e),
                None => {
                    if !state.buf.is_empty() && state.buf.last() != Some(&b'\n') {
                        state.buf.push(b'\n');
                    }
                    state.saw_stop = true;
                }
            }
        }
    })
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolSpec;

    fn provider() -> AnthropicProvider {
        AnthropicProvider::new("https://api.anthropic.com/", "claude-fable-5", "key")
    }

    #[test]
    fn user_images_become_base64_image_blocks() {
        let (_system, messages) = build_messages(&[ChatMessage::user_with_images(
            "what is on screen?",
            vec![
                crate::llm::Image::new("QUJD", "image/png"),
                crate::llm::Image::new("REVG", "image/jpeg"),
            ],
        )]);
        let content = &messages[0]["content"];
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "what is on screen?");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["type"], "base64");
        assert_eq!(content[1]["source"]["media_type"], "image/png");
        assert_eq!(content[1]["source"]["data"], "QUJD");
        // The media type comes from the image, not a hard-coded default.
        assert_eq!(content[2]["source"]["media_type"], "image/jpeg");
    }

    #[test]
    fn assistant_images_are_named_in_the_text_not_sent_as_blocks() {
        // Anthropic takes `image` blocks on user turns only; replaying an
        // assistant turn that generated one must degrade to text.
        let mut assistant = ChatMessage::assistant("here it is");
        assistant.push_image(crate::llm::Image::new("QUJD", "image/png"));
        let (_system, messages) = build_messages(&[assistant]);
        let blocks = &messages[0]["content"];
        assert_eq!(blocks.as_array().expect("blocks").len(), 1);
        assert_eq!(blocks[0]["type"], "text");
        let text = blocks[0]["text"].as_str().expect("text");
        assert!(text.contains("here it is"));
        assert!(text.contains("generated 1 image(s) (image/png)"), "{text}");
    }

    /// A tool call whose arguments never parsed cannot be sent as a string.
    ///
    /// `build_final` degrades unparseable streamed arguments to
    /// `Value::String(raw)` so the model can see what it tried to send.
    /// `build_messages` then emitted `"input": "not json"`, which the Messages
    /// API answers with a 400 — a status this client classifies permanent, so
    /// the turn ended with no retry. And the assistant message was already in
    /// the session by then, so every `/resume` of it reproduced the same 400
    /// on the first request.
    #[test]
    fn a_tool_call_that_never_parsed_is_still_a_legal_request() {
        let mut assistant = ChatMessage::assistant("");
        assistant.push_tool_call(ToolCall::new(
            "read_file",
            Value::String("{\"path\": \"src/mai".to_string()),
        ));
        // A double-encoded object is decoded rather than thrown away.
        assistant.push_tool_call(ToolCall::new(
            "write_file",
            Value::String(r#"{"path":"a.txt"}"#.to_string()),
        ));
        // Not an object at all, and not `null` either.
        assistant.push_tool_call(ToolCall::new("list_files", json!([1, 2, 3])));

        let (_system, messages) = build_messages(&[assistant]);
        let blocks: Vec<&Value> = messages[0]["content"]
            .as_array()
            .expect("content blocks")
            .iter()
            .filter(|block| block["type"] == "tool_use")
            .collect();
        assert_eq!(blocks.len(), 3);
        for block in &blocks {
            assert!(
                block["input"].is_object(),
                "tool_use.input must be an object, got {}",
                block["input"]
            );
        }
        assert_eq!(blocks[0]["input"], json!({}));
        assert_eq!(blocks[1]["input"], json!({ "path": "a.txt" }));
        assert_eq!(blocks[2]["input"], json!({}));
    }

    #[test]
    fn translates_native_request_to_messages_shape() {
        let mut assistant = ChatMessage::assistant("Let me read it.");
        assistant.push_tool_call(ToolCall::new("read_file", json!({ "path": "src/main.rs" })));
        let call_id = assistant.tool_calls()[0].id.clone();
        let request = ChatRequest {
            model: "claude-fable-5".to_string(),
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
            options: Some(crate::llm::ChatOptions {
                temperature: Some(0.5),
                num_ctx: None,
                reasoning_effort: None,
            }),
        };

        let body = provider().build_request_body(&request);
        assert_eq!(body["model"], "claude-fable-5");
        assert_eq!(
            body["max_tokens"],
            crate::llm::anthropic_max_output_tokens("claude-fable-5")
        );
        assert_eq!(body["stream"], true);
        // The system prompt is a *block array*, not a string: only that form
        // has anywhere to hang a cache breakpoint.
        assert_eq!(body["system"][0]["type"], "text");
        assert_eq!(body["system"][0]["text"], "You are Wizard.");

        // messages[0]: user text block; messages[1]: assistant text + tool_use.
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"][0]["type"], "text");

        let assistant = &body["messages"][1];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["content"][0]["type"], "text");
        let tool_use = &assistant["content"][1];
        assert_eq!(tool_use["type"], "tool_use");
        assert_eq!(tool_use["name"], "read_file");
        assert_eq!(tool_use["input"]["path"], "src/main.rs");
        assert_eq!(tool_use["id"], call_id, "the provider's own id, verbatim");

        // messages[2]: user message carrying the tool_result, correlated by id.
        let result = &body["messages"][2];
        assert_eq!(result["role"], "user");
        assert_eq!(result["content"][0]["type"], "tool_result");
        assert_eq!(result["content"][0]["tool_use_id"], call_id);
        assert_eq!(result["content"][0]["content"], "fn main() {}");

        assert_eq!(body["tools"][0]["name"], "read_file");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        // `temperature` is a 400 on this model line and cannot ride with a
        // `thinking` block on any of them, so it is dropped rather than sent.
        assert!(body.get("temperature").is_none(), "{body}");
    }

    #[test]
    fn max_tokens_follows_the_requested_model_not_a_fixed_ceiling() {
        // The failure this prevents: one hardcoded 32k `max_tokens` against a
        // model whose own ceiling is lower is an HTTP 400, which
        // `ProviderError::is_transient` correctly calls permanent, so the
        // turn dies with no retry. The request carries its own model (a
        // `/model` switch does not rebuild the provider), so the lookup has
        // to be per request, not per client.
        let provider = provider();
        let body_for = |model: &str| {
            provider.build_request_body(&ChatRequest {
                model: model.to_string(),
                messages: vec![ChatMessage::user("hi")],
                tools: Vec::new(),
                stream: true,
                options: None,
            })
        };
        // A Claude 3.5 snapshot: 8192 is its ceiling, and 32000 was a 400.
        assert_eq!(body_for("claude-3-5-sonnet-20241022")["max_tokens"], 8_192);
        // Claude 3: lower still.
        assert_eq!(body_for("claude-3-opus-20240229")["max_tokens"], 4_096);
        // The current line is not capped down to the old constant either.
        assert_eq!(body_for("claude-opus-5")["max_tokens"], 128_000);
        // An unrecognized tag (a new release, a proxy's own naming) gets the
        // conservative floor rather than a number that might 400.
        assert_eq!(
            body_for("some-proxy/model")["max_tokens"],
            crate::llm::DEFAULT_ANTHROPIC_MAX_TOKENS
        );
    }

    #[test]
    fn empty_and_image_only_user_messages_keep_the_api_shape() {
        // The API rejects an empty text block, so an image-only user message
        // must carry no text part, while an empty text-only message keeps
        // its (empty) text block, and an empty assistant turn still sends one
        // block rather than an empty content array.
        let (_system, messages) = build_messages(&[
            ChatMessage::user_with_images("", vec![crate::llm::Image::new("QUJD", "image/png")]),
            ChatMessage::user(""),
            ChatMessage::assistant(""),
        ]);
        let image_only = messages[0]["content"].as_array().expect("blocks");
        assert_eq!(image_only.len(), 1);
        assert_eq!(image_only[0]["type"], "image");
        let empty_user = messages[1]["content"].as_array().expect("blocks");
        assert_eq!(empty_user.len(), 1);
        assert_eq!(empty_user[0]["type"], "text");
        let empty_assistant = messages[2]["content"].as_array().expect("blocks");
        assert_eq!(empty_assistant.len(), 1);
        assert_eq!(empty_assistant[0]["text"], "");
    }

    /// A whole parallel batch is answered by ONE user message holding every
    /// `tool_result` block. Anthropic requires exactly that: the results for an
    /// assistant turn must all arrive in the message that follows it, so the
    /// old one-message-per-result shape was a 400 for any two-call reply, and
    /// there was no test for a multi-call batch at all.
    #[test]
    fn a_parallel_batch_is_answered_by_one_message_of_tool_results() {
        let mut assistant = ChatMessage::assistant("on it");
        assistant.push_tool_call(ToolCall::new("read_file", json!({ "path": "a" })));
        assistant.push_tool_call(ToolCall::new("read_file", json!({ "path": "b" })));
        let ids: Vec<String> = assistant
            .tool_calls()
            .iter()
            .map(|call| call.id.clone())
            .collect();
        let mut results = ChatMessage::tool_result(&ids[0], "read_file", "contents of a");
        results.push_tool_result(&ids[1], "read_file", "contents of b");

        let (_system, messages) =
            build_messages(&[ChatMessage::user("read both"), assistant, results]);
        assert_eq!(messages.len(), 3, "one message answers the whole batch");
        let blocks = messages[2]["content"].as_array().expect("blocks");
        assert_eq!(blocks.len(), 2);
        assert_eq!(messages[2]["role"], "user");
        // Correlation is by id: both calls name the same tool, so a
        // name-plus-order match cannot tell them apart.
        assert_eq!(blocks[0]["tool_use_id"], ids[0]);
        assert_eq!(blocks[0]["content"], "contents of a");
        assert_eq!(blocks[1]["tool_use_id"], ids[1]);
        assert_eq!(blocks[1]["content"], "contents of b");
    }

    /// Every rule the Messages API enforces on a parallel batch, checked on
    /// the body the client actually posts rather than on `build_messages`
    /// alone. Each assertion here is a documented 400: an unanswered
    /// `tool_use` id, a `tool_result` that does not sit in the message
    /// immediately after the assistant turn, or a result correlated by
    /// anything but id. All three were true of the pre-block adapter, so a
    /// two-call reply could not be answered at all.
    #[test]
    fn a_two_call_batch_produces_a_body_the_messages_api_accepts() {
        let mut assistant = ChatMessage::assistant("reading both");
        assistant.push_tool_call(ToolCall::new("read_file", json!({ "path": "a" })));
        assistant.push_tool_call(ToolCall::new("read_file", json!({ "path": "b" })));
        let ids: Vec<String> = assistant
            .tool_calls()
            .iter()
            .map(|call| call.id.clone())
            .collect();
        let mut results = ChatMessage::tool_result(&ids[0], "read_file", "contents of a");
        results.push_tool_result(&ids[1], "read_file", "contents of b");
        let body = provider().build_request_body(&ChatRequest {
            model: "claude-fable-5".to_string(),
            messages: vec![
                ChatMessage::system("You are Wizard."),
                ChatMessage::user("read both"),
                assistant,
                results,
            ],
            tools: vec![ToolSpec::function(
                "read_file",
                "Read a file.",
                json!({ "type": "object" }),
            )],
            stream: true,
            options: None,
        });

        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 3, "{body}");
        // The assistant turn carries both calls...
        let calls: Vec<&Value> = messages[1]["content"]
            .as_array()
            .expect("assistant blocks")
            .iter()
            .filter(|block| block["type"] == "tool_use")
            .collect();
        assert_eq!(calls.len(), 2);
        // ...and the very next message answers every one of them, by id.
        assert_eq!(messages[2]["role"], "user");
        let answers = messages[2]["content"].as_array().expect("result blocks");
        assert_eq!(answers.len(), 2);
        for (call, answer) in calls.iter().zip(answers) {
            assert_eq!(answer["type"], "tool_result");
            assert_eq!(
                answer["tool_use_id"], call["id"],
                "every tool_use id must be answered in the following message"
            );
        }
        // Both calls name the same tool, so nothing here can be correlating
        // by name or by dispatch order.
        assert_eq!(calls[0]["name"], calls[1]["name"]);
        assert_ne!(answers[0]["tool_use_id"], answers[1]["tool_use_id"]);
        assert_eq!(answers[0]["content"], "contents of a");
        assert_eq!(answers[1]["content"], "contents of b");
    }

    /// The breakpoints the release is for, at the boundaries the render order
    /// (`tools` -> `system` -> `messages`) makes stable.
    #[test]
    fn cache_breakpoints_land_on_the_tool_system_and_history_tails() {
        let mut assistant = ChatMessage::assistant("on it");
        assistant.push_tool_call(ToolCall::new("read_file", json!({ "path": "a" })));
        let call_id = assistant.tool_calls()[0].id.clone();
        let body = provider().build_request_body(&ChatRequest {
            model: "claude-fable-5".to_string(),
            messages: vec![
                ChatMessage::system("charter"),
                ChatMessage::system("skills"),
                ChatMessage::user("read it"),
                assistant,
                ChatMessage::tool_result(&call_id, "read_file", "fn main() {}"),
            ],
            tools: vec![
                ToolSpec::function("read_file", "Read a file.", json!({ "type": "object" })),
                ToolSpec::function("execute", "Run a command.", json!({ "type": "object" })),
            ],
            stream: true,
            options: None,
        });

        // Tool-schema tail: caches the schemas on their own, so a system
        // prompt edit does not cost them.
        let tools = body["tools"].as_array().expect("tools");
        assert!(tools[0].get("cache_control").is_none());
        assert_eq!(tools[1]["cache_control"]["type"], "ephemeral");
        // System tail: covers tools plus the whole fixed preamble.
        let system = body["system"].as_array().expect("system");
        assert!(system[0].get("cache_control").is_none());
        assert_eq!(system[1]["cache_control"]["type"], "ephemeral");
        // History: the last block of each of the last two anchors, which here
        // are the assistant turn and the tool-result message answering it.
        // The user turn in front of them is not an anchor.
        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 3, "{body}");
        assert_eq!(
            count_breakpoints(&messages[0]),
            0,
            "a user turn is not an anchor"
        );
        for message in &messages[1..] {
            let blocks = message["content"].as_array().expect("blocks");
            assert_eq!(
                blocks.last().expect("a block")["cache_control"]["type"],
                "ephemeral",
                "{message}"
            );
            assert_eq!(count_breakpoints(message), 1, "one marker per anchor");
        }
        // Four is the API's hard cap, and this is exactly four.
        assert_eq!(count_breakpoints(&body), 4, "{body}");

        // The preamble is written once and read all session, so it takes the
        // hour; the history breakpoints are rewritten every step, and an hour
        // of TTL on a seconds-long entry is 2x for nothing.
        assert_eq!(tools[1]["cache_control"]["ttl"], "1h");
        assert_eq!(system[1]["cache_control"]["ttl"], "1h");
        for message in &messages[1..] {
            let blocks = message["content"].as_array().expect("blocks");
            assert!(
                blocks.last().expect("a block")["cache_control"]
                    .get("ttl")
                    .is_none(),
                "history stays on the 5-minute default: {message}"
            );
        }
    }

    /// The older of the two history breakpoints is not decoration: it is what
    /// keeps the read working when one agent step appends more than the 20
    /// content blocks a lookback walks. A 12-call batch appends an assistant
    /// turn of 13 blocks plus a result message of 12, so the next step's
    /// *newest* breakpoint is 25 blocks past the entry the previous step
    /// wrote, and finds nothing. The marker on the assistant turn is 13 blocks
    /// past it, which is inside the window.
    #[test]
    fn a_parallel_batch_cannot_push_the_previous_entry_out_of_lookback_range() {
        let batch = || {
            let mut assistant = ChatMessage::assistant("reading everything");
            for index in 0..12 {
                assistant.push_tool_call(ToolCall::new(
                    "read_file",
                    json!({ "path": index.to_string() }),
                ));
            }
            let ids: Vec<String> = assistant
                .tool_calls()
                .iter()
                .map(|call| call.id.clone())
                .collect();
            let mut results = ChatMessage::tool_result(&ids[0], "read_file", "0");
            for id in &ids[1..] {
                results.push_tool_result(id, "read_file", "x");
            }
            (assistant, results)
        };
        // Two consecutive steps of one agent turn. The second is the first
        // plus the batch, exactly as the loop grows history.
        let step = vec![
            ChatMessage::system("You are Wizard."),
            ChatMessage::user("read them all"),
            ChatMessage::assistant("on it"),
            ChatMessage::tool_result("toolu_first", "read_file", "contents"),
        ];
        let (assistant, results) = batch();
        let mut next_step = step.clone();
        next_step.push(assistant);
        next_step.push(results);

        let (_system, step) = build_messages(&step);
        let (_system, next_step) = build_messages(&next_step);
        // Where the previous step's newest entry ends.
        let entry_ends_at = step
            .iter()
            .rposition(|message| count_breakpoints(message) > 0)
            .expect("the previous step wrote an entry");
        // The nearest breakpoint the next step offers behind its own tail.
        let nearest = next_step
            .iter()
            .enumerate()
            .skip(entry_ends_at + 1)
            .find(|(_, message)| count_breakpoints(message) > 0)
            .map(|(index, _)| index)
            .expect("a breakpoint after the previous entry");
        let blocks = |messages: &[Value]| -> usize {
            messages
                .iter()
                .map(|message| message["content"].as_array().map_or(0, Vec::len))
                .sum()
        };
        assert_eq!(
            blocks(&next_step[entry_ends_at + 1..=nearest]),
            13,
            "the assistant turn alone stands between the entry and the marker"
        );
        // And the newest breakpoint on its own would not have reached: this is
        // the number the second marker exists to avoid.
        assert_eq!(blocks(&next_step[entry_ends_at + 1..]), 25);
    }

    /// The cache-correctness trap this release exists to avoid, in the exact
    /// shape `turn.rs` produces.
    ///
    /// `Agent::inject_pressure_signal` pushes the context-pressure note as a
    /// **user** message carrying a live token count, sends the completion, and
    /// pops it again. A breakpoint on that message would write a prefix ending
    /// in a number that never occurs in another request: the cache would never
    /// hit while still charging the 1.25x write, which is strictly worse than
    /// not caching at all. Everything up to and including the last breakpoint
    /// therefore has to be byte-identical between two requests that differ
    /// only in the note.
    #[test]
    fn the_cached_prefix_survives_a_changing_pressure_signal() {
        let with_signal = |note: &str| {
            let mut messages = vec![
                ChatMessage::system("You are Wizard."),
                ChatMessage::user("write the thing"),
                ChatMessage::assistant("on it"),
                ChatMessage::tool_result("toolu_1", "read_file", "fn main() {}"),
            ];
            // Exactly what the agent appends: a user turn, not a system one.
            messages.push(ChatMessage::user(note));
            provider().build_request_body(&ChatRequest {
                model: "claude-fable-5".to_string(),
                messages,
                tools: Vec::new(),
                stream: true,
                options: None,
            })
        };

        let first = with_signal("[context pressure] elevated · 41200 / 200000 (21%)");
        let second = with_signal("[context pressure] critical · 168930 / 200000 (84%)");
        assert_ne!(first, second, "the signal itself has to reach the model");
        assert_eq!(
            cached_prefix(&first),
            cached_prefix(&second),
            "the cached prefix must not move when only the signal changes"
        );
        // And the note is still delivered, as the trailing turn.
        let messages = second["messages"].as_array().expect("messages");
        let last = messages.last().expect("a message");
        assert_eq!(last["role"], "user");
        let text = last["content"][0]["text"].as_str().expect("text");
        assert!(text.contains("168930"), "{text}");
        assert_eq!(
            count_breakpoints(last),
            0,
            "the volatile note must sit behind the breakpoint, not carry one"
        );
    }

    /// The same rule from the other side: what one step caches has to still be
    /// a prefix of what the next step sends, or the entry it wrote can never
    /// be read and the 1.25x premium bought nothing.
    ///
    /// This is the property the pressure note breaks if it is allowed to
    /// anchor a breakpoint, because it is popped again before the next step.
    #[test]
    fn what_one_step_caches_is_still_a_prefix_of_the_next_step() {
        let base = vec![
            ChatMessage::system("You are Wizard."),
            ChatMessage::user("write the thing"),
            ChatMessage::assistant("on it"),
            ChatMessage::tool_result("toolu_1", "read_file", "fn main() {}"),
        ];
        let body_for = |messages: Vec<ChatMessage>| {
            provider().build_request_body(&ChatRequest {
                model: "claude-fable-5".to_string(),
                messages,
                tools: Vec::new(),
                stream: true,
                options: None,
            })
        };

        // Step N: history plus the ephemeral note.
        let mut step = base.clone();
        step.push(ChatMessage::user(
            "[context pressure] elevated · 41200 / 200000 (21%)",
        ));
        let step = body_for(step);
        // Step N+1: the note is gone, the turn it produced is in history, and
        // a fresh note with a different number is appended.
        let mut next_step = base;
        next_step.push(ChatMessage::assistant("done"));
        next_step.push(ChatMessage::user(
            "[context pressure] critical · 168930 / 200000 (84%)",
        ));
        let next_step = body_for(next_step);

        // Compared without the markers themselves: `cache_control` is a
        // placement hint, not prompt content, which is exactly why a read can
        // land on an entry whose breakpoint has since moved. What has to match
        // is the text the model is handed.
        let cached = without_breakpoints(&cached_prefix(&step));
        let sent = without_breakpoints(&next_step);
        let cached = cached["messages"].as_array().expect("messages");
        let sent = sent["messages"].as_array().expect("messages");
        assert!(
            cached.len() <= sent.len(),
            "step N cached more messages than step N+1 sends"
        );
        assert_eq!(
            cached.as_slice(),
            &sent[..cached.len()],
            "the entry step N wrote is not a prefix of what step N+1 sends, so \
             nothing will ever read it"
        );
        assert!(
            cached.len() >= 3,
            "step N cached almost nothing: {cached:?}"
        );
    }

    /// A copy with every `cache_control` marker removed: the prompt bytes the
    /// cache key is actually taken over.
    fn without_breakpoints(value: &Value) -> Value {
        match value {
            Value::Object(map) => Value::Object(
                map.iter()
                    .filter(|(key, _)| key.as_str() != "cache_control")
                    .map(|(key, value)| (key.clone(), without_breakpoints(value)))
                    .collect(),
            ),
            Value::Array(items) => Value::Array(items.iter().map(without_breakpoints).collect()),
            other => other.clone(),
        }
    }

    /// Only the *leading* run of system messages is the system prompt. A
    /// note that arrives later keeps its position in the conversation.
    #[test]
    fn only_leading_system_messages_are_hoisted() {
        let (system, messages) = build_messages(&[
            ChatMessage::system("charter"),
            ChatMessage::system("skills"),
            ChatMessage::user("hello"),
            ChatMessage::system("a subagent reported back"),
            ChatMessage::user("carry on"),
        ]);
        assert_eq!(system.len(), 2);
        assert_eq!(system[0]["text"], "charter");
        assert_eq!(system[1]["text"], "skills");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["role"], "user");
        assert!(
            messages[1]["content"][0]["text"]
                .as_str()
                .expect("text")
                .contains("a subagent reported back")
        );
        // The note is between the two user turns, not appended after them.
        assert_eq!(messages[2]["content"][0]["text"], "carry on");
    }

    /// `/effort` reaches the wire in whatever the model's generation calls
    /// it: `output_config.effort` alongside adaptive thinking on the current
    /// line, a clamped `budget_tokens` on the fixed-budget line, and nothing
    /// at all on a model that has no thinking parameter to set.
    #[test]
    fn effort_reaches_the_wire_in_the_form_each_generation_takes() {
        let body_for = |model: &str, effort: Option<&str>| {
            provider().build_request_body(&ChatRequest {
                model: model.to_string(),
                messages: vec![ChatMessage::user("hi")],
                tools: Vec::new(),
                stream: true,
                options: Some(crate::llm::ChatOptions {
                    temperature: Some(0.5),
                    num_ctx: None,
                    reasoning_effort: effort.map(str::to_string),
                }),
            })
        };

        // Adaptive line: `effort` is a sibling of the thinking block, and
        // reasoning summaries are opted into (the default is empty text,
        // which would leave Wizard's reasoning pane blank).
        let adaptive = body_for("claude-opus-5", Some("high"));
        assert_eq!(adaptive["thinking"]["type"], "adaptive");
        assert_eq!(adaptive["thinking"]["display"], "summarized");
        assert_eq!(adaptive["output_config"]["effort"], "high");
        // Opus 4.6 already summarizes by default and predates `display`,
        // where an unknown field is a 400.
        let older_adaptive = body_for("claude-opus-4-6", Some("low"));
        assert_eq!(older_adaptive["thinking"]["type"], "adaptive");
        assert!(older_adaptive["thinking"].get("display").is_none());
        assert_eq!(older_adaptive["output_config"]["effort"], "low");

        // `xhigh` arrived with the 4.7 line, between `high` and `max`. On the
        // 4.6 pair it is an invalid enum value, and an invalid enum value is a
        // 400, which `ProviderError::is_transient` correctly calls permanent,
        // so the turn would die with no retry. It is dropped there, which
        // lands the request on the API's own default of `high`.
        assert_eq!(
            body_for("claude-opus-5", Some("xhigh"))["output_config"]["effort"],
            "xhigh"
        );
        for model in ["claude-opus-4-6", "claude-sonnet-4-6"] {
            let body = body_for(model, Some("xhigh"));
            assert_eq!(body["thinking"]["type"], "adaptive", "{model}");
            assert!(
                body.get("output_config").is_none(),
                "{model} has no xhigh level: {body}"
            );
        }
        // `max` predates 4.7 and is legal on the whole adaptive line, so the
        // filter above must not have swept it up with `xhigh`.
        assert_eq!(
            body_for("claude-opus-4-6", Some("max"))["output_config"]["effort"],
            "max"
        );

        // Adaptive thinking is on even with no configured effort, which is
        // what makes the reasoning stream arrive at all.
        let no_effort = body_for("claude-fable-5", None);
        assert_eq!(no_effort["thinking"]["type"], "adaptive");
        assert!(no_effort.get("output_config").is_none());

        // Fixed-budget line: a token budget, and only when asked for.
        let budget = body_for("claude-sonnet-4-5", Some("medium"));
        assert_eq!(budget["thinking"]["type"], "enabled");
        assert_eq!(budget["thinking"]["budget_tokens"], 16_384);
        assert!(budget.get("output_config").is_none());
        assert!(
            body_for("claude-sonnet-4-5", None)
                .get("thinking")
                .is_none()
        );
        // The budget is spent out of `max_tokens`, so a request whose
        // ceiling is lower than the nominal budget gets it clamped rather
        // than a 400. Opus 4.1 tops out at 32000.
        let clamped = body_for("claude-opus-4-1", Some("high"));
        assert_eq!(clamped["thinking"]["budget_tokens"], 32_000 - 1_024);

        // A model with no thinking parameter gets none, and an unrecognized
        // effort level is dropped rather than forwarded into a 400.
        assert!(
            body_for("claude-3-5-sonnet-20241022", Some("high"))
                .get("thinking")
                .is_none()
        );
        assert!(
            body_for("claude-opus-5", Some("ludicrous"))["thinking"]["display"] == "summarized"
        );
        assert!(
            body_for("claude-opus-5", Some("ludicrous"))
                .get("output_config")
                .is_none()
        );
    }

    /// `temperature` is removed on every model that takes adaptive thinking
    /// (a 400 from Opus 4.7 on), and cannot ride with an enabled `thinking`
    /// block on the older line either. It survives only where neither
    /// applies.
    #[test]
    fn temperature_is_dropped_wherever_thinking_is_enabled() {
        let body_for = |model: &str, effort: Option<&str>| {
            provider().build_request_body(&ChatRequest {
                model: model.to_string(),
                messages: vec![ChatMessage::user("hi")],
                tools: Vec::new(),
                stream: true,
                options: Some(crate::llm::ChatOptions {
                    temperature: Some(0.5),
                    num_ctx: None,
                    reasoning_effort: effort.map(str::to_string),
                }),
            })
        };
        for model in [
            "claude-fable-5",
            "claude-opus-5",
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-sonnet-5",
            "claude-opus-4-6",
        ] {
            assert!(
                body_for(model, None).get("temperature").is_none(),
                "{model} rejects sampling parameters"
            );
        }
        assert!(
            body_for("claude-sonnet-4-5", Some("high"))
                .get("temperature")
                .is_none(),
            "extended thinking requires the default temperature"
        );
        // No thinking on the request, so the sampling knob is the only one
        // there is.
        assert_eq!(body_for("claude-sonnet-4-5", None)["temperature"], 0.5);
        assert_eq!(
            body_for("claude-3-5-sonnet-20241022", None)["temperature"],
            0.5
        );
    }

    /// A signed thinking block replays at the head of the assistant turn; an
    /// unsigned one is dropped (Anthropic verifies the signature and rejects
    /// the whole request when it does not match), and every one of them comes
    /// back out when the request is going to a model that cannot take a
    /// `thinking` parameter at all: history outlives a `/model` switch.
    #[test]
    fn signed_reasoning_replays_at_the_head_of_the_assistant_turn() {
        let turn = || {
            let mut assistant = ChatMessage::new(
                Role::Assistant,
                vec![
                    ContentBlock::thinking("weighing options", Some("sig-abc".to_string())),
                    ContentBlock::text("here goes"),
                ],
            );
            assistant.push_tool_call(ToolCall::new("execute", json!({ "command": "ls" })));
            vec![
                ChatMessage::system("You are Wizard."),
                ChatMessage::user("do it"),
                assistant,
            ]
        };
        let body_for = |model: &str| {
            provider().build_request_body(&ChatRequest {
                model: model.to_string(),
                messages: turn(),
                tools: Vec::new(),
                stream: true,
                options: None,
            })
        };

        let blocks = body_for("claude-fable-5")["messages"][1]["content"]
            .as_array()
            .expect("blocks")
            .clone();
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[0]["thinking"], "weighing options");
        assert_eq!(
            blocks[0]["signature"], "sig-abc",
            "the signature has to come back byte for byte"
        );
        assert_eq!(blocks[1]["type"], "text");
        assert_eq!(blocks[2]["type"], "tool_use");

        // Same history, a model with no `thinking` parameter: the block is
        // removed rather than 400-ing the turn.
        let downgraded = body_for("claude-3-5-sonnet-20241022");
        let blocks = downgraded["messages"][1]["content"]
            .as_array()
            .expect("blocks");
        assert!(
            blocks.iter().all(|block| block["type"] != "thinking"),
            "{downgraded}"
        );
        assert_eq!(blocks[0]["type"], "text");

        // An unsigned block cannot be replayed and is left out.
        let (_system, messages) = build_messages(&[ChatMessage::new(
            Role::Assistant,
            vec![
                ContentBlock::thinking("unsigned", None),
                ContentBlock::text("answer"),
            ],
        )]);
        let blocks = messages[0]["content"].as_array().expect("blocks");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "text");
    }

    /// Removing a replayed reasoning block must not take a cache breakpoint
    /// with it. An assistant turn that reasoned but said nothing and called
    /// nothing carries the history breakpoint on its reasoning block, so on
    /// the way to a model with no `thinking` parameter the marker has to move
    /// rather than vanish.
    #[test]
    fn stripping_reasoning_keeps_the_breakpoint_it_was_carrying() {
        let turn = vec![
            ChatMessage::system("You are Wizard."),
            ChatMessage::user("do it"),
            ChatMessage::new(
                Role::Assistant,
                vec![ContentBlock::thinking(
                    "weighing options",
                    Some("sig".to_string()),
                )],
            ),
        ];
        let body = provider().build_request_body(&ChatRequest {
            model: "claude-3-5-sonnet-20241022".to_string(),
            messages: turn,
            tools: Vec::new(),
            stream: true,
            options: None,
        });

        let assistant = &body["messages"][1];
        let blocks = assistant["content"].as_array().expect("blocks");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "text", "{assistant}");
        assert_eq!(
            blocks[0]["cache_control"]["type"], "ephemeral",
            "the breakpoint moved to the block that replaced the reasoning"
        );
        // System tail plus the one history anchor, and nothing duplicated.
        assert_eq!(count_breakpoints(&body), 2, "{body}");
    }

    /// Redacted reasoning has no text and no signature of its own: it is one
    /// opaque payload, replayed verbatim.
    #[test]
    fn redacted_reasoning_replays_as_its_payload() {
        let (_system, messages) = build_messages(&[ChatMessage::new(
            Role::Assistant,
            vec![
                ContentBlock::Thinking(crate::llm::ThinkingBlock {
                    thinking: String::new(),
                    signature: None,
                    data: Some("encrypted-blob".to_string()),
                }),
                ContentBlock::text("answer"),
            ],
        )]);
        let blocks = messages[0]["content"].as_array().expect("blocks");
        assert_eq!(blocks[0]["type"], "redacted_thinking");
        assert_eq!(blocks[0]["data"], "encrypted-blob");
    }

    /// Count the `cache_control` markers anywhere in a request body. The API
    /// caps a request at four.
    fn count_breakpoints(value: &Value) -> usize {
        match value {
            Value::Object(map) => {
                let own = usize::from(map.contains_key("cache_control"));
                own + map.values().map(count_breakpoints).sum::<usize>()
            }
            Value::Array(items) => items.iter().map(count_breakpoints).sum(),
            _ => 0,
        }
    }

    /// Everything the cache key covers: the system blocks plus every message
    /// up to and including the one carrying the history breakpoint. Anything
    /// after it is outside the cached span by construction.
    fn cached_prefix(body: &Value) -> Value {
        let messages = body["messages"].as_array().expect("messages");
        let last_cached = messages
            .iter()
            .rposition(|message| count_breakpoints(message) > 0)
            .expect("a history breakpoint");
        json!({
            "system": body["system"],
            "messages": &messages[..=last_cached],
        })
    }

    #[tokio::test]
    async fn decodes_sse_text_and_tool_use() {
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(
                b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":9}}}\n\n"
                    .to_vec(),
            ),
            Ok(
                b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n"
                    .to_vec(),
            ),
            Ok(
                b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"execute\"}}\n\n"
                    .to_vec(),
            ),
            Ok(
                b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\":\"}}\n\n"
                    .to_vec(),
            ),
            Ok(
                b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"ls\\\"}\"}}\n\n"
                    .to_vec(),
            ),
            Ok(
                b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":6}}\n\n"
                    .to_vec(),
            ),
            Ok(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_vec()),
        ];
        let mut chunks = decode_sse(stream::iter(parts));

        let first = chunks.next().await.expect("text").expect("ok");
        assert!(!first.done);
        assert_eq!(first.message.expect("message").text(), "Hi");

        let last = chunks.next().await.expect("final").expect("ok");
        assert!(last.done);
        assert_eq!(last.done_reason.as_deref(), Some("tool_use"));
        assert_eq!(last.prompt_eval_count, Some(9));
        assert_eq!(last.eval_count, Some(6));
        let message = last.message.expect("tool call message");
        assert_eq!(message.tool_calls().len(), 1);
        assert_eq!(message.tool_calls()[0].function.name, "execute");
        assert_eq!(message.tool_calls()[0].function.arguments["command"], "ls");
        assert_eq!(
            message.tool_calls()[0].id,
            "toolu_1",
            "the id the stream carried, not one we invented"
        );

        assert!(chunks.next().await.is_none());
    }

    #[tokio::test]
    async fn decodes_thinking_deltas_as_thinking() {
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(
                b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n"
                    .to_vec(),
            ),
            Ok(
                b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Considering...\"}}\n\n"
                    .to_vec(),
            ),
            Ok(
                b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Answer.\"}}\n\n"
                    .to_vec(),
            ),
            Ok(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_vec()),
        ];
        let mut chunks = decode_sse(stream::iter(parts));

        let first = chunks.next().await.expect("thinking").expect("ok");
        assert!(first.thinking, "thinking delta is flagged");
        assert_eq!(first.message.expect("message").text(), "Considering...");

        let second = chunks.next().await.expect("text").expect("ok");
        assert!(!second.thinking, "visible text is not flagged");
        assert_eq!(second.message.expect("message").text(), "Answer.");

        let last = chunks.next().await.expect("final").expect("ok");
        assert!(last.done);
        assert!(chunks.next().await.is_none());
    }

    /// A cache hit must be observable, and it must not look like the context
    /// just emptied.
    ///
    /// `input_tokens` counts only what was billed at full price: on a hit,
    /// nearly the whole prompt is reported under `cache_read_input_tokens`
    /// instead. Reporting the uncached number alone would tell the agent its
    /// 46k-token prompt was 1.2k, which drives the context meter and the
    /// compaction trigger: a cache hit would have read as free headroom and
    /// compaction would never have fired.
    #[tokio::test]
    async fn cache_tokens_are_counted_into_the_reported_prompt_size() {
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(
                b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1200,\"cache_read_input_tokens\":44000,\"cache_creation_input_tokens\":900}}}\n\n"
                    .to_vec(),
            ),
            Ok(
                b"data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":31}}\n\n"
                    .to_vec(),
            ),
            Ok(b"data: {\"type\":\"message_stop\"}\n\n".to_vec()),
        ];
        let mut chunks = decode_sse(stream::iter(parts));

        let last = chunks.next().await.expect("final").expect("ok");
        assert!(last.done);
        assert_eq!(
            last.prompt_eval_count,
            Some(1_200 + 44_000 + 900),
            "the prompt size is uncached + cache read + cache write"
        );
        assert_eq!(last.eval_count, Some(31));
        // ...and the split rides out beside it, which is what stops the turn
        // being billed as 46,100 fresh input tokens. A cache read is a tenth
        // of the input rate here, so dropping this over-bills the cached part
        // by 10x; that is exactly what happened while `ChatChunk` had no
        // field for it.
        assert_eq!(
            last.cache,
            CacheTokens {
                read: 44_000,
                write: 900
            }
        );
    }

    /// A stream that reports no cache fields at all (a proxy, an older API
    /// version) still reports the plain prompt size.
    #[tokio::test]
    async fn a_stream_without_cache_fields_reports_the_plain_prompt_size() {
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(
                b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":9}}}\n\n"
                    .to_vec(),
            ),
            Ok(b"data: {\"type\":\"message_stop\"}\n\n".to_vec()),
        ];
        let mut chunks = decode_sse(stream::iter(parts));
        let last = chunks.next().await.expect("final").expect("ok");
        assert_eq!(last.prompt_eval_count, Some(9));
        assert_eq!(
            last.cache,
            CacheTokens::NONE,
            "no cache fields is no cache activity, and prices as all-fresh"
        );
    }

    /// The thinking block's signature has to survive the stream, or the
    /// follow-up turn cannot replay the reasoning it belongs to.
    #[tokio::test]
    async fn a_thinking_signature_survives_the_stream() {
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(
                b"data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n"
                    .to_vec(),
            ),
            Ok(
                b"data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"weighing \"}}\n\n"
                    .to_vec(),
            ),
            Ok(
                b"data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"options\"}}\n\n"
                    .to_vec(),
            ),
            Ok(
                b"data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-\"}}\n\n"
                    .to_vec(),
            ),
            Ok(
                b"data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"abc\"}}\n\n"
                    .to_vec(),
            ),
            Ok(
                b"data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"blob\"}}\n\n"
                    .to_vec(),
            ),
            Ok(
                b"data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"execute\"}}\n\n"
                    .to_vec(),
            ),
            Ok(b"data: {\"type\":\"message_stop\"}\n\n".to_vec()),
        ];
        let mut chunks = decode_sse(stream::iter(parts));

        // The reasoning still streams live, flagged, exactly as before.
        assert!(chunks.next().await.expect("thinking").expect("ok").thinking);
        assert!(chunks.next().await.expect("thinking").expect("ok").thinking);

        let last = chunks.next().await.expect("final").expect("ok");
        let message = last.message.expect("a message");
        // Reasoning leads the turn, which is the order the API requires back.
        let ContentBlock::Thinking(thinking) = &message.content[0] else {
            panic!("expected reasoning first, got {:?}", message.content[0]);
        };
        assert_eq!(thinking.thinking, "weighing options");
        assert_eq!(
            thinking.signature.as_deref(),
            Some("sig-abc"),
            "a split signature reassembles"
        );
        let ContentBlock::Thinking(redacted) = &message.content[1] else {
            panic!("expected the redacted block second");
        };
        assert_eq!(redacted.data.as_deref(), Some("blob"));
        assert!(redacted.signature.is_none());
        assert_eq!(message.tool_calls().len(), 1);

        // Round trip: the decoded turn replays with its signature intact.
        let (_system, messages) = build_messages(&[message]);
        let blocks = messages[0]["content"].as_array().expect("blocks");
        assert_eq!(blocks[0]["signature"], "sig-abc");
        assert_eq!(blocks[1]["type"], "redacted_thinking");
    }

    #[tokio::test]
    async fn reassembles_frames_split_across_reads() {
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(b"data: {\"type\":\"content_block_delta\",\"index\":0,\"del".to_vec()),
            Ok(
                b"ta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\ndata: {\"type\":\"mess"
                    .to_vec(),
            ),
            Ok(b"age_stop\"}\n\n".to_vec()),
        ];
        let mut chunks = decode_sse(stream::iter(parts));

        let first = chunks.next().await.expect("text").expect("ok");
        assert_eq!(first.message.expect("message").text(), "Hi");
        let last = chunks.next().await.expect("final").expect("ok");
        assert!(last.done);
        assert!(chunks.next().await.is_none());
    }

    #[tokio::test]
    async fn malformed_and_unknown_events_are_skipped() {
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(b"data: {not json at all\n\n".to_vec()),
            Ok(b"event: ping\ndata: {\"type\":\"ping\"}\n\n".to_vec()),
            Ok(
                b"data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n"
                    .to_vec(),
            ),
            Ok(b"data: {\"type\":\"message_stop\"}\n\n".to_vec()),
        ];
        let mut chunks = decode_sse(stream::iter(parts));

        let first = chunks.next().await.expect("text").expect("ok");
        assert_eq!(first.message.expect("message").text(), "ok");
        assert!(chunks.next().await.expect("final").expect("ok").done);
        assert!(chunks.next().await.is_none());
    }

    /// A stream that stops before the API says the message is over is a
    /// failure, not a short answer.
    ///
    /// This used to end the turn cleanly: the trailing line was flushed, a
    /// `done: true` chunk was synthesized with no `stop_reason`, and the agent
    /// took the half-sentence that had arrived as the model's reply and
    /// stopped. Nothing anywhere reported a problem, because from the agent's
    /// side nothing had gone wrong — which is precisely why "it randomly
    /// stops" was the only description anyone could give of it. The failure
    /// has to be typed and transient so the retry ladder re-runs the call.
    ///
    /// The partial text still streams out first: it was already on screen, and
    /// [`AgentEvent::StreamRetrying`](crate::agent::AgentEvent::StreamRetrying)
    /// is what tells the surfaces to drop it when the retry starts over.
    #[tokio::test]
    async fn a_stream_that_stops_before_message_stop_is_a_transient_failure() {
        let parts: Vec<Result<Vec<u8>>> = vec![Ok(
            b"data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}"
                .to_vec(),
        )];
        let mut chunks = decode_sse(stream::iter(parts));

        let first = chunks.next().await.expect("text").expect("ok");
        assert_eq!(first.message.expect("message").text(), "partial");
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

    /// A `stop_reason` without the `message_stop` that normally follows it is
    /// a complete reply: the API has already said why the message ended, and
    /// several proxies close the connection at that point rather than sending
    /// the final formality. Refusing it would turn every turn through such a
    /// proxy into an endless retry.
    #[tokio::test]
    async fn a_stop_reason_alone_ends_the_stream_cleanly() {
        let parts: Vec<Result<Vec<u8>>> = vec![Ok(
            b"data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n"
                .to_vec(),
        )];
        let mut chunks = decode_sse(stream::iter(parts));
        let last = chunks.next().await.expect("final").expect("ok");
        assert!(last.done);
        assert_eq!(last.done_reason.as_deref(), Some("end_turn"));
        assert!(chunks.next().await.is_none());
    }

    /// `event: error` inside a 200 stream, which is how Anthropic reports an
    /// overload that only becomes apparent after generation has started.
    ///
    /// It used to land in `Event::Other` and be dropped, so the stream ran to
    /// EOF and produced a *successful* completion holding whatever text had
    /// arrived. The agent has no reason to retry a success, so an overloaded
    /// Claude read as a model that had finished talking.
    #[tokio::test]
    async fn an_overloaded_error_inside_the_stream_is_a_transient_failure() {
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(
                b"data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"thinking about\"}}\n\n"
                    .to_vec(),
            ),
            Ok(
                b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n"
                    .to_vec(),
            ),
        ];
        let mut chunks = decode_sse(stream::iter(parts));

        // The text that arrived before the error is still delivered.
        let first = chunks.next().await.expect("text").expect("ok");
        assert_eq!(first.message.expect("message").text(), "thinking about");

        let err = chunks
            .next()
            .await
            .expect("an item")
            .expect_err("an in-band error is an error");
        let provider = err.downcast_ref::<ProviderError>().expect("typed");
        assert_eq!(
            provider.status,
            Some(529),
            "the same status a pre-stream overload returns, so one condition \
             classifies one way whenever it arrives"
        );
        assert!(provider.is_transient());
        assert!(provider.message.contains("Overloaded"), "{provider:?}");
    }

    /// A rate limit relayed in-band backs off like one received up front, and
    /// an error object with nothing readable in it still fails the stream
    /// rather than passing for a completed reply.
    #[tokio::test]
    async fn in_band_errors_carry_their_own_class() {
        let rate_limited: Vec<Result<Vec<u8>>> = vec![Ok(
            b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"slow down\"}}\n\n"
                .to_vec(),
        )];
        let err = decode_sse(stream::iter(rate_limited))
            .next()
            .await
            .expect("an item")
            .expect_err("an error");
        let provider = err.downcast_ref::<ProviderError>().expect("typed");
        assert_eq!(provider.status, Some(429));
        assert!(provider.is_transient());

        let bare: Vec<Result<Vec<u8>>> =
            vec![Ok(b"event: error\ndata: {\"type\":\"error\"}\n\n".to_vec())];
        let err = decode_sse(stream::iter(bare))
            .next()
            .await
            .expect("an item")
            .expect_err("an error");
        let provider = err.downcast_ref::<ProviderError>().expect("typed");
        assert_eq!(provider.status, Some(500), "attributed to the server");
        assert!(provider.is_transient());
    }

    #[tokio::test]
    async fn tool_input_that_is_not_json_degrades_to_a_string_argument() {
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(
                b"data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"execute\"}}\n\n"
                    .to_vec(),
            ),
            Ok(
                b"data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"not json\"}}\n\n"
                    .to_vec(),
            ),
            Ok(
                b"data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t2\",\"name\":\"list\"}}\n\n"
                    .to_vec(),
            ),
            Ok(b"data: {\"type\":\"message_stop\"}\n\n".to_vec()),
        ];
        let mut chunks = decode_sse(stream::iter(parts));

        let last = chunks.next().await.expect("final").expect("ok");
        assert!(last.done);
        let calls = last.message.expect("message").take_tool_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].function.name, "execute");
        assert_eq!(
            calls[0].function.arguments,
            Value::String("not json".to_string())
        );
        // A tool_use block that never received input gets empty arguments.
        assert_eq!(calls[1].function.name, "list");
        assert_eq!(calls[1].function.arguments, json!({}));
        assert!(chunks.next().await.is_none());
    }

    #[tokio::test]
    async fn a_mid_stream_transport_error_surfaces_as_an_error() {
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(
                b"data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n"
                    .to_vec(),
            ),
            Err(anyhow!("connection reset")),
        ];
        let mut chunks = decode_sse(stream::iter(parts));

        assert!(chunks.next().await.expect("text").is_ok());
        let err = chunks.next().await.expect("item").expect_err("error");
        assert!(err.to_string().contains("connection reset"));
    }

    #[test]
    fn context_window_table_covers_claude_and_unknowns() {
        assert_eq!(context_window("claude-fable-5"), Some(1_000_000));
        assert_eq!(context_window("claude-opus-4-8"), Some(1_000_000));
        assert_eq!(context_window("claude-sonnet-5"), Some(1_000_000));
        assert_eq!(context_window("claude-haiku-4-5"), Some(200_000));
        assert_eq!(context_window("Claude-Opus-4"), Some(200_000));
        assert_eq!(context_window("claude-sonnet-4-5[1m]"), Some(1_000_000));
        assert_eq!(context_window("gpt-4o"), None);
        assert_eq!(context_window(""), None);
    }
}
