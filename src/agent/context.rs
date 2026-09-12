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
use crate::llm::{
    CacheTokens, ChatMessage, ChatOptions, ChatRequest, Role, ToolCall, ToolResultBlock,
};

/// Most-recent messages a compaction pass keeps at the outside.
///
/// A ceiling, and no longer the rule. [`Budget::low_water_tokens`] is what
/// decides how much of the tail survives, and it cuts deeper than this
/// whenever ten messages are too many tokens — which is the case that
/// mattered. This stays because it is also what a `/compact` over a half-empty
/// window folds away: under the low-water mark there is nothing the budget
/// wants cut, and the request is still to cut something.
pub(crate) const KEEP_RECENT: usize = 10;

/// Tool results kept in something like full, counted in results rather than in
/// messages: everything older collapses to [`digest_line`].
///
/// [`KEEP_RECENT`] is the reserve nothing touches at all, and it is a message
/// count, so a batch of five calls uses it up in one step. This is the second
/// band, and it is counted in results because that is the thing being kept:
/// twelve is a couple of dozen steps of ordinary work, far enough back that a
/// result the model has not referred to since is one it is not coming back to,
/// and near enough that the file it read four calls ago is still there whole.
pub(crate) const KEEP_WHOLE_RESULTS: usize = 12;

/// Bytes a tool result outside [`KEEP_WHOLE_RESULTS`] has to be carrying
/// before it is worth replacing with a [`digest_line`].
///
/// The digest is itself about 150 characters, so collapsing a 300-character
/// result would save nothing and lose what it said. Most `execute` results in
/// a real session are under this: a short command and a short answer, which
/// stay verbatim however far back they are.
const DIGEST_MIN_CHARS: usize = 500;

/// Characters one pass has to reclaim before it rewrites anything.
///
/// Every rewrite mid-history throws away the provider's cached prefix from
/// that point on, so a pass that runs between compactions has to be worth that
/// much. Without a floor the newest result crossing the [`KEEP_RECENT`]
/// boundary would trigger a rewrite on every single step, paying a full
/// re-prefill of the tail to reclaim a few hundred characters. 16k characters
/// is about 4k tokens, comfortably more than the tail a rewrite costs.
///
/// A compaction pass passes `0`: it was going to invalidate that prefix
/// anyway.
pub(crate) const MIN_RECLAIM_CHARS: usize = 16_384;

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

/// Characters one tool result outside the recent window may carry before
/// [`shrink_old_results`] cuts it down to a head/tail excerpt.
///
/// This is the number that makes a compaction pass cheap, or unnecessary. The
/// summarizer is billed for every character of the span it reads, and on a
/// working session that span is overwhelmingly tool output: a `cargo test`
/// run, a large source file read whole, a directory listing. What a model
/// comes back to in that output is the start (what was asked) and the end
/// (what it found, how it ended); the middle is the part nobody re-reads. So
/// the middle can go for free, before any model is asked to read it, and the
/// pass either summarizes a much smaller span or finds it has nothing left to
/// do.
///
/// 8192 is deliberately generous. A tool result already arrives capped at
/// [`MAX_OUTPUT_BYTES`](crate::tools) (30 KB), so this is roughly a quarter of
/// the worst case and several times what any surface renders of a result. A
/// result under it is one the model can still read end to end, which is the
/// property worth keeping: this is not a place to be clever about what matters,
/// because the cost of guessing wrong is context the model was relying on.
const PRUNE_RESULT_MAX_CHARS: usize = 8_192;

/// Left in place of the middle of a pruned tool result.
///
/// A fixed string rather than a formatted one, and that is what makes pruning
/// idempotent. [`prune_excerpt`] budgets the head and the tail against
/// this marker's length to land the excerpt on exactly
/// [`PRUNE_RESULT_MAX_CHARS`]; a marker whose length depended on how much it
/// elided would make that accounting circular, and an excerpt that came out a
/// few characters over the threshold would be re-pruned by the next pass, and
/// the one after that, each time losing a little more of a result the model
/// can no longer recover.
const PRUNE_OMISSION_MARKER: &str = "\n\n[... middle of this tool result elided \
     to reclaim context; run the tool again if you need the part that is \
     missing ...]\n\n";

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

/// The window a run manages itself against: the provider's, capped by
/// `max_tokens` (`0` means no cap).
///
/// Both [`pressure`] and [`Budget`] measure against a *fraction* of the
/// window, which is the right shape for a 32k local model and the wrong one
/// for grok-4.6, where 80% of 500k is 400k tokens and the trigger simply never
/// arrives: over 89 benchmark trials no run ever compacted, and the biggest
/// per-call prompt was 105k. A prompt that large costs real prefill on every
/// step and is where long-context answers start coming back garbled, so the
/// cap is what makes the fraction mean something again. 150k caps the default
/// trigger at 120k tokens: under every window Wizard is pointed at often
/// enough to matter (128k on GPT-class models, 200k on Claude) the fraction
/// still binds, and above that this does.
pub fn effective_window(window: Option<u32>, max_tokens: u32) -> Option<u32> {
    match window {
        Some(window) if max_tokens > 0 => Some(window.min(max_tokens)),
        other => other,
    }
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
    /// No summarization call was made, but `count` tool results were cut down
    /// mechanically on the way: results a later edit had superseded, replaced
    /// with stubs ([`evict_superseded_reads`]), and oversized results outside
    /// the recent window, cut to a head/tail excerpt ([`shrink_old_results`]).
    ///
    /// One variant for both because both are the same event from every
    /// caller's side: the context shrank, it is worth a notice and not an
    /// error, and the pass cost nothing. A pass that reports this either found
    /// nothing left worth summarizing or found the mechanical passes had
    /// already brought the history under the mark a summary would have cut to,
    /// which is the outcome worth having, because the reclaimed context is
    /// free.
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
                    "1 oversized or stale tool result".to_string()
                } else {
                    format!("{count} oversized or stale tool results")
                };
                format!("elided {results}; no summarization call needed")
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
/// accepts as an opener. [`compact`] picks the note's role from whatever the
/// tail now starts with, so a tail that begins on an assistant turn still
/// opens with a user message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchor {
    /// The tail begins at a user or assistant message.
    ///
    /// User-only used to look sufficient: a finished turn has a user message
    /// every few entries, so walking back from the token cut landed nearby
    /// and the in-flight tool results stayed grouped. Mid-turn that user is
    /// the prompt at the start of the history, and the walk went all the
    /// way back onto it. One live session then spent an hour summarizing a
    /// single note 69 times while the fat tool tail (and the elevated
    /// pressure that made the model call `compact` again) never moved.
    /// Stopping on an assistant turn keeps the tool-call group whole and
    /// actually cuts to the budget.
    Conversation,
    /// The tail begins at the nearest message that is not a tool result.
    ///
    /// A sub-loop's history has exactly *one* user message — the task at index
    /// 1, so even the conversation rule above would sometimes rather start
    /// there. Allowing a system note as well lets a sub-run that already
    /// compacted keep that note as the opener instead of walking onto the
    /// task and finding nothing to cut.
    SubLoop,
}

impl Anchor {
    /// Whether the kept tail may start at `message`.
    fn may_begin_tail(self, message: &ChatMessage) -> bool {
        match self {
            // Not a tool result: that would orphan it from the assistant
            // message that asked for it. System notes are skipped so a
            // previous progress note is folded into the next summary
            // rather than becoming the tail opener.
            Anchor::Conversation => message.role == Role::User || message.role == Role::Assistant,
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
    /// A pass that summarized nothing, having cut `elided` tool results down
    /// mechanically on the way (usually zero). See
    /// [`CompactOutcome::Evicted`].
    fn no_summary(elided: usize) -> Self {
        Self {
            outcome: if elided == 0 {
                CompactOutcome::Nothing
            } else {
                CompactOutcome::Evicted(elided)
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
///
/// # The free passes run first
///
/// [`evict_superseded_reads`] and [`shrink_old_results`] both reclaim context
/// with no model call at all, so both run before the boundary is chosen. That
/// ordering is worth two things. The span the summarizer reads is smaller, so
/// the call it is billed for is cheaper; and when the two of them alone bring
/// the history to the low-water mark this pass was aiming for, there is
/// nothing left for a summary to reclaim and the call is skipped outright.
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
    // Then, still for free: the middles of tool results too large to be worth
    // carrying whole.
    let pruned = shrink_old_results(history, KEEP_WHOLE_RESULTS, 0);

    // The payoff. Pruning is gated on a per-result size and knows nothing
    // about the window, so it is perfectly capable of reclaiming more than
    // this pass needed; when it has, the summarization call would be spent
    // cutting a history that already fits. Gated on `pruned` rather than on
    // the mechanical passes generally, because eviction has always run ahead
    // of an unconditional cut and a forced `/compact` still means "cut
    // something".
    if pruned > 0 && crate::llm::estimate_history_tokens(history) <= budget.low_water_tokens() {
        return Compacted::no_summary(evicted + pruned);
    }

    let start = 1;
    let Some(end) = cut_boundary(history, anchor, budget.low_water_tokens()) else {
        return Compacted::no_summary(evicted + pruned);
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
/// begin, which is what keeps an assistant tool-call message with its results.
/// Mid-turn that is the nearest assistant message, not the user prompt at the
/// start of the history: walking onto that user is how a pass used to
/// summarize one note and leave the tool tail (and the pressure) untouched.
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

/// Shrink the tool results this history no longer needs whole, and report how
/// many it rewrote.
///
/// The mechanical half of compaction, and the half that costs nothing. Three
/// bands, newest to oldest:
///
/// * the last [`KEEP_RECENT`] messages: untouched. Those are the results of
///   the calls the model just made, so they are the ones it is most likely
///   still working from, and [`cut_boundary`] refuses to summarize past them
///   for the same reason.
/// * older than that, but inside the last `keep_whole` results: cut to a
///   head/tail excerpt ([`prune_excerpt`]). The model can still read what was
///   asked and how it ended.
/// * older than `keep_whole` results: one [`digest_line`] naming the tool, its
///   subject, and whether it succeeded.
///
/// Nothing is rewritten at all unless the whole pass reclaims `min_reclaim`
/// characters, because each rewrite costs the provider's cached prefix from
/// that point on (see [`MIN_RECLAIM_CHARS`]). A compaction pass passes `0`:
/// that prefix was going already.
///
/// Like [`evict_superseded_reads`] this replaces the result's *content*, never
/// the block: a `tool_result` separated from the `tool_use` that asked for it
/// is a hard 400 from every provider.
///
/// None of it reaches the transcript on disk. A session file is append-only
/// ([`session::Session::append`](super::session)): each result was written
/// whole as it landed and no later pass rewrites those lines, so what is cut
/// here is cut from what the model is sent and from nothing else. The full
/// result is still in `~/.wizard/sessions/`, which is what makes this safe to
/// do without asking anyone.
pub(crate) fn shrink_old_results(
    history: &mut [ChatMessage],
    keep_whole: usize,
    min_reclaim: usize,
) -> usize {
    let recent = history.len().saturating_sub(KEEP_RECENT);
    let results: usize = history
        .iter()
        .map(|message| message.tool_results().len())
        .sum();
    let digest_before = results.saturating_sub(keep_whole);

    // Planned first, applied second, and not only to keep the borrow checker
    // happy about reading the calls while writing the results: a pass that
    // reclaims too little must not rewrite anything at all, and that is not
    // known until every candidate has been costed.
    let mut planned: Vec<(usize, usize, String)> = Vec::new();
    let mut reclaimed = 0usize;
    {
        let calls = calls_by_id(history);
        let mut seen = 0usize;
        for (index, message) in history.iter().enumerate() {
            for (block_index, block) in message.content.iter().enumerate() {
                let crate::llm::ContentBlock::ToolResult(result) = block else {
                    continue;
                };
                let ordinal = seen;
                seen += 1;
                if index >= recent {
                    continue;
                }
                let before = result.content.chars().count();
                let shrunk = if ordinal < digest_before {
                    if before < DIGEST_MIN_CHARS {
                        continue;
                    }
                    digest_line(result, calls.get(result.tool_use_id.as_str()).copied())
                } else {
                    match prune_excerpt(&result.content) {
                        Some(excerpt) => excerpt,
                        None => continue,
                    }
                };
                let after = shrunk.chars().count();
                if after >= before {
                    continue;
                }
                reclaimed += before - after;
                planned.push((index, block_index, shrunk));
            }
        }
    }

    if reclaimed < min_reclaim {
        return 0;
    }
    let shrunk = planned.len();
    for (index, block_index, content) in planned {
        if let crate::llm::ContentBlock::ToolResult(result) =
            &mut history[index].content[block_index]
        {
            result.content = content;
        }
    }
    shrunk
}

/// Every tool call in `history`, by the id its result carries.
fn calls_by_id(history: &[ChatMessage]) -> HashMap<&str, &ToolCall> {
    history
        .iter()
        .flat_map(ChatMessage::tool_calls)
        .map(|call| (call.id.as_str(), call))
        .collect()
}

/// The one line left in place of a tool result too far back to carry.
///
/// Says what ran, what it was about, and how it ended, because the two things
/// a model comes back to an old result for are "did that work" and "what did I
/// call it on". Whether it worked is read off the result itself: a failure
/// arrives prefixed `Error:` (turn.rs `result_body`), and
/// the digest has to keep saying so: a result that succeeded and then got
/// elided must not read like one that failed, and a result that failed must
/// not read like one that succeeded.
fn digest_line(result: &ToolResultBlock, call: Option<&ToolCall>) -> String {
    let status = if result.content.starts_with("Error: ") {
        "error"
    } else {
        "ok"
    };
    let lines = result.content.lines().count();
    let subject = call.and_then(call_subject).unwrap_or_default();
    format!(
        "[{}{subject}: {status}, {lines} lines elided to reclaim context. The full output is \
         in this session's transcript; run it again if you need it.]",
        result.name
    )
}

/// The argument of `call` worth naming in a digest (` `cargo test` `), or
/// `None` when it has no obvious one.
fn call_subject(call: &ToolCall) -> Option<String> {
    const NAMED: [&str; 5] = ["command", "path", "pattern", "query", "url"];
    const MAX_CHARS: usize = 60;
    let subject = NAMED.iter().find_map(|key| {
        call.function
            .arguments
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    })?;
    let mut short: String = subject.chars().take(MAX_CHARS).collect();
    if subject.chars().nth(MAX_CHARS).is_some() {
        short.push_str("...");
    }
    Some(format!(" `{}`", short.replace('\n', " ")))
}

/// `content` cut to a bounded head, [`PRUNE_OMISSION_MARKER`], and a bounded
/// tail, or `None` when it already fits [`PRUNE_RESULT_MAX_CHARS`].
///
/// Two properties this has to hold, because a compaction pass may run over the
/// same history any number of times and each trip through here is
/// unrecoverable:
///
/// * an excerpt is *exactly* [`PRUNE_RESULT_MAX_CHARS`] characters, so a
///   second pass over it takes the `None` branch and changes nothing. That is
///   what the fixed marker buys: the head and the tail are budgeted against
///   its known length rather than against a count of what they omitted.
/// * an excerpt is strictly smaller than the content that produced it, since
///   this only runs on content strictly over the threshold. A pruning pass
///   therefore always terminates and never grows a result.
///
/// Counted and cut in `char`s, never bytes, so a multibyte result cannot be
/// split down the middle of a code point and arrive at the provider as
/// mojibake. The same care [`truncate_output`](crate::tools) takes, for the
/// same reason. The tail gets three quarters of the budget because a tool
/// result ends in its conclusion: the failing assertion, the exit status, the
/// summary line.
fn prune_excerpt(content: &str) -> Option<String> {
    let total = content.chars().count();
    if total <= PRUNE_RESULT_MAX_CHARS {
        return None;
    }
    let marker = PRUNE_OMISSION_MARKER.chars().count();
    // A marker wider than the budget it is being fitted into would make the
    // "excerpt" longer than the threshold, which breaks both properties above.
    // Unreachable at the constants this file ships, and cheaper to rule out
    // than to reason about if either of them ever moves.
    if marker >= PRUNE_RESULT_MAX_CHARS {
        return None;
    }
    let budget = PRUNE_RESULT_MAX_CHARS - marker;
    let head = budget / 4;
    let tail = budget - head;

    // Byte offsets of char boundaries, so the slices below cannot land inside
    // a code point. `total > PRUNE_RESULT_MAX_CHARS` guarantees the tail
    // starts after the head ends, so the two never overlap.
    let boundary = |index: usize| {
        content
            .char_indices()
            .nth(index)
            .map_or(content.len(), |(at, _)| at)
    };
    let head_end = boundary(head);
    let tail_start = boundary(total - tail);
    Some(format!(
        "{}{PRUNE_OMISSION_MARKER}{}",
        &content[..head_end],
        &content[tail_start..]
    ))
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

    /// Mid-turn: one user prompt, then a long assistant/tool loop whose
    /// results overshoot the low-water mark. Walking the cut back onto that
    /// user used to make the span a single earlier note and leave the fat
    /// tail (and the pressure that made the model call `compact`) untouched.
    /// One live session then spent an hour summarizing one message 69 times.
    #[tokio::test]
    async fn a_mid_turn_pass_cuts_the_tool_loop_instead_of_one_earlier_note() {
        let mut history = vec![
            ChatMessage::system("you are wizard"),
            ChatMessage::system(format!(
                "{COMPACT_SUMMARY_HEADING}\nwhat happened so far: {}",
                "detail, ".repeat(100)
            )),
            ChatMessage::user("keep going"),
        ];
        for step in 0..12 {
            history.push(ChatMessage::assistant(format!("step {step}")));
            // 10k tokens each, so a handful fill the 40k low-water mark and
            // the leftover tail is shorter than KEEP_RECENT. Smaller results
            // would leave a second pass something the message-count rule
            // still wants to fold.
            history.push(ChatMessage::tool_result(
                format!("id{step}"),
                "probe",
                "x".repeat(40_000),
            ));
        }
        let before = history.len();

        let compacted = compact(
            &mut history,
            Anchor::Conversation,
            budget(),
            &stub(),
            "model",
        )
        .await;
        match compacted.outcome {
            CompactOutcome::Summarized(count) => {
                assert!(
                    count > 1,
                    "a mid-turn pass has to fold more than the earlier note: {count}"
                );
            }
            other => panic!("expected a real cut, got {other:?}"),
        }
        let after = crate::llm::estimate_history_tokens(&history);
        assert!(
            after <= LOW_WATER + 1_000,
            "the tool tail was cut to the budget, not left standing: {after}"
        );

        let assistant = history
            .iter()
            .rposition(|m| m.role == Role::Assistant)
            .expect("an assistant turn survived in the tail");
        assert_eq!(
            history[assistant + 1].role,
            Role::Tool,
            "the in-flight tool-call group stayed paired"
        );
        assert!(
            history.len() < before,
            "history shrank: {before} → {}",
            history.len()
        );

        // The anti-thrash property still holds: a second pass does not spend
        // another call re-summarizing the note it just wrote.
        let second = compact(
            &mut history,
            Anchor::Conversation,
            budget(),
            &stub(),
            "model",
        )
        .await;
        assert_eq!(second.outcome, CompactOutcome::Nothing);
        assert_eq!(second.usage, CompactUsage::default());
    }

    /// Conversation will not open the tail on a system note, so a previous
    /// progress note is folded into the next summary. SubLoop will, because
    /// a sub-run that already compacted has that note as the only opener it
    /// can keep without walking onto the task.
    #[test]
    fn conversation_skips_a_system_note_that_a_sub_loop_would_keep() {
        let history = [
            ChatMessage::system("you are a worker"),
            ChatMessage::user("the task"),
            ChatMessage::system(format!("{COMPACT_SUMMARY_HEADING}\nso far")),
            ChatMessage::assistant("step"),
            ChatMessage::tool_result("id", "probe", "output"),
        ];
        let note = 2;

        let mut end = note;
        while end > 1 && !Anchor::Conversation.may_begin_tail(&history[end]) {
            end -= 1;
        }
        assert_eq!(
            history[end].role,
            Role::User,
            "conversation walks past the note onto the user"
        );

        let mut end = note;
        while end > 1 && !Anchor::SubLoop.may_begin_tail(&history[end]) {
            end -= 1;
        }
        assert_eq!(
            history[end].role,
            Role::System,
            "a sub-loop can open on the note it already wrote"
        );
        assert!(
            history[end].text().starts_with(COMPACT_SUMMARY_HEADING),
            "{}",
            history[end].text()
        );
    }

    /// A history in the shape a tool loop leaves behind: the system prompt,
    /// then paired assistant tool-call and tool-result messages. Results
    /// landing before the [`KEEP_RECENT`] reserve carry `old`, results inside
    /// it carry `recent`, so a test says in one line which half it wants fat.
    fn tool_loop_history(old: &str, recent: &str) -> Vec<ChatMessage> {
        const STEPS: usize = 10;
        let reserve = (1 + STEPS * 2) - KEEP_RECENT;
        let mut history = vec![ChatMessage::system("you are wizard")];
        for step in 0..STEPS {
            history.push(ChatMessage::assistant(format!("step {step}")));
            // `len()` before the push is the index this result will occupy.
            let body = if history.len() < reserve { old } else { recent };
            history.push(ChatMessage::tool_result(format!("id{step}"), "probe", body));
        }
        history
    }

    /// A tool result several times the per-result budget.
    fn oversized() -> String {
        "y".repeat(PRUNE_RESULT_MAX_CHARS * 5)
    }

    /// Pruning has to be a fixed point after one application, because a
    /// compaction pass can run over the same history any number of times and
    /// every trip through the excerpt is unrecoverable. So an excerpt lands
    /// *at* the threshold rather than near it: strictly smaller than what
    /// triggered the cut, and small enough that the next pass declines.
    #[test]
    fn pruning_a_second_time_changes_nothing() {
        let body = oversized();
        let mut history = tool_loop_history(&body, &body);
        assert_eq!(shrink_old_results(&mut history, KEEP_WHOLE_RESULTS, 0), 5);

        let reserve = history.len() - KEEP_RECENT;
        for result in history[..reserve]
            .iter()
            .flat_map(ChatMessage::tool_results)
        {
            let length = result.content.chars().count();
            assert_eq!(
                length, PRUNE_RESULT_MAX_CHARS,
                "an excerpt lands exactly on the threshold"
            );
            assert!(
                length < body.chars().count(),
                "and is strictly smaller than the result that triggered the cut"
            );
            assert!(result.content.contains(PRUNE_OMISSION_MARKER));
        }

        let settled: Vec<_> = history.iter().map(|m| m.content.clone()).collect();
        assert_eq!(
            shrink_old_results(&mut history, KEEP_WHOLE_RESULTS, 0),
            0,
            "a second pass finds nothing over the threshold"
        );
        for (message, before) in history.iter().zip(&settled) {
            assert_eq!(&message.content, before, "and rewrote nothing");
        }
    }

    /// The reserve is the one notion of "recent" this module has: the results
    /// of the calls the model just made are the ones it is most likely still
    /// working from, and [`cut_boundary`] already refuses to summarize past
    /// them. Pruning stops in the same place.
    #[test]
    fn the_recent_reserve_is_not_pruned() {
        let body = oversized();
        let mut history = tool_loop_history(&body, &body);
        let reserve = history.len() - KEEP_RECENT;

        shrink_old_results(&mut history, KEEP_WHOLE_RESULTS, 0);

        for result in history[reserve..]
            .iter()
            .flat_map(ChatMessage::tool_results)
        {
            assert_eq!(
                result.content, body,
                "a result inside the reserve is carried whole"
            );
        }
    }

    /// A result is cut in `char`s, never bytes: a cut through the middle of a
    /// code point would reach the provider as mojibake, and the tail of a tool
    /// result (the failing assertion, the exit status) is the half worth
    /// keeping.
    #[test]
    fn a_multibyte_result_is_cut_on_char_boundaries() {
        let content = format!("héad{}táil", "ü".repeat(PRUNE_RESULT_MAX_CHARS * 3));
        assert!(
            content.len() > content.chars().count(),
            "the fixture is genuinely multibyte"
        );

        let excerpt = prune_excerpt(&content).expect("over the threshold");
        assert_eq!(excerpt.chars().count(), PRUNE_RESULT_MAX_CHARS);
        assert!(excerpt.starts_with("héad"), "{}", &excerpt[..16]);
        assert!(excerpt.ends_with("táil"));
        assert!(excerpt.contains(PRUNE_OMISSION_MARKER));
        assert!(
            excerpt.chars().all(|c| c != char::REPLACEMENT_CHARACTER),
            "nothing was split down the middle of a code point"
        );

        assert_eq!(
            prune_excerpt("héad"),
            None,
            "a result already under the threshold is left alone"
        );
    }

    /// The payoff. Pruning knows nothing about the window, so it can reclaim
    /// more than the pass needed; when it has, the summarization call would be
    /// spent cutting a history that already fits, and it is skipped. The pass
    /// still reports what it did and still bills nothing.
    #[tokio::test]
    async fn pruning_alone_can_meet_the_budget_and_skip_the_summary() {
        let mut history = tool_loop_history(&oversized(), "ok");
        let before = crate::llm::estimate_history_tokens(&history);
        assert!(
            before > LOW_WATER,
            "the history is over the mark a pass cuts to: {before}"
        );
        assert!(
            cut_boundary(&history, Anchor::Conversation, LOW_WATER).is_some(),
            "and a summarization pass had a span it would have paid to read"
        );
        let length = history.len();

        let compacted = compact(
            &mut history,
            Anchor::Conversation,
            budget(),
            &stub(),
            "model",
        )
        .await;

        assert_eq!(compacted.outcome, CompactOutcome::Evicted(5));
        assert_eq!(
            compacted.usage,
            CompactUsage::default(),
            "no model was called"
        );
        assert!(compacted.note.is_none());
        assert!(
            compacted
                .outcome
                .describe()
                .contains("no summarization call"),
            "{}",
            compacted.outcome.describe()
        );

        let after = crate::llm::estimate_history_tokens(&history);
        assert!(
            after <= LOW_WATER,
            "the free pass alone got the history under the mark: {after}"
        );
        assert_eq!(
            history.len(),
            length,
            "and nothing was folded away to do it"
        );
        assert!(
            history
                .iter()
                .all(|m| !m.text().starts_with(COMPACT_SUMMARY_HEADING)),
            "no summary note was written"
        );
    }

    /// The common case: results the model can still read end to end. Pruning
    /// leaves every one of them byte for byte, and the pass it runs inside
    /// still does what it always did. A free win that did not happen must not
    /// stand in for the summary, or a `/compact` over a history of ordinary
    /// tool output would quietly stop cutting anything.
    #[tokio::test]
    async fn small_results_are_left_whole_and_the_ordinary_pass_still_runs() {
        let mut history = tool_loop_history("ok", "ok");
        let before: Vec<_> = history.iter().map(|m| m.content.clone()).collect();

        assert_eq!(shrink_old_results(&mut history, KEEP_WHOLE_RESULTS, 0), 0);
        for (message, original) in history.iter().zip(&before) {
            assert_eq!(&message.content, original, "nothing was rewritten");
        }

        let compacted = compact(
            &mut history,
            Anchor::Conversation,
            budget(),
            &stub(),
            "model",
        )
        .await;
        assert!(
            matches!(compacted.outcome, CompactOutcome::Summarized(_)),
            "{:?}",
            compacted.outcome
        );
        assert!(compacted.usage.reported());
    }

    /// A history in the shape a long working session leaves behind: one
    /// `execute` per step, each with its own command and its own output.
    fn worked_history(steps: usize, output: &dyn Fn(usize) -> String) -> Vec<ChatMessage> {
        let mut history = vec![ChatMessage::system("you are wizard")];
        for step in 0..steps {
            let call = ToolCall::new(
                "execute",
                json!({ "command": format!("cargo test --lib step{step}") }),
            );
            let id = call.id.clone();
            let mut assistant = ChatMessage::assistant("");
            assistant.push_tool_call(call);
            history.push(assistant);
            history.push(ChatMessage::tool_result(&id, "execute", output(step)));
        }
        history
    }

    /// The oldest results stop being carried at all: one line naming the
    /// command and saying how it ended. What that line must never do is read
    /// like the command failed, because a model that thinks its build broke
    /// goes and fixes a build that is fine.
    #[test]
    fn the_oldest_results_collapse_to_a_line_that_keeps_the_verdict() {
        const STEPS: usize = 20;
        let failed = 3;
        let mut history = worked_history(STEPS, &|step| {
            if step == failed {
                format!("Error: exit 1\n{}", "assertion failed\n".repeat(200))
            } else {
                "ok\n".repeat(600)
            }
        });

        let shrunk = shrink_old_results(&mut history, KEEP_WHOLE_RESULTS, 0);
        assert_eq!(
            shrunk,
            STEPS - KEEP_WHOLE_RESULTS,
            "everything past the last {KEEP_WHOLE_RESULTS} results"
        );

        let digests: Vec<&str> = history
            .iter()
            .flat_map(ChatMessage::tool_results)
            .map(|result| result.content.as_str())
            .filter(|content| content.contains("lines elided"))
            .collect();
        assert_eq!(digests.len(), shrunk);
        assert!(
            digests[0].contains("execute `cargo test --lib step0`"),
            "the digest names what ran: {}",
            digests[0]
        );
        assert!(
            digests[0].contains("session's transcript"),
            "and where the full output went: {}",
            digests[0]
        );

        let (errors, oks): (Vec<&&str>, Vec<&&str>) =
            digests.iter().partition(|line| line.contains(": error,"));
        assert_eq!(errors.len(), 1, "one command failed, and its line says so");
        assert!(errors[0].contains("step3"), "{}", errors[0]);
        for line in &oks {
            assert!(line.contains(": ok,"), "{line}");
            assert!(
                !line.starts_with("Error:") && !line.contains("error"),
                "a result that succeeded cannot come back reading like one that failed: {line}"
            );
        }

        // The newest results are untouched, and a second pass changes nothing.
        let kept: Vec<String> = history
            .iter()
            .flat_map(ChatMessage::tool_results)
            .skip(shrunk)
            .map(|result| result.content.clone())
            .collect();
        assert_eq!(kept.len(), KEEP_WHOLE_RESULTS);
        assert!(kept.iter().all(|content| content.len() > DIGEST_MIN_CHARS));
        assert_eq!(
            shrink_old_results(&mut history, KEEP_WHOLE_RESULTS, 0),
            0,
            "a settled history is a fixed point"
        );
    }

    /// Between compactions a pass has to pay for itself. Rewriting one message
    /// in the middle of the history throws away the provider's cached prefix
    /// from there on, so a step that would reclaim a few hundred characters
    /// leaves the history exactly as it is and waits for one that reclaims
    /// something.
    #[test]
    fn a_pass_that_would_reclaim_too_little_rewrites_nothing() {
        let mut history = worked_history(20, &|step| {
            if step == 0 {
                "y".repeat(DIGEST_MIN_CHARS + 200)
            } else {
                "ok".to_string()
            }
        });
        let before: Vec<_> = history
            .iter()
            .map(|message| message.content.clone())
            .collect();

        assert_eq!(
            shrink_old_results(&mut history, KEEP_WHOLE_RESULTS, MIN_RECLAIM_CHARS),
            0
        );
        for (message, original) in history.iter().zip(&before) {
            assert_eq!(&message.content, original, "nothing was rewritten");
        }

        assert_eq!(
            shrink_old_results(&mut history, KEEP_WHOLE_RESULTS, 0),
            1,
            "the same pass inside a compaction, which pays that cost anyway, still cuts"
        );
    }

    /// The trigger is a fraction of the window, which is the right rule at 32k
    /// and useless at 500k: 80% of a grok-4.6 window is 400k tokens, and over
    /// 89 benchmark trials nothing ever got near it. Capping the window is
    /// what puts the trigger back where a long session reaches it.
    #[test]
    fn a_large_window_is_capped_so_the_trigger_arrives() {
        const CAP: u32 = 150_000;
        let huge = Some(500_000);
        assert_eq!(effective_window(huge, CAP), Some(CAP));
        assert_eq!(
            effective_window(Some(128_000), CAP),
            Some(128_000),
            "a window under the cap is its own budget"
        );
        assert_eq!(effective_window(huge, 0), huge, "0 means no cap");
        assert_eq!(effective_window(None, CAP), None);

        let measured = |window| Measured {
            tokens: 121_000,
            window,
            bytes: 500_000,
            byte_threshold: 48_000,
            last_prompt: Some(121_000),
        };
        assert_eq!(
            pressure(measured(effective_window(huge, CAP))).level,
            PressureLevel::Critical,
            "121k of a 150k budget compacts"
        );
        assert_eq!(
            pressure(measured(huge)).level,
            PressureLevel::Ok,
            "and the uncapped window is what let it sail past"
        );

        let budget = Budget {
            window: effective_window(huge, CAP),
            byte_threshold: 48_000,
        };
        assert_eq!(
            budget.low_water_tokens(),
            60_000,
            "a pass still cuts to half the trigger"
        );
    }

    /// What the whole thing is for, on a session shaped like the ones the
    /// benchmark ran: 60 steps of shell work, a third of them producing real
    /// output. Prints the prompt sizes so the numbers in the notes are
    /// reproducible with `cargo test -- --nocapture`.
    #[test]
    fn a_long_session_sends_a_fraction_of_the_prompt_it_used_to() {
        const STEPS: usize = 60;
        const TRIGGER: u64 = 32_000;
        let output = |step: usize| {
            if step.is_multiple_of(3) {
                format!(
                    "running step {step}\n{}",
                    "warning: unused variable\n".repeat(500)
                )
            } else {
                format!("step {step} ok\n")
            }
        };

        // Two runs of the same session: one that never shrinks anything (what
        // 3.1 does, since compaction needs 400k on this model), one that runs
        // the pass the loop now runs.
        let mut before = vec![ChatMessage::system("you are wizard")];
        let mut after = vec![ChatMessage::system("you are wizard")];
        let (mut before_total, mut after_total) = (0u64, 0u64);
        for step in 0..STEPS {
            for history in [&mut before, &mut after] {
                let call = ToolCall::new(
                    "execute",
                    json!({ "command": format!("cargo test --lib step{step}") }),
                );
                let id = call.id.clone();
                let mut assistant = ChatMessage::assistant("");
                assistant.push_tool_call(call);
                history.push(assistant);
                history.push(ChatMessage::tool_result(&id, "execute", output(step)));
            }
            if crate::llm::estimate_history_tokens(&after) >= TRIGGER {
                shrink_old_results(&mut after, KEEP_WHOLE_RESULTS, MIN_RECLAIM_CHARS);
            }
            before_total += crate::llm::estimate_history_tokens(&before);
            after_total += crate::llm::estimate_history_tokens(&after);
        }

        let before_last = crate::llm::estimate_history_tokens(&before);
        let after_last = crate::llm::estimate_history_tokens(&after);
        println!(
            "{STEPS} steps: prompt per call {} -> {} tokens on average, \
             {before_last} -> {after_last} at the last step",
            before_total / STEPS as u64,
            after_total / STEPS as u64
        );
        assert!(
            after_total * 3 < before_total * 2,
            "the average prompt is down by a third, over a run whose first half \
             never reaches the trigger: {} -> {}",
            before_total / STEPS as u64,
            after_total / STEPS as u64
        );
        assert!(
            after_last < before_last / 3,
            "and the prompt stops growing: {before_last} -> {after_last} at the last step"
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
