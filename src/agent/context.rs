//! Context-window stewardship over a bare message history: how full the next
//! call's prompt is, and what to drop when it is too full.
//!
//! Nothing here knows about [`Agent`](super::Agent). That is the point. The
//! parent turn loop and [`subagent::spawn`](super::subagent::spawn) both run
//! for as long as the model keeps calling tools, and both therefore outgrow the
//! window the same way; until this module existed only the first of them could
//! do anything about it, so a long sub-run walked off the end of its context
//! and came back as a provider error. `/ultra` made that N sub-runs per turn.
//!
//! Two things are shared, and they are separate on purpose: [`pressure`]
//! *measures* and never mutates, because it is also what feeds the live signal
//! the model reads each step; [`compact`] cuts, and is the only thing that
//! does.
//!
//! # What differs between a conversation and a sub-loop
//!
//! One thing: where the kept tail is allowed to begin (see [`Anchor`]). Every
//! other rule — the low-water mark the tail is cut to, the rolling summary,
//! the fallback to truncation — is the same, because the constraint behind
//! them (a request the provider will still accept, carrying what the model
//! needs to keep working) is the same.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde_json::Value;

use crate::llm::provider::LlmProvider;
use crate::llm::{CacheTokens, ChatMessage, ChatOptions, ChatRequest, Role, ToolCall};

/// Most-recent messages a compaction pass keeps at the outside.
///
/// A ceiling, and no longer the rule. [`Budget::low_water_tokens`] is what
/// decides how much of the tail survives, and it cuts deeper than this
/// whenever ten messages are too many tokens — which is the case that
/// mattered. This stays because it is also what a `/compact` over a half-empty
/// window folds away: under the low-water mark there is nothing the budget
/// wants cut, and the request is still to cut something.
pub(crate) const KEEP_RECENT: usize = 10;

/// Heading of the note [`compact`] leaves in place of the span it summarized.
/// Public because it is the only handle anything downstream has on that note:
/// it is an ordinary message like any other, and
/// [`ultra::render_context`](super::ultra) has to be able to tell "everything
/// older than the tail, summarized" apart from an ordinary injected note when
/// it briefs a candidate.
pub const COMPACT_SUMMARY_HEADING: &str = "[Compacted progress summary]";

/// Prefix of the ephemeral per-step pressure line injected (in memory only)
/// before each model completion. Surfaces and compaction can recognize it; it
/// is never persisted to the session file.
pub const CONTEXT_PRESSURE_HEADING: &str = "[context pressure]";

/// Fraction of the provider's context window the last prompt may fill before
/// token-aware compaction kicks in.
const COMPACT_WINDOW_FRACTION: f64 = 0.8;

/// Fraction of the window a compaction pass cuts *down* to: the low-water
/// mark against [`COMPACT_WINDOW_FRACTION`]'s high-water trigger.
///
/// Half the trigger, and the gap between the two is the point. See
/// [`Budget::low_water_tokens`] for what happens without one.
const COMPACT_LOW_WATER_FRACTION: f64 = 0.4;

/// Bytes a superseded tool result has to be carrying before
/// [`evict_superseded_reads`] replaces it with a stub.
///
/// Rewriting a message mid-history invalidates the provider's cached prefix
/// from that point on, so an eviction has to save more than the re-write it
/// costs. A result smaller than this is not worth the cache entry, which is
/// also why eviction only ever runs inside a compaction pass, which was going
/// to invalidate that prefix anyway.
const STALE_RESULT_MIN_BYTES: usize = 500;

/// Soft pressure band: the model is nudged to call `compact` once fill crosses
/// this fraction of the known window (auto-compact still waits for
/// [`COMPACT_WINDOW_FRACTION`]).
pub(crate) const PRESSURE_ELEVATED_FRACTION: f64 = 0.5;

/// Strong pressure band: the model is told to compact *before* more tool work.
const PRESSURE_HIGH_FRACTION: f64 = 0.7;

/// Chunk size (chars) fed to one rolling-summary pass during compaction.
const COMPACT_CHUNK_CHARS: usize = 20_000;

/// How long a compaction summary's stream may produce nothing before it is
/// abandoned.
///
/// Shorter than the turn loop's equivalent
/// ([`turn::STREAM_IDLE_TIMEOUT`](super::turn)) because the stakes are the
/// other way round. Abandoning a real completion loses the model's work;
/// abandoning a summary costs a rolling pass and falls back to truncation,
/// which is a worse history but a live one. What must not happen is the third
/// option: a perpetual run parked forever inside a compaction that a proxy
/// stopped answering halfway through, which is silent, indefinite, and
/// indistinguishable from the run having stopped.
const SUMMARY_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// How full the next model call's prompt is, for the live pressure signal and
/// the `compact` tool's reply. Built from the last reported prompt size (or a
/// char/4 estimate) plus the provider's known context window when available.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextPressure {
    /// Tokens that will load into the next model call.
    pub tokens: u64,
    /// Provider context window in tokens, when known.
    pub window: Option<u32>,
    /// `tokens / window` when the window is known; otherwise a byte-threshold
    /// proxy so headless runs without a reported window still get a signal.
    pub fill: f64,
    pub level: PressureLevel,
}

/// Coarse pressure band shown to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressureLevel {
    /// Comfortable headroom.
    Ok,
    /// Crossing ~50% of the window (or half the byte threshold) — compact when
    /// convenient.
    Elevated,
    /// Crossing ~70% — compact before more tool work.
    High,
    /// At or past the auto-compact trigger (~80% / byte threshold).
    Critical,
}

impl PressureLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            PressureLevel::Ok => "ok",
            PressureLevel::Elevated => "elevated",
            PressureLevel::High => "high",
            PressureLevel::Critical => "critical",
        }
    }
}

impl ContextPressure {
    /// One-line note the model sees each step (ephemeral, not persisted).
    pub fn signal_line(&self) -> String {
        let tokens = crate::usage::format_tokens(self.tokens);
        let window = match self.window {
            Some(w) => crate::usage::format_tokens(u64::from(w)),
            None => "unknown window".to_string(),
        };
        let pct = (self.fill * 100.0).round() as i32;
        let advice = match self.level {
            PressureLevel::Ok => "headroom ok",
            PressureLevel::Elevated => "consider calling compact soon",
            PressureLevel::High => "call compact before more tool work",
            PressureLevel::Critical => "auto-compact imminent — call compact now",
        };
        format!(
            "{CONTEXT_PRESSURE_HEADING} {} · {tokens} / {window} ({pct}%) — {advice}",
            self.level.as_str()
        )
    }
}

/// The four numbers a pressure reading is derived from, gathered by whoever
/// owns the history. Taken as a struct rather than four arguments because
/// three of them are token counts and transposing two of those is a bug that
/// still compiles.
#[derive(Debug, Clone, Copy)]
pub struct Measured {
    /// Tokens that will load into the next call: the backend's last reported
    /// prompt size when it has one, otherwise a char/4 estimate.
    pub tokens: u64,
    /// The provider's context window for this model, when it is known.
    pub window: Option<u32>,
    /// Serialized size of the history, the fallback measure when it is not.
    pub bytes: usize,
    /// Byte ceiling that fallback is judged against.
    pub byte_threshold: usize,
    /// The last prompt size the backend actually *reported*, as opposed to
    /// [`Self::tokens`], which may be an estimate.
    pub last_prompt: Option<u64>,
}

/// Live fill of the next model call against the provider window (or a
/// byte-threshold proxy when the window is unknown). Powers the per-step
/// pressure signal and the `compact` tool's reply.
///
/// Soft bands (`elevated` / `high`) may use a char/4 estimate when the backend
/// has not reported a prompt size yet. The `critical` band — which drives
/// auto-compaction — needs a *reported* last prompt over 80% of a known
/// window; the byte threshold is only the fallback gate when the window is
/// unknown. A known window makes tokens the authoritative measure — the byte
/// proxy (48 KB default, sized for small local models) would otherwise scream
/// "critical" at a few percent of a large window. Estimates never trip
/// auto-compact on their own (they would fire on the system prompt alone and
/// steal the first completion of a short turn).
pub fn pressure(measured: Measured) -> ContextPressure {
    let Measured {
        tokens,
        window,
        bytes,
        byte_threshold,
        last_prompt,
    } = measured;
    let threshold = byte_threshold.max(1);

    let fill = match window {
        Some(w) if w > 0 => tokens as f64 / f64::from(w),
        _ => bytes as f64 / threshold as f64,
    };

    let auto_critical = match window {
        Some(w) if w > 0 => match last_prompt {
            Some(prompt) => prompt as f64 > f64::from(w) * COMPACT_WINDOW_FRACTION,
            None => false,
        },
        _ => bytes > threshold,
    };

    let level = if auto_critical {
        PressureLevel::Critical
    } else if fill >= PRESSURE_HIGH_FRACTION {
        PressureLevel::High
    } else if fill >= PRESSURE_ELEVATED_FRACTION {
        PressureLevel::Elevated
    } else {
        PressureLevel::Ok
    };
    ContextPressure {
        tokens,
        window,
        fill,
        level,
    }
}

/// The size a compaction pass cuts *to*, gathered by whoever owns the
/// history.
///
/// The same two numbers [`Measured`] judges pressure against, and they have to
/// be: a pass that cut to a ceiling unrelated to the one that triggered it is
/// how a run ends up compacting every other step.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    /// The provider's context window for this model, when it is known.
    pub window: Option<u32>,
    /// Byte ceiling the fallback measure is judged against when it is not.
    pub byte_threshold: usize,
}

impl Budget {
    /// Tokens the kept tail may carry: the low-water mark a pass cuts to.
    ///
    /// This exists because [`KEEP_RECENT`] is a *count*, and a count is not a
    /// size. Ten messages are 2 KB of conversation or 300 KB of tool results,
    /// so a pass that kept ten of them could finish with the history still
    /// over the [`COMPACT_WINDOW_FRACTION`] trigger — which fires compaction
    /// again on the very next step, and the step after that. One real session
    /// took 302 passes in 63 minutes that way; each one is an LLM call, and
    /// each one also rewrites the middle of the history and so throws away the
    /// provider's cached prefix.
    ///
    /// Cutting to 40% of a window that triggers at 80% makes the hysteresis
    /// structural rather than incidental: the conversation has to grow back
    /// through half the window before it can ask to be compacted again.
    fn low_water_tokens(self) -> u64 {
        match self.window {
            Some(window) if window > 0 => (f64::from(window) * COMPACT_LOW_WATER_FRACTION) as u64,
            // No window named, so the trigger is the byte ceiling itself
            // rather than a fraction of one. Same ratio off that instead,
            // converted at the char/4 estimate the rest of this module uses.
            _ => {
                let ratio = COMPACT_LOW_WATER_FRACTION / COMPACT_WINDOW_FRACTION;
                crate::llm::estimate_tokens_from_chars(
                    (self.byte_threshold as f64 * ratio) as usize,
                )
            }
        }
    }
}

/// What a compaction pass did, reported back so callers (auto-compaction
/// notices and the `/compact` command) can describe the outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactOutcome {
    /// Too little history between the system prompt and the recent tail.
    Nothing,
    /// Summarized `count` middle messages into one progress note.
    Summarized(usize),
    /// Nothing was worth summarizing, but `count` tool results that a later
    /// edit had superseded were replaced with stubs. See
    /// [`evict_superseded_reads`].
    Evicted(usize),
    /// The summary LLM failed, so `count` middle messages were dropped.
    Truncated { count: usize, error: String },
}

impl CompactOutcome {
    /// One-line notice describing what the pass did, shared by the
    /// auto-compaction events and the `/compact` command.
    pub fn describe(&self) -> String {
        fn messages(count: usize) -> String {
            if count == 1 {
                "1 message".to_string()
            } else {
                format!("{count} messages")
            }
        }
        match self {
            CompactOutcome::Nothing => "nothing to compact yet".to_string(),
            CompactOutcome::Summarized(count) => {
                format!("compacted {} into a summary", messages(*count))
            }
            CompactOutcome::Evicted(count) => {
                let results = if *count == 1 {
                    "1 stale tool result".to_string()
                } else {
                    format!("{count} stale tool results")
                };
                format!("elided {results}; nothing else to compact yet")
            }
            CompactOutcome::Truncated { count, error } => format!(
                "compacted {} by truncation (summary failed: {error})",
                messages(*count)
            ),
        }
    }
}

/// Where the kept tail is allowed to begin, which is the one rule a
/// conversation and a sub-loop do not share.
///
/// Both anchors exist to keep the *shrunken* request legal. A tool result
/// separated from the assistant message that asked for it is rejected by every
/// provider, and Anthropic additionally takes the system prompt as a top-level
/// field, so its adapter hoists every `Role::System` message out of the
/// history — which means the first message left standing has to be one the API
/// accepts as an opener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchor {
    /// The tail begins at a `Role::User` message.
    ///
    /// A real conversation has one every turn, so this both keeps tool-call
    /// groups whole and leaves a user message opening the request for free.
    Conversation,
    /// The tail begins at the nearest message that is not a tool result.
    ///
    /// A sub-loop's history has exactly *one* user message — the task at index
    /// 1 — and [`Anchor::Conversation`] would walk the boundary all the way
    /// back onto it and find nothing it was allowed to cut, so a sub-run that
    /// outgrew its window simply failed. Stopping at an assistant turn instead
    /// is safe for tool-call groups (the calls and their results go together),
    /// and the note that replaces the span becomes the user message that opens
    /// the request — see [`compact`].
    SubLoop,
}

impl Anchor {
    /// Whether the kept tail may start at `message`.
    fn may_begin_tail(self, message: &ChatMessage) -> bool {
        match self {
            Anchor::Conversation => message.role == Role::User,
            Anchor::SubLoop => message.role != Role::Tool,
        }
    }
}

/// What the summarization calls of one compaction pass were billed for.
///
/// A pass is one model call per [`COMPACT_CHUNK_CHARS`] of span, and for a
/// long time nobody was told about any of them: [`summarize_transcript`] holds
/// a provider, not an [`Agent`](super::Agent), so its tokens reached no
/// counter and no line of `~/.wizard/usage.jsonl`. On a run that compacted
/// every few steps that is not a rounding error, and the number it hid was
/// specifically the number a user checks to find out why compaction is
/// expensive. So the pass hands its bill back and the caller — which does have
/// the tracker and the log — records it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CompactUsage {
    pub prompt: u64,
    pub completion: u64,
    /// Prompt tokens served from the provider's cache, a *subset* of
    /// [`Self::prompt`] (see [`CacheTokens`]).
    pub cache_read: u64,
    /// Prompt tokens written into the provider's cache, also a subset.
    pub cache_write: u64,
}

impl CompactUsage {
    /// Whether any backend reported anything at all.
    pub fn reported(&self) -> bool {
        self.prompt > 0 || self.completion > 0
    }

    /// Fold in one model call's final chunk.
    fn add(&mut self, prompt: Option<u64>, completion: Option<u64>, cache: CacheTokens) {
        self.prompt = self.prompt.saturating_add(prompt.unwrap_or(0));
        self.completion = self.completion.saturating_add(completion.unwrap_or(0));
        self.cache_read = self.cache_read.saturating_add(cache.read);
        self.cache_write = self.cache_write.saturating_add(cache.write);
    }
}

/// A completed compaction pass: what it did, the note it wrote, and what it
/// cost.
#[derive(Debug, Clone)]
pub struct Compacted {
    pub outcome: CompactOutcome,
    /// The note that replaced the span, for a caller that persists one. `None`
    /// when nothing was cut, or when the summary failed and the span was
    /// dropped outright.
    pub note: Option<ChatMessage>,
    /// Tokens the pass spent summarizing. Non-zero even on the truncation
    /// path: a summary that failed halfway still billed for what it read.
    pub usage: CompactUsage,
}

impl Compacted {
    /// A pass that summarized nothing, having evicted `evicted` stale tool
    /// results on the way (usually zero).
    fn no_summary(evicted: usize) -> Self {
        Self {
            outcome: if evicted == 0 {
                CompactOutcome::Nothing
            } else {
                CompactOutcome::Evicted(evicted)
            },
            note: None,
            usage: CompactUsage::default(),
        }
    }
}

/// Cut `history` down to `budget`'s low-water mark: everything between the
/// system prompt and the kept tail becomes one progress note, unconditionally.
/// Callers decide *when* by consulting [`pressure`]; this decides *what*.
///
/// The tail is the newest messages that fit [`Budget::low_water_tokens`], and
/// never more than [`KEEP_RECENT`] of them, so a pass that runs because the
/// window is full leaves the window half empty and the next step has no reason
/// to run another one.
///
/// A summarization failure falls back to dropping the middle span, so this
/// never fails and never aborts a run: history that cannot be summarized is
/// still history that has to fit.
///
/// The note's role follows the message the tail now starts with: a system note
/// when that is a user message (the conversation case, where the note is
/// context *about* the transcript), and a user message when it is not, because
/// then the note is the only thing left that can open the request — see
/// [`Anchor`].
pub async fn compact(
    history: &mut Vec<ChatMessage>,
    anchor: Anchor,
    budget: Budget,
    client: &Arc<dyn LlmProvider>,
    model: &str,
) -> Compacted {
    // First, for free: results the transcript itself proves are out of date.
    // Done before the boundary is chosen so the tail is measured at the size
    // it will actually be sent at, and so the summarizer is not paid to read
    // a file's old contents.
    let evicted = evict_superseded_reads(history);

    let start = 1;
    let Some(end) = cut_boundary(history, anchor, budget.low_water_tokens()) else {
        return Compacted::no_summary(evicted);
    };
    let count = end - start;

    let mut usage = CompactUsage::default();
    match summarize_span(&history[start..end], client, model, &mut usage).await {
        Ok(summary) => {
            let text = format!("{COMPACT_SUMMARY_HEADING}\n{summary}");
            let note = if history[end].role == Role::User {
                ChatMessage::system(text)
            } else {
                ChatMessage::user(text)
            };
            history.splice(start..end, std::iter::once(note.clone()));
            Compacted {
                outcome: CompactOutcome::Summarized(count),
                note: Some(note),
                usage,
            }
        }
        Err(err) => {
            // Fall back to truncation: drop the middle span outright.
            history.drain(start..end);
            Compacted {
                outcome: CompactOutcome::Truncated {
                    count,
                    error: format!("{err:#}"),
                },
                note: None,
                usage,
            }
        }
    }
}

/// Exclusive end of the span [`compact`] replaces, or `None` when there is
/// nothing worth cutting. The span always starts at index 1, right after the
/// system prompt.
///
/// Two boundaries, and the cut goes to whichever is further:
///
/// * the token one, which keeps the newest messages that fit `low_water` and
///   is the binding constraint whenever the history is actually large, because
///   that is when a tail of ten messages stops being a small tail;
/// * the [`KEEP_RECENT`] one, which is what a forced `/compact` over a
///   half-empty window still folds away.
///
/// Then the [`Anchor`] rule moves it back to somewhere the tail is allowed to
/// begin, which is what keeps an assistant tool-call message with its results
/// and leaves the request with an opener the API accepts.
fn cut_boundary(history: &[ChatMessage], anchor: Anchor, low_water: u64) -> Option<usize> {
    const START: usize = 1;
    // Need history[0] (system prompt) + a non-empty middle + the recent tail.
    if history.len() <= KEEP_RECENT + START {
        return None;
    }

    let mut fits = history.len();
    let mut kept = 0u64;
    while fits > START {
        let with_next = kept.saturating_add(history[fits - 1].estimated_tokens());
        if with_next > low_water {
            break;
        }
        kept = with_next;
        fits -= 1;
    }

    // At least one message stays behind whatever the budget says: a single
    // message larger than the whole low-water mark would otherwise leave the
    // note standing alone, and the anchor walk below indexes `history[end]`.
    let mut end = fits.max(history.len() - KEEP_RECENT).min(history.len() - 1);
    while end > START && !anchor.may_begin_tail(&history[end]) {
        end -= 1;
    }
    if end <= START {
        return None;
    }

    // A span whose only real content is the note an earlier pass wrote is the
    // one span never worth summarizing. Every trip through the model loses a
    // little of a summary — that is what makes repeated passes decay into
    // 1.5 KB of vagueness — and here it would be spending a call to lose it,
    // since the handful of messages that joined the note since are smaller
    // than the note itself. Leaving them costs nothing measurable and keeps
    // the note verbatim, and it is also the last thing standing between a
    // pass and re-running on its own output.
    let (notes, fresh): (u64, u64) =
        history[START..end]
            .iter()
            .fold((0, 0), |(notes, fresh), m| {
                let tokens = m.estimated_tokens();
                if m.text().starts_with(COMPACT_SUMMARY_HEADING) {
                    (notes + tokens, fresh)
                } else {
                    (notes, fresh + tokens)
                }
            });
    if fresh < notes {
        return None;
    }
    Some(end)
}

/// Replace tool results that a later edit has provably superseded with a
/// one-line stub, and report how many were replaced.
///
/// A `read_file` result is a snapshot. Once a `write_file` or `edit_file`
/// later in the same history has changed that path, the snapshot is not merely
/// stale, it is *wrong*, and it keeps being re-sent on every step until a full
/// compaction pass happens to sweep it up. Eviction is the cheap version of
/// that sweep: it drops the bytes without spending a summarization call, and
/// it leaves the model a note saying where they went.
///
/// Deliberately conservative, because the cost of guessing wrong is deleting
/// context the model is relying on:
///
/// * only `read_file`, and only against `write_file` / `edit_file` on the
///   same path string. A shell command that rewrote the file is invisible
///   here and stays that way.
/// * only when the edit came in a *later* message than the read. Two calls in
///   one assistant turn are not ordered against each other.
/// * only for results big enough to be worth the cache invalidation
///   ([`STALE_RESULT_MIN_BYTES`]).
///
/// The stub replaces the result's *content*, never the block: a `tool_result`
/// separated from the `tool_use` that asked for it is a hard 400 from every
/// provider.
fn evict_superseded_reads(history: &mut [ChatMessage]) -> usize {
    /// Path argument of a call, normalized just enough that `./src/x.rs` and
    /// `src/x.rs` compare equal. Anything cleverer (canonicalizing, resolving
    /// against the project root) would start matching paths that only look
    /// alike, which is the direction that loses context.
    fn call_path(call: &ToolCall) -> Option<String> {
        let path = call
            .function
            .arguments
            .get("path")
            .and_then(Value::as_str)?;
        Some(path.trim().trim_start_matches("./").to_string())
    }

    // Last message index that wrote each path, and every read that might be
    // behind one, keyed by the call id its result carries.
    let mut edited: HashMap<String, usize> = HashMap::new();
    let mut reads: Vec<(usize, String, String)> = Vec::new();
    for (index, message) in history.iter().enumerate() {
        for call in message.tool_calls() {
            let Some(path) = call_path(call) else {
                continue;
            };
            match call.function.name.as_str() {
                "write_file" | "edit_file" => {
                    edited.insert(path, index);
                }
                "read_file" => reads.push((index, call.id.clone(), path)),
                _ => {}
            }
        }
    }
    if edited.is_empty() {
        return 0;
    }
    let stale: HashMap<&str, &str> = reads
        .iter()
        .filter(|(index, _, path)| edited.get(path).is_some_and(|edit| edit > index))
        .map(|(_, id, path)| (id.as_str(), path.as_str()))
        .collect();
    if stale.is_empty() {
        return 0;
    }

    let mut evicted = 0;
    for message in history.iter_mut() {
        for block in &mut message.content {
            let crate::llm::ContentBlock::ToolResult(result) = block else {
                continue;
            };
            let Some(path) = stale.get(result.tool_use_id.as_str()) else {
                continue;
            };
            if result.content.len() < STALE_RESULT_MIN_BYTES {
                continue;
            }
            let lines = result.content.lines().count();
            result.content = format!(
                "[read_file {path}: {lines} lines elided, superseded by a later edit to that file; read it again if you need it]"
            );
            evicted += 1;
        }
    }
    evicted
}

/// Summarize `span` with a rolling per-chunk pass, so an arbitrarily large
/// span is fully represented instead of hard-truncated: each chunk is
/// summarized together with the summary of everything before it.
async fn summarize_span(
    span: &[ChatMessage],
    client: &Arc<dyn LlmProvider>,
    model: &str,
    usage: &mut CompactUsage,
) -> Result<String> {
    // Render each message and pack into ~20k-char chunks, splitting oversized
    // messages at char boundaries.
    let mut chunks: Vec<String> = vec![String::new()];
    for msg in span {
        let role = match msg.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        let rendered = format!("{role}: {}\n", msg.text());
        let mut rest = rendered.as_str();
        while !rest.is_empty() {
            let chunk = chunks.last_mut().expect("at least one chunk");
            let room = COMPACT_CHUNK_CHARS.saturating_sub(chunk.len());
            if rest.len() <= room {
                chunk.push_str(rest);
                break;
            }
            if room == 0 {
                chunks.push(String::new());
                continue;
            }
            let mut cut = room;
            while cut > 0 && !rest.is_char_boundary(cut) {
                cut -= 1;
            }
            if cut == 0 {
                chunks.push(String::new());
                continue;
            }
            chunk.push_str(&rest[..cut]);
            rest = &rest[cut..];
            chunks.push(String::new());
        }
    }

    let mut summary: Option<String> = None;
    for chunk in &chunks {
        let blob = match &summary {
            None => chunk.clone(),
            Some(prev) => format!(
                "[Progress summary of the transcript so far]\n{prev}\n\n\
                 [Transcript continues]\n{chunk}"
            ),
        };
        summary = Some(summarize_transcript(&blob, client, model, usage).await?);
    }
    summary.ok_or_else(|| anyhow::anyhow!("nothing to summarize"))
}

/// Summarize a transcript blob into a terse progress note via the model.
/// Deltas are not forwarded to any UI; the token counts the backend reports
/// are folded into `usage` so the caller can bill them.
async fn summarize_transcript(
    blob: &str,
    client: &Arc<dyn LlmProvider>,
    model: &str,
    usage: &mut CompactUsage,
) -> Result<String> {
    let request = ChatRequest {
        model: model.to_string(),
        messages: vec![
            ChatMessage::system(
                "Summarize the following Wizard agent transcript into a compact progress \
                 note. Preserve: the mission/goal, decisions made, files changed, commands \
                 run, what worked/failed, and open next steps. Preserve the current todo \
                 list state verbatim (every item and its status) if one was maintained, \
                 and mention the plan file path (.wizard/plan.md) if a plan was written. \
                 Be terse and factual.",
            ),
            ChatMessage::user(blob.to_string()),
        ],
        tools: Vec::new(),
        stream: true,
        options: Some(ChatOptions {
            temperature: Some(0.2),
            num_ctx: None,
            // Internal summarization stays at the provider default; the user's
            // `/effort` applies to real turns, not compaction.
            reasoning_effort: None,
        }),
    };

    let mut stream = client
        .chat_stream(request)
        .await
        .context("starting compaction summary")?;
    let mut summary = String::new();
    loop {
        let Some(chunk) = tokio::time::timeout(SUMMARY_IDLE_TIMEOUT, stream.next())
            .await
            .map_err(|_elapsed| {
                anyhow::anyhow!(
                    "the compaction summary stream produced nothing for {}s",
                    SUMMARY_IDLE_TIMEOUT.as_secs()
                )
            })?
        else {
            break;
        };
        let chunk = chunk.context("reading compaction stream")?;
        // Billed before the reply is judged: an empty summary still cost
        // whatever the backend charged for reading the span.
        usage.add(chunk.prompt_eval_count, chunk.eval_count, chunk.cache);
        if let Some(message) = chunk.message
            && !chunk.thinking
        {
            summary.push_str(&message.text());
        }
        if chunk.done {
            break;
        }
    }
    if summary.trim().is_empty() {
        anyhow::bail!("empty summary");
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::llm::{ChatChunk, ChatStream, ToolCall};

    /// The window every budgeted test measures against, and the mark a pass
    /// cuts to.
    const WINDOW: u32 = 100_000;
    const LOW_WATER: u64 = 40_000;

    /// Provider that answers every summarization call with the same note and
    /// the same bill.
    struct StubSummarizer;

    /// What [`StubSummarizer`] reports for one call.
    const STUB_PROMPT: u64 = 1_200;
    const STUB_COMPLETION: u64 = 80;
    const STUB_CACHE: CacheTokens = CacheTokens {
        read: 900,
        write: 100,
    };

    #[async_trait::async_trait]
    impl LlmProvider for StubSummarizer {
        async fn health(&self) -> Result<()> {
            Ok(())
        }

        async fn supports_native_tools(&self, _model: &str) -> Result<bool> {
            Ok(true)
        }

        async fn list_models(&self) -> Result<Vec<String>> {
            Ok(Vec::new())
        }

        async fn chat_stream(&self, _request: ChatRequest) -> Result<ChatStream> {
            let chunk = ChatChunk {
                message: Some(ChatMessage::assistant("everything so far")),
                images: Vec::new(),
                thinking: false,
                done: true,
                done_reason: None,
                eval_count: Some(STUB_COMPLETION),
                prompt_eval_count: Some(STUB_PROMPT),
                cache: STUB_CACHE,
            };
            Ok(Box::pin(futures_util::stream::once(
                async move { Ok(chunk) },
            )))
        }

        fn label(&self) -> String {
            "stub".to_string()
        }
    }

    fn stub() -> Arc<dyn LlmProvider> {
        Arc::new(StubSummarizer)
    }

    fn budget() -> Budget {
        Budget {
            window: Some(WINDOW),
            byte_threshold: 48_000,
        }
    }

    /// A history of `count` user messages of `tokens` each, behind a system
    /// prompt. Every message is a legal tail start under either anchor, so
    /// what a pass does to it is decided by size alone.
    fn history_of(count: usize, tokens: usize) -> Vec<ChatMessage> {
        let mut history = vec![ChatMessage::system("you are wizard")];
        for index in 0..count {
            history.push(ChatMessage::user(format!(
                "{index} {}",
                "x".repeat(tokens * 4)
            )));
        }
        history
    }

    /// The whole point of the low-water mark: a pass triggered at 80% of the
    /// window leaves the history near 40%, so the next step has no reason to
    /// run another one. Ten messages is not a size, and ten messages this big
    /// would have left the history over the trigger it just fired on.
    #[tokio::test]
    async fn a_pass_cuts_to_the_low_water_mark_and_the_next_one_finds_nothing() {
        // 12 messages of 10k tokens: 120k against a 100k window.
        let mut history = history_of(12, 10_000);
        let before = crate::llm::estimate_history_tokens(&history);
        assert!(before > u64::from(WINDOW), "the history is over the window");

        let first = compact(
            &mut history,
            Anchor::Conversation,
            budget(),
            &stub(),
            "model",
        )
        .await;
        assert!(
            matches!(first.outcome, CompactOutcome::Summarized(_)),
            "{:?}",
            first.outcome
        );
        let after = crate::llm::estimate_history_tokens(&history);
        assert!(
            after <= LOW_WATER + 1_000,
            "cut to the low-water mark, not to a message count: {after} tokens"
        );
        assert!(
            history.len() < 1 + KEEP_RECENT,
            "the tail is shorter than the message reserve, because the tokens bind: {}",
            history.len()
        );

        // The anti-thrash property: nothing is left that a second pass would
        // spend another model call on.
        let second = compact(
            &mut history,
            Anchor::Conversation,
            budget(),
            &stub(),
            "model",
        )
        .await;
        assert_eq!(second.outcome, CompactOutcome::Nothing);
        assert_eq!(
            second.usage,
            CompactUsage::default(),
            "and it billed nothing"
        );
    }

    /// The degenerate case the note itself creates: a history whose cuttable
    /// span is the summary an earlier pass wrote plus a couple of short
    /// messages. Re-summarizing a summary costs a call and loses a little of
    /// it every time, so a pass declines instead and the note is carried
    /// through verbatim.
    #[tokio::test]
    async fn a_span_that_is_mostly_an_earlier_note_is_left_alone() {
        let mut history = vec![
            ChatMessage::system("you are wizard"),
            ChatMessage::system(format!(
                "{COMPACT_SUMMARY_HEADING}\nwhat happened so far: {}",
                "detail, ".repeat(100)
            )),
        ];
        for index in 0..KEEP_RECENT + 3 {
            history.push(ChatMessage::user(format!("tail {index}")));
        }

        let compacted = compact(
            &mut history,
            Anchor::Conversation,
            budget(),
            &stub(),
            "model",
        )
        .await;
        assert_eq!(compacted.outcome, CompactOutcome::Nothing);
        assert!(
            history[1].text().contains("what happened so far"),
            "the note survived verbatim rather than going through the model again"
        );
    }

    /// Every summarization call is billed back to the caller. Nothing here
    /// can record usage itself — that is the point of the module — so a pass
    /// that did not hand its bill back would spend tokens that appear in no
    /// counter and no line of `usage.jsonl`.
    #[tokio::test]
    async fn a_pass_reports_what_its_summaries_cost() {
        let mut history = history_of(12, 10_000);
        let compacted = compact(
            &mut history,
            Anchor::Conversation,
            budget(),
            &stub(),
            "model",
        )
        .await;

        // One call per chunk of span; the span here is far past
        // `COMPACT_CHUNK_CHARS`, so the count is what the rolling pass made.
        let calls = compacted.usage.prompt / STUB_PROMPT;
        assert!(calls >= 2, "a large span is summarized in chunks: {calls}");
        assert_eq!(compacted.usage.completion, calls * STUB_COMPLETION);
        assert_eq!(compacted.usage.cache_read, calls * STUB_CACHE.read);
        assert_eq!(compacted.usage.cache_write, calls * STUB_CACHE.write);
        assert!(compacted.usage.reported());
    }

    /// A `read_file` result for a file edited later is not stale, it is
    /// wrong, and it rides along on every step until something removes it.
    /// What must survive is the pairing: the stub replaces the result's
    /// content, never the block.
    #[test]
    fn a_read_superseded_by_a_later_edit_becomes_a_stub() {
        fn call(name: &str, path: &str) -> (ChatMessage, String) {
            let call = ToolCall::new(name, json!({ "path": path }));
            let id = call.id.clone();
            let mut message = ChatMessage::assistant("");
            message.push_tool_call(call);
            (message, id)
        }

        let (read, read_id) = call("read_file", "src/x.rs");
        let (edit, edit_id) = call("edit_file", "./src/x.rs");
        let (other, other_id) = call("read_file", "src/y.rs");
        let (again, again_id) = call("read_file", "src/x.rs");
        let body = "line\n".repeat(200);
        let mut history = vec![
            ChatMessage::system("you are wizard"),
            read,
            ChatMessage::tool_result(&read_id, "read_file", &body),
            other,
            ChatMessage::tool_result(&other_id, "read_file", &body),
            edit,
            ChatMessage::tool_result(&edit_id, "edit_file", "edited"),
            again,
            ChatMessage::tool_result(&again_id, "read_file", &body),
        ];

        assert_eq!(evict_superseded_reads(&mut history), 1);
        let stub = history[2].tool_results()[0];
        assert_eq!(
            stub.tool_use_id, read_id,
            "the block still answers its call"
        );
        assert!(stub.content.contains("src/x.rs"), "{}", stub.content);
        assert!(
            stub.content.contains("200 lines elided"),
            "{}",
            stub.content
        );
        assert_eq!(
            history[4].tool_results()[0].content,
            body,
            "a read of a file nobody edited is untouched"
        );
        assert_eq!(
            history[8].tool_results()[0].content,
            body,
            "and so is a read taken after the edit"
        );

        assert_eq!(
            evict_superseded_reads(&mut history),
            0,
            "a second sweep finds nothing: the stub is already below the size floor"
        );
    }

    /// The mark is half the trigger whichever measure is in play, so the
    /// hysteresis does not disappear on a backend that names no window.
    #[test]
    fn the_low_water_mark_is_half_the_trigger() {
        let known = Budget {
            window: Some(200_000),
            byte_threshold: 48_000,
        };
        assert_eq!(known.low_water_tokens(), 80_000);
        assert!(
            (known.low_water_tokens() as f64) < f64::from(200_000u32) * COMPACT_WINDOW_FRACTION,
            "a pass has to leave the history under the level that triggered it"
        );

        let unknown = Budget {
            window: None,
            byte_threshold: 48_000,
        };
        assert_eq!(
            unknown.low_water_tokens(),
            crate::llm::estimate_tokens_from_chars(24_000),
            "half the byte ceiling, at the same char/4 estimate"
        );
    }

    /// A sub-loop's history: system prompt, the task, then assistant/tool
    /// pairs and nothing else. `Anchor::Conversation` has exactly one user
    /// message to aim at — the task — and it is `start`, so the boundary walks
    /// all the way back and there is nothing left to cut. That is the shape
    /// that used to make a long subagent run un-compactable, and it is
    /// entirely invisible on a parent conversation.
    #[test]
    fn a_sub_loops_history_offers_a_conversation_anchor_no_boundary_at_all() {
        let mut history = vec![
            ChatMessage::system("you are a worker"),
            ChatMessage::user("the task"),
        ];
        for step in 0..8 {
            history.push(ChatMessage::assistant(format!("step {step}")));
            history.push(ChatMessage::tool_result("id", "probe", "output"));
        }
        let naive = history.len() - KEEP_RECENT;
        assert!(naive > 1, "there is a middle span to cut");

        let mut end = naive;
        while end > 1 && !Anchor::Conversation.may_begin_tail(&history[end]) {
            end -= 1;
        }
        assert_eq!(end, 1, "the only user message is the task at index 1");

        let mut end = naive;
        while end > 1 && !Anchor::SubLoop.may_begin_tail(&history[end]) {
            end -= 1;
        }
        assert!(end > 1, "the sub-loop anchor finds a boundary: {end}");
        assert_ne!(
            history[end].role,
            Role::Tool,
            "and never one that orphans a tool result"
        );
    }

    /// The reported prompt size is what trips auto-compaction, and only
    /// against a window the provider actually named. An estimate must not,
    /// or the system prompt alone would compact a fresh session.
    #[test]
    fn only_a_reported_prompt_against_a_known_window_is_critical() {
        let known = Measured {
            tokens: 90_000,
            window: Some(100_000),
            bytes: 10,
            byte_threshold: 48_000,
            last_prompt: Some(90_000),
        };
        assert_eq!(pressure(known).level, PressureLevel::Critical);
        assert_eq!(
            pressure(Measured {
                last_prompt: None,
                ..known
            })
            .level,
            PressureLevel::High,
            "an estimate reaches the soft bands and stops there"
        );
        assert_eq!(
            pressure(Measured {
                bytes: 100_000,
                window: None,
                ..known
            })
            .level,
            PressureLevel::Critical,
            "with no window the byte proxy is the gate"
        );
    }
}
