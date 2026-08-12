//! Token-usage accounting: per-call counters accumulated by the agent loop,
//! per-turn records appended to `~/.wizard/usage.jsonl`, and the cost of each
//! turn, computed at write time from a per-model price table (see [`PRICES`],
//! and [`SELLER_PRICES`] for the ids whose price depends on which host sold
//! them) or from per-provider `usd_per_mtok_{in,out}` config.
//!
//! Counts come from [`ChatChunk`](crate::llm::ChatChunk)'s
//! `prompt_eval_count` / `eval_count` fields (every provider reports them on
//! its final chunk when the backend exposes usage). Backends that report
//! nothing simply accumulate zeros and write no records.
//!
//! Cached prompt tokens are priced separately from fresh ones (a cache read
//! is a tenth of the input rate on Anthropic), because a cost column that
//! billed them at the full rate would make prompt caching look worthless,
//! which is exactly backwards.
//!
//! One caveat worth knowing when reading the numbers: the OAuth
//! subscription backends ([`ProviderKind::ChatgptOauth`],
//! [`ProviderKind::XaiOauth`]) are not billed per token at all, so their
//! cost is what the same tokens *would* have cost on the metered API rather
//! than money that left an account.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::ProviderKind;

/// Token counters for one agent. Atomics so the agent loop can record from
/// `&self` mid-stream; the agent is single-threaded over these, so plain
/// relaxed ordering suffices.
#[derive(Debug, Default)]
pub struct UsageTracker {
    session_prompt: AtomicU64,
    session_completion: AtomicU64,
    turn_prompt: AtomicU64,
    turn_completion: AtomicU64,
    /// Prompt size of the most recent model call, +1 so 0 means "unknown"
    /// (a genuinely 0-token prompt cannot occur: the system prompt counts).
    last_prompt: AtomicU64,
    session_cache_read: AtomicU64,
    session_cache_write: AtomicU64,
    turn_cache_read: AtomicU64,
    turn_cache_write: AtomicU64,
}

impl UsageTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the usage of one model call. `None` fields (backend reported
    /// nothing) leave the counters untouched.
    pub fn record(&self, prompt_tokens: Option<u64>, completion_tokens: Option<u64>) {
        if let Some(prompt) = prompt_tokens {
            self.session_prompt.fetch_add(prompt, Ordering::Relaxed);
            self.turn_prompt.fetch_add(prompt, Ordering::Relaxed);
            self.last_prompt
                .store(prompt.saturating_add(1), Ordering::Relaxed);
        }
        if let Some(completion) = completion_tokens {
            self.session_completion
                .fetch_add(completion, Ordering::Relaxed);
            self.turn_completion
                .fetch_add(completion, Ordering::Relaxed);
        }
    }

    /// Record the prompt-cache breakdown of one model call: how many of that
    /// call's prompt tokens were served from the provider's cache, and how
    /// many were written into it.
    ///
    /// Split from [`record`](Self::record) rather than folded into it so the
    /// adapters can adopt it one at a time: a backend that reports no cache
    /// detail simply never calls this and its turns price as all-fresh, the
    /// same (conservative) way they price today. Both counts are *subsets* of
    /// the prompt count passed to `record`; see [`TurnTokens`] for why that
    /// matters and what an adapter owes this seam.
    pub fn record_cache(&self, cache_read_tokens: u64, cache_write_tokens: u64) {
        self.session_cache_read
            .fetch_add(cache_read_tokens, Ordering::Relaxed);
        self.turn_cache_read
            .fetch_add(cache_read_tokens, Ordering::Relaxed);
        self.session_cache_write
            .fetch_add(cache_write_tokens, Ordering::Relaxed);
        self.turn_cache_write
            .fetch_add(cache_write_tokens, Ordering::Relaxed);
    }

    /// Record the usage of one *delegated* model call — a subagent run the
    /// parent paid for (`spawn_subagent`, or one of `/ultra`'s candidates and
    /// judges).
    ///
    /// It lands on the session and turn totals, because the tokens are spent
    /// either way, but deliberately not on `last_prompt`: that one is the
    /// *parent's* own prompt size and is what decides when to compact, and a
    /// candidate's prompt — a different history, a different system prompt —
    /// says nothing about how full the parent's context window is.
    ///
    /// **The cache split of a delegated call goes through
    /// [`record_cache`](Self::record_cache), called beside this**, exactly as
    /// the parent's own calls do. There is no `record_delegated_cache` and
    /// there should not be: `record_cache` touches only the cache counters,
    /// which have no parent/delegated distinction to draw — the whole reason
    /// this method exists separately is `last_prompt`, and `record_cache`
    /// does not write it. A delegated call that reports a cache hit and does
    /// not reach `record_cache` bills as all-fresh, which under-states the
    /// saving in the one place it is largest: `/ultra` fans N candidates that
    /// each re-send the same prefix, which is precisely what a prompt cache
    /// is for.
    pub fn record_delegated(&self, prompt_tokens: u64, completion_tokens: u64) {
        self.session_prompt
            .fetch_add(prompt_tokens, Ordering::Relaxed);
        self.turn_prompt.fetch_add(prompt_tokens, Ordering::Relaxed);
        self.session_completion
            .fetch_add(completion_tokens, Ordering::Relaxed);
        self.turn_completion
            .fetch_add(completion_tokens, Ordering::Relaxed);
    }

    /// Record a model call that is billed on its own line of
    /// `usage.jsonl` instead of inside a turn record: history compaction,
    /// which the agent writes as it happens.
    ///
    /// Session totals only, and that is the whole distinction. A compaction
    /// pass can run *between* turns (`/compact` at the prompt), where the
    /// per-turn counters are about to be zeroed by the next
    /// [`begin_turn`](Self::begin_turn) and anything left in them is lost; and
    /// when it runs *inside* a turn, adding it to the turn totals as well as
    /// writing its own line would bill the same tokens twice. It stays off
    /// `last_prompt` for the same reason a delegated call does: the
    /// summarizer's prompt is a different history, and it says nothing about
    /// how full this conversation's window is.
    pub fn record_side_call(
        &self,
        prompt_tokens: u64,
        completion_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
    ) {
        self.session_prompt
            .fetch_add(prompt_tokens, Ordering::Relaxed);
        self.session_completion
            .fetch_add(completion_tokens, Ordering::Relaxed);
        self.session_cache_read
            .fetch_add(cache_read_tokens, Ordering::Relaxed);
        self.session_cache_write
            .fetch_add(cache_write_tokens, Ordering::Relaxed);
    }

    /// Reset the per-turn counters (called at the top of every turn).
    pub fn begin_turn(&self) {
        self.turn_prompt.store(0, Ordering::Relaxed);
        self.turn_completion.store(0, Ordering::Relaxed);
        self.turn_cache_read.store(0, Ordering::Relaxed);
        self.turn_cache_write.store(0, Ordering::Relaxed);
    }

    /// `(prompt, completion)` tokens of the current turn.
    pub fn turn_totals(&self) -> (u64, u64) {
        (
            self.turn_prompt.load(Ordering::Relaxed),
            self.turn_completion.load(Ordering::Relaxed),
        )
    }

    /// `(cache_read, cache_write)` prompt tokens of the current turn, both
    /// subsets of [`turn_totals`](Self::turn_totals)'s prompt count.
    pub fn turn_cache_totals(&self) -> (u64, u64) {
        (
            self.turn_cache_read.load(Ordering::Relaxed),
            self.turn_cache_write.load(Ordering::Relaxed),
        )
    }

    /// `(prompt, completion)` tokens of the whole session.
    pub fn session_totals(&self) -> (u64, u64) {
        (
            self.session_prompt.load(Ordering::Relaxed),
            self.session_completion.load(Ordering::Relaxed),
        )
    }

    /// `(cache_read, cache_write)` prompt tokens of the whole session.
    pub fn session_cache_totals(&self) -> (u64, u64) {
        (
            self.session_cache_read.load(Ordering::Relaxed),
            self.session_cache_write.load(Ordering::Relaxed),
        )
    }

    /// Prompt size of the most recent model call, when the backend reported
    /// one. Drives token-aware compaction.
    pub fn last_prompt_tokens(&self) -> Option<u64> {
        match self.last_prompt.load(Ordering::Relaxed) {
            0 => None,
            stored => Some(stored - 1),
        }
    }

    /// Forget the last prompt size (after compaction shrank the history, so
    /// a stale large count does not re-trigger compaction immediately).
    pub fn clear_last_prompt(&self) {
        self.last_prompt.store(0, Ordering::Relaxed);
    }

    /// Zero every counter (session, turn, last prompt). Used by `/clear` so
    /// the TUI context meter and `/cost` do not keep totals from the wiped
    /// conversation.
    pub fn clear_session(&self) {
        self.session_prompt.store(0, Ordering::Relaxed);
        self.session_completion.store(0, Ordering::Relaxed);
        self.turn_prompt.store(0, Ordering::Relaxed);
        self.turn_completion.store(0, Ordering::Relaxed);
        self.last_prompt.store(0, Ordering::Relaxed);
        self.session_cache_read.store(0, Ordering::Relaxed);
        self.session_cache_write.store(0, Ordering::Relaxed);
        self.turn_cache_read.store(0, Ordering::Relaxed);
        self.turn_cache_write.store(0, Ordering::Relaxed);
    }
}

/// One line of `~/.wizard/usage.jsonl`: the token usage of one agent turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    /// Unix seconds when the turn ended.
    pub ts: u64,
    /// Project root the agent worked in.
    pub project: String,
    pub model: String,
    /// Configured provider name (e.g. `"local"`, `"anthropic"`).
    pub provider: String,
    /// Every input token of the turn, cached ones included. See
    /// [`TurnTokens::prompt`].
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Prompt tokens served from the provider's cache (Anthropic
    /// `cache_read_input_tokens`, OpenAI
    /// `prompt_tokens_details.cached_tokens`). A *subset* of
    /// `prompt_tokens`, never an addition to it.
    #[serde(default)]
    pub cache_read_tokens: u64,
    /// Prompt tokens written into the provider's cache (Anthropic
    /// `cache_creation_input_tokens`). Also a subset of `prompt_tokens`;
    /// providers that do not bill a separate cache write report 0.
    #[serde(default)]
    pub cache_write_tokens: u64,
    /// Estimated cost of the turn in USD, from [`estimate_cost`]. `None`
    /// only on records written before cost accounting existed.
    #[serde(default)]
    pub cost_usd: Option<f64>,
    /// Where `cost_usd` came from, so `wizard usage` can flag the rows whose
    /// price is a guess instead of quietly presenting them as fact.
    #[serde(default)]
    pub price_source: PriceSource,
    /// Personality mode (`genie` / `sovereign`).
    pub mode: String,
}

/// Which rate produced a [`UsageRecord::cost_usd`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceSource {
    /// `usd_per_mtok_in` / `usd_per_mtok_out` from the provider's config.
    /// The user typed these, so they win over everything below.
    Config,
    /// The built-in [`PRICES`] table matched the model id.
    Table,
    /// A self-hosted backend (llama.cpp, Ollama): the tokens cost
    /// electricity, not dollars, so $0.00 is the honest figure here.
    Local,
    /// Unknown model on a metered backend: priced at [`FALLBACK_PRICE`].
    /// Deliberately visible, because the number is an over-estimate.
    Fallback,
    /// Record written before cost accounting existed. Only ever produced by
    /// deserializing an old line, never by [`estimate_cost`].
    #[default]
    Unpriced,
}

/// `~/.wizard/usage.jsonl`, or `None` when the home directory cannot be
/// resolved (usage logging is then skipped, never fatal).
pub fn default_log_path() -> Option<PathBuf> {
    crate::config::Config::wizard_dir()
        .ok()
        .map(|dir| dir.join("usage.jsonl"))
}

/// Append one record to the JSONL usage log at `path`, creating the file
/// (and its parent directory) as needed.
pub fn append(path: &Path, record: &UsageRecord) -> Result<()> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut line = serde_json::to_string(record).context("serializing usage record")?;
    line.push('\n');
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| file.write_all(line.as_bytes()))
        .with_context(|| format!("appending to {}", path.display()))
}

/// Current time as unix seconds.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Estimated cost in USD for the given token totals, when at least one rate
/// (`usd_per_mtok_in` / `usd_per_mtok_out`, dollars per million tokens) is
/// configured for the provider. `None` means "no rates configured".
///
/// Cache-aware, and it has to be: this used to take a flat prompt count and
/// bill every token of it at the full input rate, while the per-turn writer
/// two functions down priced the same session's cache reads at a tenth of
/// that. `/cost` and `wizard usage` then reported different numbers for
/// identical work, and the gap was not small — a long session resends the
/// whole conversation every step, so cache reads dominate the prompt side and
/// `/cost` could overstate it by close to tenfold.
///
/// The two agree now because they share [`price_tokens`] and the same
/// [`ModelPrice::per_mtok`] multipliers. `estimate_cost`'s configured-rate arm
/// derives its cache rates exactly this way, for the reason stated there:
/// config has no cache fields, so configured rates get the default
/// multipliers.
pub fn cost_usd(
    tokens: TurnTokens,
    usd_per_mtok_in: Option<f64>,
    usd_per_mtok_out: Option<f64>,
) -> Option<f64> {
    if usd_per_mtok_in.is_none() && usd_per_mtok_out.is_none() {
        return None;
    }
    Some(price_tokens(
        tokens,
        ModelPrice::per_mtok(
            usd_per_mtok_in.unwrap_or(0.0),
            usd_per_mtok_out.unwrap_or(0.0),
        ),
    ))
}

/// Token counts to dollars, at one price. The only place that arithmetic
/// lives, so the session total and the per-turn total cannot disagree about
/// what a cached token costs — which is exactly how they came apart.
fn price_tokens(tokens: TurnTokens, price: ModelPrice) -> f64 {
    let cached = tokens.cache_read.saturating_add(tokens.cache_write);
    // Tokens billed at the full input rate. When the caller reported a prompt
    // that *excludes* its cached tokens (Anthropic's wire shape, un-summed)
    // the subtraction would saturate to zero and we would bill nothing for
    // the uncached remainder, so that case charges the reported prompt whole.
    // Both conventions therefore land on the same total; see [`TurnTokens`].
    let full_rate = if tokens.prompt >= cached {
        tokens.prompt - cached
    } else {
        tokens.prompt
    };

    let per_mtok = |count: u64, rate: f64| count as f64 / 1e6 * rate;
    per_mtok(full_rate, price.input)
        + per_mtok(tokens.cache_read, price.cache_read)
        + per_mtok(tokens.cache_write, price.cache_write)
        + per_mtok(tokens.completion, price.output)
}

// ---------------------------------------------------------------------------
// Price table
// ---------------------------------------------------------------------------
//
// MAINTAINING THIS: the numbers below are list prices in USD per million
// tokens, copied by hand from the vendor's published pricing page. Each block
// carries the date it was checked and the page it came from. When a price
// moves or a model ships, edit the block, bump its date, and run
// `cargo test -p wizard usage::` (`price_table_covers_the_fallback` will tell
// you if the new model is pricier than the unknown-model fallback).
//
// A model that is NOT listed here is not free: it prices at FALLBACK_PRICE
// and is flagged in `wizard usage` output, so the miss is visible and
// over-stated rather than invisible and zero. Adding a row is therefore an
// accuracy improvement, never a correctness fix, and it is fine to leave a
// provider out until someone has the real numbers in front of them. Users who
// cannot wait set `usd_per_mtok_in` / `usd_per_mtok_out` on their provider in
// ~/.wizard/config.toml, which overrides everything here.
//
// Still deliberately absent, as of 2026-08-07: Cloudflare Workers AI (prices
// in neurons, no published per-token table), Z.AI, MiniMax, Together,
// Fireworks and Cerebras (no per-token rate found on a first-party page —
// aggregator blogs quote numbers, and a number sourced from a blog would be
// flagged `table` and read as fact). Leaving them on the fallback is the
// stated policy, not an oversight.

/// List price of one model, in USD per million tokens, split by token class.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPrice {
    /// Fresh (uncached) input tokens.
    pub input: f64,
    /// Output tokens.
    pub output: f64,
    /// Input tokens served from the provider's prompt cache.
    pub cache_read: f64,
    /// Input tokens written into the provider's prompt cache.
    pub cache_write: f64,
}

impl ModelPrice {
    /// Build a price from the two headline rates, deriving the cache rates
    /// from Anthropic's published multipliers: a cache read is 0.1x input and
    /// a cache write (5 minute TTL) is 1.25x input.
    ///
    /// Every provider that bills caching at all is at least this shape, and
    /// where they differ they are cheaper: OpenAI discounts cached input and
    /// bills nothing extra for the write. Guessing high on the write and
    /// low-but-nonzero on the read keeps the estimate on the safe side of the
    /// truth without erasing the saving that caching actually buys.
    const fn per_mtok(input: f64, output: f64) -> Self {
        Self {
            input,
            output,
            cache_read: input * 0.1,
            cache_write: input * 1.25,
        }
    }

    /// Build a price from a vendor that publishes its own cached-input rate,
    /// rather than inferring one from Anthropic's multipliers.
    ///
    /// Worth a second constructor because the multipliers are not close to
    /// universal on the read side: DeepSeek's disk cache charges about 2% of
    /// the miss rate (0.02x, not 0.1x) and xAI charges 0.15x, so guessing
    /// 0.1x over-states one by fivefold and under-states the other by half.
    ///
    /// `cache_write` is the plain input rate here, deliberately, and this is
    /// the one place in the table where that differs from [`per_mtok`]:
    /// Anthropic is the only vendor listed that bills a *premium* to seed its
    /// cache. Everywhere else the cache fills automatically as a side effect
    /// of the miss, so a token counted as "written" is a cache-miss token and
    /// costs exactly what an uncached input token costs. Charging it 1.25x
    /// would invent a fee the vendor does not levy.
    ///
    /// [`per_mtok`]: Self::per_mtok
    const fn per_mtok_cached(input: f64, output: f64, cache_read: f64) -> Self {
        Self {
            input,
            output,
            cache_read,
            cache_write: input,
        }
    }
}

/// Per-model list prices, matched against the model id (see
/// [`lookup_price`]). Read the maintenance note above before editing.
///
/// Three hazards decide what is in here and what is deliberately not, and all
/// three come from the same place: [`lookup_price`] sees a model id and
/// nothing else.
///
/// 1. **A model id does not name a seller.** `gpt-oss-120b` is on Groq,
///    Together, Fireworks and Cerebras — four rows in [`crate::llm::compat`]
///    alone — at four prices, so any single number for it is wrong for three
///    of them, and wrong while flagged [`PriceSource::Table`], which reads as
///    fact. Every id *below* is one that exactly one vendor sells, so it needs
///    no seller to be priced. The ids several vendors sell live in
///    [`SELLER_PRICES`] instead, keyed by the API host that billed them; a
///    seller with no row there still falls to [`FALLBACK_PRICE`] rather than
///    borrowing a rival's number.
/// 2. **A shorter key shadows a longer id**, because matching is substring.
///    That is wanted for date suffixes and vendor prefixes and unwanted for
///    a pricier sibling: `gpt-5.5-pro` is six times `gpt-5.5`, so tabling
///    `gpt-5.5` would silently bill a Pro turn at the base rate. The base
///    `gpt-5`, `gpt-5.1`, `gpt-5.2`, `gpt-5.4` and `gpt-5.5` ids are left out
///    for exactly that reason and fall to [`FALLBACK_PRICE`], which is
///    visibly a guess. `models_with_a_pricier_sibling_stay_on_the_fallback`
///    fails if one is ever added.
/// 3. **It has no per-request prompt size.** xAI and Google both bill a
///    second, higher rate above a 200k-token request, and the number reaching
///    [`estimate_cost`] is a whole *turn* — the sum over every model call in
///    it — so a five-step turn of 50k-token calls would trip a 200k threshold
///    that no single request crossed. Tiering on that total would over-charge
///    ordinary multi-step turns to avoid under-charging rare long ones, so
///    the tiered models below carry their standard (under-200k) rate and a
///    genuinely long single request under-states. Fixing it properly means
///    pricing per call, not per turn.
///
/// Anthropic block: checked 2026-06-24 against Anthropic's published API
/// pricing (platform.claude.com/docs/en/pricing, mirrored in the bundled
/// `claude-api` skill's model table). Sonnet 5's $3/$15 is the standard rate,
/// not the $2/$10 introductory rate that expires 2026-08-31, because
/// over-stating a discount is the safer error.
const PRICES: &[(&str, ModelPrice)] = &[
    ("claude-fable-5", ModelPrice::per_mtok(10.0, 50.0)),
    ("claude-mythos-5", ModelPrice::per_mtok(10.0, 50.0)),
    ("claude-opus-5", ModelPrice::per_mtok(5.0, 25.0)),
    ("claude-opus-4-8", ModelPrice::per_mtok(5.0, 25.0)),
    ("claude-opus-4-7", ModelPrice::per_mtok(5.0, 25.0)),
    ("claude-opus-4-6", ModelPrice::per_mtok(5.0, 25.0)),
    ("claude-opus-4-5", ModelPrice::per_mtok(5.0, 25.0)),
    ("claude-sonnet-5", ModelPrice::per_mtok(3.0, 15.0)),
    ("claude-sonnet-4-6", ModelPrice::per_mtok(3.0, 15.0)),
    ("claude-sonnet-4-5", ModelPrice::per_mtok(3.0, 15.0)),
    ("claude-haiku-4-5", ModelPrice::per_mtok(1.0, 5.0)),
    // OpenAI: checked 2026-08-07 against developers.openai.com/api/docs/pricing.
    // Cached input is a flat 0.1x of input across the whole family, and the
    // cache is automatic with no write fee — the rates are carried explicitly
    // anyway so a future divergence is a one-line edit rather than a silent
    // inheritance of Anthropic's multipliers.
    //
    // The `-pro` variants ($30/$180 and $21/$168) are absent on purpose: no
    // Wizard picker offers one, and tabling them would force FALLBACK_PRICE
    // up to $30/$180 and make every unknown model read three times worse than
    // it does now. Their *base* ids are absent for hazard 2 above.
    ("gpt-5.6-sol", ModelPrice::per_mtok_cached(5.0, 30.0, 0.5)),
    ("gpt-5.6-terra", ModelPrice::per_mtok_cached(2.0, 12.0, 0.2)),
    ("gpt-5.6-luna", ModelPrice::per_mtok_cached(0.2, 1.2, 0.02)),
    (
        "gpt-5.3-codex",
        ModelPrice::per_mtok_cached(1.75, 14.0, 0.175),
    ),
    (
        "gpt-5.4-mini",
        ModelPrice::per_mtok_cached(0.75, 4.5, 0.075),
    ),
    ("gpt-5.4-nano", ModelPrice::per_mtok_cached(0.2, 1.25, 0.02)),
    ("gpt-5-mini", ModelPrice::per_mtok_cached(0.25, 2.0, 0.025)),
    ("gpt-5-nano", ModelPrice::per_mtok_cached(0.05, 0.4, 0.005)),
    // xAI: checked 2026-08-07 against docs.x.ai/docs/models. Standard
    // (under-200k-request) tier; see hazard 3. Cached input is 0.15x to 0.2x
    // here rather than the 0.1x `per_mtok` would have assumed, so these rates
    // are the published ones, not derived.
    ("grok-4.5", ModelPrice::per_mtok_cached(2.0, 6.0, 0.3)),
    ("grok-4.3", ModelPrice::per_mtok_cached(1.25, 2.5, 0.2)),
    ("grok-4.20", ModelPrice::per_mtok_cached(1.25, 2.5, 0.2)),
    ("grok-build-0.1", ModelPrice::per_mtok_cached(1.0, 2.0, 0.2)),
    // Google Gemini: checked 2026-08-07 against ai.google.dev/gemini-api/docs/pricing,
    // standard (non-batch) tier, text rates. The Pro models tier at 200k; see
    // hazard 3. Gemini's context cache also bills storage by the hour, which
    // is not a token rate and has no counter on a chat response, so it is not
    // and cannot be represented here — a heavy cache user pays slightly more
    // than this column says.
    //
    // Every `-lite` id is listed beside its non-lite sibling because it
    // *contains* it: without the longer key, `gemini-2.5-flash-lite` would
    // match `gemini-2.5-flash` and bill three times its real rate.
    (
        "gemini-3.6-flash",
        ModelPrice::per_mtok_cached(1.5, 7.5, 0.15),
    ),
    (
        "gemini-3.5-flash",
        ModelPrice::per_mtok_cached(1.5, 9.0, 0.15),
    ),
    (
        "gemini-3.5-flash-lite",
        ModelPrice::per_mtok_cached(0.3, 2.5, 0.03),
    ),
    (
        "gemini-3.1-pro",
        ModelPrice::per_mtok_cached(2.0, 12.0, 0.2),
    ),
    (
        "gemini-3.1-flash-lite",
        ModelPrice::per_mtok_cached(0.25, 1.5, 0.025),
    ),
    (
        "gemini-2.5-pro",
        ModelPrice::per_mtok_cached(1.25, 10.0, 0.125),
    ),
    (
        "gemini-2.5-flash",
        ModelPrice::per_mtok_cached(0.3, 2.5, 0.03),
    ),
    (
        "gemini-2.5-flash-lite",
        ModelPrice::per_mtok_cached(0.1, 0.4, 0.01),
    ),
    // DeepSeek: checked 2026-08-07 against api-docs.deepseek.com/quick_start/pricing.
    // The cache-hit rate is the reason `per_mtok_cached` exists: 0.003625
    // against a 0.435 miss is 0.0083x, not 0.1x, so the derived rate would
    // have over-billed a cached DeepSeek turn twelvefold. The `input` figure
    // here is DeepSeek's cache-*miss* price, which is what an uncached token
    // costs.
    (
        "deepseek-v4-pro",
        ModelPrice::per_mtok_cached(0.435, 0.87, 0.003_625),
    ),
    (
        "deepseek-v4-flash",
        ModelPrice::per_mtok_cached(0.14, 0.28, 0.0028),
    ),
    // Mistral: checked 2026-08-07 against mistral.ai/pricing/api. The page
    // states cached input is 90% off, i.e. 0.1x, which is applied here rather
    // than left to `per_mtok` so the number is the vendor's rather than
    // Anthropic's that happens to coincide. `mistral-large` really is cheaper
    // than `mistral-medium` on that page; that is Mistral's repricing of
    // Large 3, not a transcription slip.
    //
    // `devstral-2512` — offered by Wizard's Mistral preset — is not on the
    // pricing page under that name, so it is not here and falls back.
    (
        "mistral-medium",
        ModelPrice::per_mtok_cached(1.5, 7.5, 0.15),
    ),
    ("mistral-large", ModelPrice::per_mtok_cached(0.5, 1.5, 0.05)),
    (
        "mistral-small",
        ModelPrice::per_mtok_cached(0.15, 0.6, 0.015),
    ),
    (
        "devstral-medium",
        ModelPrice::per_mtok_cached(0.4, 2.0, 0.04),
    ),
    (
        "devstral-small",
        ModelPrice::per_mtok_cached(0.1, 0.3, 0.01),
    ),
    // Groq: checked 2026-08-07 against console.groq.com/docs/models. This is
    // the only Groq id that belongs in the *open* table — the `-versatile`
    // suffix is Groq's own routing tag, so it names exactly one seller, while
    // the rest of Groq's catalogue is open-weight models three other presets
    // also serve (hazard 1) and is priced per seller in [`SELLER_PRICES`].
    //
    // The cache read is priced at the full input rate on purpose:
    // console.groq.com/docs/prompt-caching says caching is "currently only
    // supported for the following models" and lists three gpt-oss ids, of
    // which this is not one. Applying Groq's 50%-off cached rate here would
    // discount a cache that never fills, and under-billing is the invisible
    // error.
    (
        "llama-3.3-70b-versatile",
        ModelPrice::per_mtok_cached(0.59, 0.79, 0.59),
    ),
    // Moonshot: checked 2026-08-07 against platform.kimi.ai/docs/pricing/chat-k3.
    // `input` is the cache-miss price; the hit is 0.1x it.
    ("kimi-k3", ModelPrice::per_mtok_cached(3.0, 15.0, 0.3)),
];

/// Prices for model ids that more than one seller serves, keyed by
/// `(API host, model id fragment)`.
///
/// This is hazard 1 above, closed for the pairings whose rate is published
/// and left open for the ones whose rate is not. An open-weight model has no
/// single list price — `gpt-oss-120b` is $0.15/$0.60 on Groq, Together and
/// Fireworks and $0.35/$0.75 on Cerebras — so pricing it needs to know who
/// sold the tokens, and [`PRICES`] cannot say.
///
/// **Why a second table rather than a seller column on every row.** Almost
/// every id in [`PRICES`] is sold by exactly one vendor: `claude-opus-5`
/// costs what Anthropic charges wherever Wizard's Anthropic provider points,
/// and `grok-4.5` is xAI's whether it arrives as `grok-4.5` or
/// `grok-4.5-0309-reasoning`. Stamping a seller onto those rows would be
/// noise on 30 entries to make four legible, and worse, it would invite the
/// lookup to *require* a seller — turning every unrecognised endpoint (a
/// gateway, a mirror, a Bedrock ARN) into a fallback for models whose price
/// never depended on the endpoint. Rows here are the exception, and they read
/// as one.
///
/// **Why the host and not the provider's configured name.** The name is a
/// label: presets seed it (`groq`, `together`), but the custom-endpoint flow
/// asks the user to type one, so a local proxy can legitimately be called
/// `groq` and would then inherit Groq's rates for tokens Groq never sold.
/// The host is where the request actually went, which is the same thing as
/// who invoiced it. It also survives the harmless case the name does not: a
/// second Groq provider named `groq-fast` still prices as Groq. Matching is
/// exact host equality (see [`api_host`]), never a substring, so
/// `api.groq.com.someone-else.example` is a different seller — which it is.
///
/// A row here must not duplicate a model that [`PRICES`] already covers: the
/// open row would answer for every *other* seller of that id, which is the
/// hazard this table exists to avoid.
/// `a_seller_row_never_shares_its_model_with_the_open_table` fails if one
/// ever does.
const SELLER_PRICES: &[(&str, &str, ModelPrice)] = &[
    // Groq: checked 2026-08-07 against console.groq.com/docs/models
    // ($0.15 in / $0.60 out) and console.groq.com/docs/prompt-caching, which
    // gives cached input a 50% discount ($0.075) and lists `gpt-oss-120b` as
    // one of the three models caching is supported on — so unlike Groq's
    // llama row above, this cache discount is one the vendor really applies.
    (
        "api.groq.com",
        "gpt-oss-120b",
        ModelPrice::per_mtok_cached(0.15, 0.6, 0.075),
    ),
    // Together: checked 2026-08-07 against together.ai/pricing, serverless
    // chat models ($0.15 in / $0.60 out). Together prints a cached rate in
    // parentheses on the rows that have one and prints none on this one, so a
    // cache read is priced as fresh input rather than at an assumed discount.
    (
        "api.together.xyz",
        "gpt-oss-120b",
        ModelPrice::per_mtok_cached(0.15, 0.6, 0.15),
    ),
    // Fireworks: checked 2026-08-07 against docs.fireworks.ai/serverless/pricing,
    // whose row reads "$0.15 / $0.015 / $0.60" — input / cached input /
    // output. That is the Standard tier, not Priority ($0.18 / $0.018 /
    // $0.72): Priority is opt-in per request through the `service_tier`
    // parameter, which Wizard never sends.
    (
        "api.fireworks.ai",
        "gpt-oss-120b",
        ModelPrice::per_mtok_cached(0.15, 0.6, 0.015),
    ),
    // Cerebras: checked 2026-08-07 against
    // inference-docs.cerebras.ai/models/openai-oss ($0.35 in / $0.75 out) —
    // more than twice the others, which is the entire argument for this
    // table. The page lists prompt caching as a capability of the model but
    // publishes no cached-input rate, so a cache read costs fresh input.
    (
        "api.cerebras.ai",
        "gpt-oss-120b",
        ModelPrice::per_mtok_cached(0.35, 0.75, 0.35),
    ),
];

/// Price applied to a metered model that [`PRICES`] does not know and the
/// user has not given rates for.
///
/// Set to the most expensive entry in the table on purpose. An unknown model
/// has to read as "expensive guess", because the alternative failure (pricing
/// it at zero) renders as *free* and is the single most misleading thing this
/// column could say. `price_table_covers_the_fallback` fails if a pricier
/// model is ever added without raising this.
const FALLBACK_PRICE: ModelPrice = ModelPrice::per_mtok(10.0, 50.0);

/// Token counts of one turn, as they land on a [`UsageRecord`].
///
/// `prompt` is the WHOLE prompt: every input token the call was billed for,
/// cached ones included. `cache_read` and `cache_write` are the portions of
/// that total the provider served from / wrote into its prompt cache, so
/// `prompt - cache_read - cache_write` is what was billed at the full input
/// rate.
///
/// **Adapters owe this seam the summed number.** Providers disagree: OpenAI's
/// `prompt_tokens` already contains `prompt_tokens_details.cached_tokens`,
/// while Anthropic's `input_tokens` *excludes* both `cache_read_input_tokens`
/// and `cache_creation_input_tokens` (the three are siblings that have to be
/// added up). An Anthropic-shaped adapter must add them before reporting,
/// which it wants to do anyway: the context meter needs the real prompt size,
/// not the uncached remainder. [`estimate_cost`] still defends against the
/// un-summed shape rather than trusting the invariant, because that mistake
/// under-charges, and an under-charge is invisible.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TurnTokens {
    pub prompt: u64,
    pub completion: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

/// Everything [`estimate_cost`] needs besides the token counts: which model
/// produced them and how the provider is billed.
#[derive(Debug, Clone, Copy)]
pub struct PriceInputs<'a> {
    /// Model id as configured, e.g. `claude-opus-5` or `qwen3-8b`.
    pub model: &'a str,
    /// Base URL of the provider that served the turn, e.g.
    /// `https://api.groq.com/openai/v1`. Only its host is read, and only to
    /// tell two sellers of the same model id apart (see [`SELLER_PRICES`]).
    ///
    /// `""` is a legitimate value meaning "seller unknown", and is what a
    /// caller that has no endpoint to offer should pass: it narrows the
    /// lookup to the ids exactly one vendor sells, which is where every price
    /// lived before this field existed. It never turns a known price into a
    /// fallback.
    pub endpoint: &'a str,
    /// The provider's configured input rate, if any. Overrides the table.
    pub usd_per_mtok_in: Option<f64>,
    /// The provider's configured output rate, if any.
    pub usd_per_mtok_out: Option<f64>,
    /// True when the backend runs on the user's own hardware; see
    /// [`self_hosted`].
    pub self_hosted: bool,
}

/// The cost of one turn plus the provenance of the rate that produced it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PricedTurn {
    pub usd: f64,
    pub source: PriceSource,
}

/// Whether a backend runs on the user's own hardware, where tokens cost
/// electricity rather than dollars.
///
/// Exhaustive on purpose: a new backend has to declare which side of the bill
/// it is on rather than inheriting "free" from a wildcard arm.
pub fn self_hosted(kind: ProviderKind) -> bool {
    match kind {
        ProviderKind::LlamaCpp | ProviderKind::Ollama => true,
        ProviderKind::Openai
        | ProviderKind::Anthropic
        | ProviderKind::OpenRouter
        | ProviderKind::Xai
        | ProviderKind::XaiOauth
        | ProviderKind::ChatgptOauth
        | ProviderKind::Cloudflare => false,
    }
}

/// The host a base URL addresses, lowercased — the seller [`SELLER_PRICES`]
/// is keyed by. `None` when the string carries no authority at all (empty, or
/// a bare path), which reads as "seller unknown".
///
/// Hand-rolled rather than pulled in as a URL dependency because the whole
/// requirement is one field of one config string, and the failure mode of
/// getting it wrong is bounded: an unparsed authority matches no key and the
/// caller lands on the open table, which is where it would have been anyway.
/// Scheme, userinfo, port, path and query are all dropped; an IPv6 literal
/// keeps its brackets so the colons inside it are not mistaken for a port.
fn api_host(base_url: &str) -> Option<String> {
    let rest = base_url.trim();
    let rest = rest.split_once("://").map_or(rest, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = match (authority.starts_with('['), authority.find(']')) {
        (true, Some(end)) => &authority[..=end],
        _ => authority.split(':').next().unwrap_or_default(),
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// The price for a model id bought from `endpoint`, or `None` when neither
/// table covers that pairing.
///
/// Matches the longest table key that appears anywhere in the (lowercased) id,
/// which absorbs the ways the same model reaches us under different names:
/// date suffixes (`claude-opus-4-5-20251101`), Bedrock and Vertex prefixes
/// (`anthropic.claude-opus-5`, `us.anthropic.claude-haiku-4-5-v1:0`) and
/// OpenRouter's vendor-qualified ids (`anthropic/claude-sonnet-5`). Longest
/// wins so `claude-opus-4-5` cannot be shadowed by a shorter key.
///
/// [`SELLER_PRICES`] rows join that same contest, but only the ones whose
/// host is the host of `endpoint`: a row for a seller this turn did not use
/// is not a candidate at all, so an untabled seller of a tabled id falls
/// through to [`FALLBACK_PRICE`] instead of quietly matching a rival. Length
/// still decides first — hazard 2 does not care who is selling, and a longer
/// open key is still the more specific id — with seller-qualified winning
/// only a tie, where it is the strictly better-informed row.
fn lookup_price(model: &str, endpoint: &str) -> Option<ModelPrice> {
    let id = model.trim().to_ascii_lowercase();
    let host = api_host(endpoint);
    let open = PRICES.iter().map(|(key, price)| (*key, *price, false));
    let sellers = SELLER_PRICES
        .iter()
        .filter(|(seller, _, _)| host.as_deref() == Some(*seller))
        .map(|(_, key, price)| (*key, *price, true));
    open.chain(sellers)
        .filter(|(key, _, _)| id.contains(key))
        .max_by_key(|(key, _, from_seller)| (key.len(), *from_seller))
        .map(|(_, price, _)| price)
}

/// Estimated cost of one turn, with the rate's provenance.
///
/// Precedence: configured rates, then a self-hosted backend, then the price
/// table, then [`FALLBACK_PRICE`]. There is no "no answer" arm: every turn
/// gets a number, because a blank cost column is what this replaced.
pub fn estimate_cost(tokens: TurnTokens, inputs: &PriceInputs<'_>) -> PricedTurn {
    let (price, source) = match (inputs.usd_per_mtok_in, inputs.usd_per_mtok_out) {
        // Config has no cache fields, so configured rates get the default
        // multipliers. The user asked for these numbers; they outrank both
        // the table and the self-hosted shortcut below.
        (None, None) => {
            if inputs.self_hosted {
                return PricedTurn {
                    usd: 0.0,
                    source: PriceSource::Local,
                };
            }
            match lookup_price(inputs.model, inputs.endpoint) {
                Some(price) => (price, PriceSource::Table),
                None => (FALLBACK_PRICE, PriceSource::Fallback),
            }
        }
        (input, output) => (
            ModelPrice::per_mtok(input.unwrap_or(0.0), output.unwrap_or(0.0)),
            PriceSource::Config,
        ),
    };

    PricedTurn {
        usd: price_tokens(tokens, price),
        source,
    }
}

// ---------------------------------------------------------------------------
// `wizard usage` — rollup over ~/.wizard/usage.jsonl
// ---------------------------------------------------------------------------

/// Read-side view of one usage.jsonl line. Liberal on purpose (missing
/// fields default, unknown fields are ignored) so old and future records
/// both roll up; `cost_usd` is summed when a writer recorded one.
#[derive(Debug, Clone, Deserialize)]
struct LoggedTurn {
    ts: u64,
    #[serde(default)]
    project: String,
    #[serde(default)]
    provider: String,
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    cache_read_tokens: u64,
    #[serde(default)]
    cost_usd: Option<f64>,
    /// Read as a plain string, not as [`PriceSource`]: an unknown variant
    /// written by a future version must not take the whole line down with a
    /// deserialization error.
    #[serde(default)]
    price_source: Option<String>,
}

/// Aggregated usage for one rollup key (a project or a provider).
#[derive(Debug, Default, Clone, PartialEq)]
struct Rollup {
    turns: u64,
    prompt: u64,
    completion: u64,
    /// Prompt tokens that were served from a cache, a subset of `prompt`.
    cache_read: u64,
    /// Sum of the records that carried a cost; `None` when none did.
    cost_usd: Option<f64>,
    /// Any turn in the group was priced at the unknown-model fallback, so the
    /// total is an over-estimate and says so in the table.
    estimated: bool,
}

impl Rollup {
    fn add(&mut self, turn: &LoggedTurn) {
        self.turns += 1;
        self.prompt += turn.prompt_tokens;
        self.completion += turn.completion_tokens;
        self.cache_read += turn.cache_read_tokens;
        if let Some(cost) = turn.cost_usd {
            *self.cost_usd.get_or_insert(0.0) += cost;
        }
        self.estimated |= turn.price_source.as_deref() == Some("fallback");
    }
}

/// Parse a `--since` value: `7d`, `7D`, or a bare day count.
fn parse_since_days(raw: &str) -> Result<u64> {
    let days: u64 = raw
        .trim()
        .trim_end_matches(['d', 'D'])
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid --since {raw:?} (expected a day count like 7d)"))?;
    if days == 0 {
        bail!("--since must be at least 1 day");
    }
    Ok(days)
}

/// Group turns under a key ("(unknown)" for records missing it).
fn roll_up<'a>(
    turns: &'a [LoggedTurn],
    key: impl Fn(&'a LoggedTurn) -> &'a str,
) -> BTreeMap<String, Rollup> {
    let mut groups: BTreeMap<String, Rollup> = BTreeMap::new();
    for turn in turns {
        let raw = key(turn);
        let name = if raw.is_empty() { "(unknown)" } else { raw };
        groups.entry(name.to_string()).or_default().add(turn);
    }
    groups
}

/// A dollar amount for the cost column. Sub-dollar totals get four decimals,
/// and a total too small even for those rounds up to a visible `<$0.0001`
/// rather than down to `$0.0000`: a rollup that rendered every cheap turn as
/// zero would read as free, which is the same lie as printing no cost at all.
/// An exact `0.0` is the one figure allowed to print as zero, because a
/// self-hosted turn really did cost nothing.
fn format_usd(usd: f64) -> String {
    if usd >= 1.0 {
        format!("${usd:.2}")
    } else if usd > 0.0 && usd < 0.0001 {
        // The threshold is the smallest figure the four-decimal form can
        // show, so the label is literally true rather than an approximation
        // of wherever `{:.4}` happens to round.
        "<$0.0001".to_string()
    } else {
        format!("${usd:.4}")
    }
}

/// Append one aligned rollup table.
/// Widest the first column may get.
///
/// The keys are project paths, and one deep path used to set the width of
/// every row: a 110-character `/tmp/...` directory made all five other columns
/// start at column 112, so every row wrapped — including the short ones, which
/// were the readable ones. Same failure the mesh peer list already guards
/// against, and the same fix.
///
/// 40 rather than something that fits 80 columns. The five numeric columns and
/// their gaps are a fixed 57 characters, so an 80-column table would leave the
/// name 23 — enough to elide `/home/user/projects/web/dashboard` down to its
/// last two components. 40 puts the worst case at 97, which fits the terminal
/// most people run and keeps a path recognisable. The column is still sized to
/// the longest name present, so this only binds when something pathological is
/// in the log.
const MAX_NAME_WIDTH: usize = 40;

/// Clip a label to `max` characters, keeping the **end**.
///
/// The opposite end from the mesh's peer names, on purpose: these are
/// filesystem paths, and `/home/user/projects/web/dashboard` and
/// `/home/user/projects/web/dashboard-v2` differ in their last few
/// characters and share the first thirty. Truncating the head is what keeps
/// two projects tellable apart.
///
/// Characters, not bytes, so a multi-byte path component cannot be split into
/// something that is no longer text.
fn clip_end(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let kept = max.saturating_sub(1);
    std::iter::once('…')
        .chain(text.chars().skip(count - kept))
        .collect()
}

fn write_rollup(out: &mut String, title: &str, groups: &BTreeMap<String, Rollup>) {
    let names: Vec<String> = groups
        .keys()
        .map(|key| clip_end(key, MAX_NAME_WIDTH))
        .collect();
    let name_width = names
        .iter()
        .map(|name| name.chars().count())
        .max()
        .unwrap_or(0)
        .max(title.len());
    let _ = writeln!(
        out,
        "{title:<name_width$}  {:>6}  {:>10}  {:>10}  {:>10}  {:>11}",
        "turns", "prompt", "completion", "cached", "cost"
    );
    for (name, rollup) in names.iter().zip(groups.values()) {
        let cost = rollup.cost_usd.map_or_else(
            || "-".to_string(),
            |usd| {
                format!(
                    "{}{}",
                    format_usd(usd),
                    if rollup.estimated { "*" } else { "" }
                )
            },
        );
        let _ = writeln!(
            out,
            "{name:<name_width$}  {:>6}  {:>10}  {:>10}  {:>10}  {cost:>11}",
            rollup.turns,
            format_tokens(rollup.prompt),
            format_tokens(rollup.completion),
            format_tokens(rollup.cache_read),
        );
    }
}

/// Render the `wizard usage` report for an already-read log. Split out of
/// [`run_cli`] so tests can drive it over a fixture without a home directory,
/// and with a fixed `now` so the `--since` window is deterministic.
fn render_report(raw: &str, days: Option<u64>, now: u64) -> String {
    let cutoff = days.map(|d| now.saturating_sub(d.saturating_mul(86_400)));
    let turns: Vec<LoggedTurn> = raw
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| match serde_json::from_str::<LoggedTurn>(line) {
            Ok(turn) => Some(turn),
            Err(err) => {
                tracing::warn!("skipping malformed usage line: {err}");
                None
            }
        })
        .filter(|turn| cutoff.is_none_or(|cutoff| turn.ts >= cutoff))
        .collect();

    if turns.is_empty() {
        return match days {
            Some(days) => format!("no usage recorded in the last {days} day(s)\n"),
            None => "no usage recorded yet\n".to_string(),
        };
    }

    let by_project = roll_up(&turns, |t| t.project.as_str());
    let by_provider = roll_up(&turns, |t| t.provider.as_str());
    let window = days.map_or_else(|| "all time".to_string(), |d| format!("last {d}d"));

    let mut out = String::new();
    let _ = writeln!(out, "usage ({window}): {} turn(s)\n", turns.len());
    write_rollup(&mut out, "project", &by_project);
    out.push('\n');
    write_rollup(&mut out, "provider", &by_provider);
    if by_project.values().any(|rollup| rollup.estimated) {
        let _ = writeln!(
            out,
            "\n* estimated: a model with no entry in Wizard's price table was billed at the \
             highest known rate. Set usd_per_mtok_in / usd_per_mtok_out on that provider in \
             ~/.wizard/config.toml for the real figure."
        );
    }
    out
}

/// The whole `wizard usage` output for the log file at `path`, including the
/// "nothing here yet" line when the file does not exist (a fresh install has
/// no log, which is not an error). Split from [`run_cli`] so a test can drive
/// the read path over a fixture `usage.jsonl` instead of the caller's home
/// directory.
fn report_for_log(path: &Path, days: Option<u64>, now: u64) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(raw) => Ok(render_report(&raw, days, now)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(format!(
            "no usage recorded yet ({} does not exist)\n",
            path.display()
        )),
        Err(err) => Err(err).context(format!("reading {}", path.display())),
    }
}

/// `wizard usage [--since <days>d]`: per-project and per-provider rollup of
/// `~/.wizard/usage.jsonl`. Self-contained: no config load, no LLM.
pub fn run_cli(since: Option<&str>) -> Result<i32> {
    let days = since.map(parse_since_days).transpose()?;
    let path = default_log_path().context("could not resolve ~/.wizard")?;
    print!("{}", report_for_log(&path, days, unix_now())?);
    Ok(0)
}

/// Compact human form of a token count for status lines: `842 tok`,
/// `12.3k tok`, `4.2M tok`.
pub fn format_tokens(count: u64) -> String {
    if count < 1_000 {
        format!("{count} tok")
    } else if count < 1_000_000 {
        format!("{:.1}k tok", count as f64 / 1e3)
    } else {
        format!("{:.1}M tok", count as f64 / 1e6)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_accumulates_turn_and_session_totals() {
        let tracker = UsageTracker::new();
        assert_eq!(tracker.session_totals(), (0, 0));
        assert_eq!(tracker.last_prompt_tokens(), None);

        tracker.record(Some(100), Some(20));
        tracker.record(Some(150), Some(30));
        assert_eq!(tracker.turn_totals(), (250, 50));
        assert_eq!(tracker.session_totals(), (250, 50));
        assert_eq!(tracker.last_prompt_tokens(), Some(150));

        tracker.begin_turn();
        assert_eq!(tracker.turn_totals(), (0, 0));
        assert_eq!(tracker.session_totals(), (250, 50), "session survives");
        assert_eq!(
            tracker.last_prompt_tokens(),
            Some(150),
            "last prompt survives the turn boundary"
        );

        tracker.record(None, None);
        assert_eq!(tracker.session_totals(), (250, 50), "None records nothing");

        tracker.clear_last_prompt();
        assert_eq!(tracker.last_prompt_tokens(), None);

        tracker.record(Some(10), Some(5));
        tracker.clear_session();
        assert_eq!(tracker.session_totals(), (0, 0));
        assert_eq!(tracker.turn_totals(), (0, 0));
        assert_eq!(tracker.last_prompt_tokens(), None);
    }

    #[test]
    fn append_writes_one_json_line_per_record() {
        let dir = std::env::temp_dir().join(format!("wizard-usage-{}", uuid::Uuid::new_v4()));
        let path = dir.join("usage.jsonl");
        let record = UsageRecord {
            ts: 1_700_000_000,
            project: "/tmp/proj".to_string(),
            model: "qwen3-8b".to_string(),
            provider: "local".to_string(),
            prompt_tokens: 123,
            completion_tokens: 45,
            cache_read_tokens: 100,
            cache_write_tokens: 20,
            cost_usd: Some(0.5),
            price_source: PriceSource::Table,
            mode: "genie".to_string(),
        };
        append(&path, &record).expect("first append");
        append(&path, &record).expect("second append");

        let raw = std::fs::read_to_string(&path).expect("readable");
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 2);
        let parsed: UsageRecord = serde_json::from_str(lines[0]).expect("valid json");
        assert_eq!(parsed.prompt_tokens, 123);
        assert_eq!(parsed.completion_tokens, 45);
        assert_eq!(parsed.model, "qwen3-8b");
        assert_eq!(parsed.provider, "local");
        assert_eq!(parsed.mode, "genie");
        assert_eq!(parsed.cache_read_tokens, 100);
        assert_eq!(parsed.cache_write_tokens, 20);
        assert_eq!(parsed.cost_usd, Some(0.5));
        assert_eq!(parsed.price_source, PriceSource::Table);
        assert!(
            lines[0].contains("\"price_source\":\"table\""),
            "provenance is written in its wire form: {}",
            lines[0]
        );

        // A pre-cost-accounting line still parses; the new fields default.
        let legacy = r#"{"ts":1,"project":"/p","model":"m","provider":"local","prompt_tokens":5,"completion_tokens":2,"mode":"genie"}"#;
        let parsed: UsageRecord = serde_json::from_str(legacy).expect("legacy line parses");
        assert_eq!(parsed.cache_read_tokens, 0);
        assert_eq!(parsed.cost_usd, None);
        assert_eq!(parsed.price_source, PriceSource::Unpriced);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Flat tokens: no cache split, so this is the arithmetic it always was.
    fn flat(prompt: u64, completion: u64) -> TurnTokens {
        TurnTokens {
            prompt,
            completion,
            cache_read: 0,
            cache_write: 0,
        }
    }

    #[test]
    fn cost_requires_at_least_one_rate() {
        assert_eq!(cost_usd(flat(1_000_000, 1_000_000), None, None), None);
        assert_eq!(
            cost_usd(flat(1_000_000, 2_000_000), Some(3.0), Some(15.0)),
            Some(33.0)
        );
        assert_eq!(
            cost_usd(flat(2_000_000, 500_000), Some(1.0), None),
            Some(2.0)
        );
        assert_eq!(cost_usd(flat(0, 0), Some(3.0), Some(15.0)), Some(0.0));
    }

    /// `/cost` and the per-turn writer price the same session identically.
    ///
    /// They did not. `cost_usd` took a flat prompt count and billed all of it
    /// at the input rate, while `estimate_cost` — the function that writes
    /// every line of `usage.jsonl`, and so the one `wizard usage` sums —
    /// priced a cache read at a tenth of that. A long session is mostly cache
    /// reads, because the whole conversation is resent every step, so the two
    /// numbers for identical work came apart by nearly the full 10x.
    #[test]
    fn the_session_total_prices_a_cached_token_the_way_the_turn_writer_does() {
        // A million prompt tokens of which 900k were served from cache.
        let tokens = TurnTokens {
            prompt: 1_000_000,
            completion: 0,
            cache_read: 900_000,
            cache_write: 0,
        };

        let session = cost_usd(tokens, Some(3.0), Some(15.0)).expect("rates are configured");
        let per_turn = estimate_cost(
            tokens,
            &PriceInputs {
                model: "whatever",
                endpoint: "",
                self_hosted: false,
                usd_per_mtok_in: Some(3.0),
                usd_per_mtok_out: Some(15.0),
            },
        );
        assert!(
            (session - per_turn.usd).abs() < 1e-9,
            "/cost said {session} and the turn writer said {}",
            per_turn.usd
        );

        // And it is the cheaper, correct number: 100k fresh at $3/Mtok plus
        // 900k cached at a tenth of that, not 1M fresh.
        assert!(
            (session - (0.3 + 0.27)).abs() < 1e-9,
            "expected $0.57, got ${session}"
        );
        assert!(
            session < 3.0,
            "the old flat arithmetic would have said $3.00; got ${session}"
        );
    }

    #[test]
    fn token_formatting_scales() {
        assert_eq!(format_tokens(0), "0 tok");
        assert_eq!(format_tokens(842), "842 tok");
        assert_eq!(format_tokens(12_345), "12.3k tok");
        assert_eq!(format_tokens(4_200_000), "4.2M tok");
    }

    #[test]
    fn since_parsing_accepts_day_suffix_and_rejects_junk() {
        assert_eq!(parse_since_days("7d").unwrap(), 7);
        assert_eq!(parse_since_days("30D").unwrap(), 30);
        assert_eq!(parse_since_days(" 2 ").unwrap(), 2);
        assert!(parse_since_days("0d").is_err());
        assert!(parse_since_days("soon").is_err());
        assert!(parse_since_days("").is_err());
    }

    #[test]
    fn rollup_groups_by_key_and_sums_optional_cost() {
        let turn = |project: &str, provider: &str, cost: Option<f64>| LoggedTurn {
            ts: 1_700_000_000,
            project: project.to_string(),
            provider: provider.to_string(),
            prompt_tokens: 100,
            completion_tokens: 10,
            cache_read_tokens: 40,
            cost_usd: cost,
            price_source: cost.map(|_| "table".to_string()),
        };
        let turns = vec![
            turn("/a", "local", None),
            turn("/a", "claude", Some(0.25)),
            turn("/b", "claude", Some(0.50)),
            turn("", "", None),
        ];

        let by_project = roll_up(&turns, |t| t.project.as_str());
        assert_eq!(by_project.len(), 3);
        let a = &by_project["/a"];
        assert_eq!((a.turns, a.prompt, a.completion), (2, 200, 20));
        assert_eq!(a.cost_usd, Some(0.25));
        assert!(by_project.contains_key("(unknown)"), "empty key is labeled");

        let by_provider = roll_up(&turns, |t| t.provider.as_str());
        let claude = &by_provider["claude"];
        assert_eq!(claude.turns, 2);
        assert_eq!(claude.cost_usd, Some(0.75));
        assert_eq!(
            by_provider["local"].cost_usd, None,
            "no cost recorded stays None, not $0"
        );
    }

    /// Convenience: price a turn on a metered provider with no configured
    /// rates and no seller named, i.e. the ordinary path through the open
    /// table.
    fn priced(model: &str, tokens: TurnTokens) -> PricedTurn {
        priced_from(model, "", tokens)
    }

    /// The same, for a turn bought from a named endpoint — the only way to
    /// reach a [`SELLER_PRICES`] row.
    fn priced_from(model: &str, endpoint: &str, tokens: TurnTokens) -> PricedTurn {
        estimate_cost(
            tokens,
            &PriceInputs {
                model,
                endpoint,
                usd_per_mtok_in: None,
                usd_per_mtok_out: None,
                self_hosted: false,
            },
        )
    }

    #[test]
    fn tracker_accumulates_cache_tokens_alongside_the_totals() {
        let tracker = UsageTracker::new();
        assert_eq!(tracker.turn_cache_totals(), (0, 0));

        tracker.record(Some(1_000), Some(50));
        tracker.record_cache(800, 100);
        tracker.record(Some(1_200), Some(60));
        tracker.record_cache(1_100, 0);
        assert_eq!(tracker.turn_cache_totals(), (1_900, 100));
        assert_eq!(tracker.session_cache_totals(), (1_900, 100));

        tracker.begin_turn();
        assert_eq!(tracker.turn_cache_totals(), (0, 0), "turn counters reset");
        assert_eq!(
            tracker.session_cache_totals(),
            (1_900, 100),
            "session cache totals survive the turn boundary"
        );

        tracker.clear_session();
        assert_eq!(tracker.session_cache_totals(), (0, 0));
        assert_eq!(tracker.turn_cache_totals(), (0, 0));
    }

    #[test]
    fn a_cache_hit_costs_materially_less_than_the_same_turn_cold() {
        // Same 100k-token prompt twice: once fresh, once with 90% of it
        // served from the provider's cache.
        let cold = priced(
            "claude-opus-5",
            TurnTokens {
                prompt: 100_000,
                completion: 1_000,
                cache_read: 0,
                cache_write: 0,
            },
        );
        let warm = priced(
            "claude-opus-5",
            TurnTokens {
                prompt: 100_000,
                completion: 1_000,
                cache_read: 90_000,
                cache_write: 0,
            },
        );
        assert_eq!(cold.source, PriceSource::Table);
        assert_eq!(warm.source, PriceSource::Table);

        // $5/Mtok in: 100k fresh = $0.50, plus $0.025 of output.
        assert!((cold.usd - 0.525).abs() < 1e-9, "{cold:?}");
        // 10k fresh ($0.05) + 90k at 0.1x ($0.045) + output ($0.025).
        assert!((warm.usd - 0.12).abs() < 1e-9, "{warm:?}");
        assert!(
            warm.usd < cold.usd * 0.5,
            "a cache hit has to show up as a much smaller number, else the \
             column makes caching look pointless: cold {cold:?}, warm {warm:?}"
        );
        assert!(warm.usd > 0.0, "cache reads are cheap, not free: {warm:?}");

        // The write side runs the other way: seeding the cache costs a
        // premium over plain input.
        let seeding = priced(
            "claude-opus-5",
            TurnTokens {
                prompt: 100_000,
                completion: 1_000,
                cache_read: 0,
                cache_write: 90_000,
            },
        );
        assert!(
            seeding.usd > cold.usd,
            "a cache write is 1.25x input: {seeding:?} vs {cold:?}"
        );
    }

    /// The delegated path is the parent's path minus `last_prompt`, and that
    /// is the whole difference. A subagent run or an `/ultra` candidate that
    /// hit the provider's cache has to reach the same cache counters the
    /// parent's own calls do, because it is the parent's `UsageRecord` the
    /// tokens land on.
    ///
    /// `/ultra` is the case that makes this load-bearing rather than tidy: it
    /// fans N candidates that each re-send the same prefix, so the cached
    /// fraction of an ultra turn is far higher than an ordinary one, and a
    /// delegated call whose split never arrives bills that prefix N times at
    /// the full input rate.
    #[test]
    fn delegated_calls_carry_their_cache_split_onto_the_turn() {
        let tracker = UsageTracker::new();

        // The parent's own call: `record` then `record_cache`.
        tracker.record(Some(10_000), Some(200));
        tracker.record_cache(8_000, 1_000);
        assert_eq!(tracker.last_prompt_tokens(), Some(10_000));

        // A candidate the parent paid for: `record_delegated` then the very
        // same `record_cache`, which has no parent/delegated distinction to
        // draw because it never touches `last_prompt`.
        tracker.record_delegated(50_000, 400);
        tracker.record_cache(45_000, 0);

        assert_eq!(tracker.turn_totals(), (60_000, 600));
        assert_eq!(
            tracker.turn_cache_totals(),
            (53_000, 1_000),
            "the candidate's 45k cache read is on the turn, not just the parent's 8k"
        );
        assert_eq!(tracker.session_cache_totals(), (53_000, 1_000));
        assert_eq!(
            tracker.last_prompt_tokens(),
            Some(10_000),
            "a candidate's prompt is a different conversation and must not \
             move the parent's compaction trigger"
        );

        // What that difference is worth on the bill. The second figure is the
        // same turn with the delegated split dropped — what this priced as
        // before the threading existed.
        let (prompt, completion) = tracker.turn_totals();
        let (cache_read, cache_write) = tracker.turn_cache_totals();
        let threaded = priced(
            "claude-opus-5",
            TurnTokens {
                prompt,
                completion,
                cache_read,
                cache_write,
            },
        );
        let parent_only = priced(
            "claude-opus-5",
            TurnTokens {
                prompt,
                completion,
                cache_read: 8_000,
                cache_write,
            },
        );
        assert!(
            threaded.usd < parent_only.usd * 0.5,
            "dropping a delegated cache read bills the candidate's prefix at \
             the full input rate: {threaded:?} vs {parent_only:?}"
        );
    }

    #[test]
    fn an_unknown_model_is_priced_high_never_free() {
        let tokens = TurnTokens {
            prompt: 1_000_000,
            completion: 1_000_000,
            cache_read: 0,
            cache_write: 0,
        };
        let unknown = priced("some-model-nobody-has-heard-of", tokens);
        assert_eq!(unknown.source, PriceSource::Fallback);
        // The provenance is the whole reason the fallback is tolerable, so it
        // has to *differ* from a real entry rather than being a second way of
        // saying the same thing. `wizard usage` stars only `fallback` rows.
        assert_eq!(priced("gpt-5.6-sol", tokens).source, PriceSource::Table);
        assert_ne!(unknown.source, priced("gpt-5.6-sol", tokens).source);
        assert!(
            unknown.usd > 0.0,
            "an unknown model priced at zero reads as free, which is the most \
             misleading possible answer: {unknown:?}"
        );
        // Erring high: at least as expensive as the priciest model we know.
        let priciest = priced("claude-fable-5", tokens);
        assert!(
            unknown.usd >= priciest.usd,
            "the fallback must over-estimate: {unknown:?} vs {priciest:?}"
        );
    }

    #[test]
    fn price_table_covers_the_fallback() {
        // A ratchet, not a tautology: if someone adds a model pricier than
        // FALLBACK_PRICE without raising it, unknown models would quietly
        // start under-stating. Raise FALLBACK_PRICE when this fires.
        for (model, price) in PRICES {
            assert!(
                price.input <= FALLBACK_PRICE.input && price.output <= FALLBACK_PRICE.output,
                "{model} is pricier than the unknown-model fallback: {price:?}"
            );
        }
        for (seller, model, price) in SELLER_PRICES {
            assert!(
                price.input <= FALLBACK_PRICE.input && price.output <= FALLBACK_PRICE.output,
                "{model} on {seller} is pricier than the unknown-model fallback: {price:?}"
            );
        }
    }

    /// A million prompt tokens and a million completion tokens, all fresh, so
    /// the dollar figure that comes back *is* the pair of published rates
    /// added together. Each provider test below asserts that sum, which is
    /// the only way to check "the rate actually used" rather than "some rate
    /// was used".
    fn headline(model: &str) -> PricedTurn {
        headline_from(model, "")
    }

    /// [`headline`] for a turn bought from a named endpoint.
    fn headline_from(model: &str, endpoint: &str) -> PricedTurn {
        priced_from(
            model,
            endpoint,
            TurnTokens {
                prompt: 1_000_000,
                completion: 1_000_000,
                cache_read: 0,
                cache_write: 0,
            },
        )
    }

    /// A million prompt tokens of which 900k came from the cache, no output,
    /// so the figure isolates the cached-input rate: `0.1 * input + 0.9 *
    /// cache_read`, in dollars per million.
    fn warm(model: &str) -> PricedTurn {
        warm_from(model, "")
    }

    /// [`warm`] for a turn bought from a named endpoint.
    fn warm_from(model: &str, endpoint: &str) -> PricedTurn {
        priced_from(
            model,
            endpoint,
            TurnTokens {
                prompt: 1_000_000,
                completion: 0,
                cache_read: 900_000,
                cache_write: 0,
            },
        )
    }

    /// The unknown-model figure every assertion below has to differ from: if
    /// a provider's rate silently regressed to the fallback the test would
    /// still be asserting *a* number, and this is the number it would be.
    const FALLBACK_HEADLINE: f64 = 60.0; // $10 in + $50 out per million.

    #[track_caller]
    fn assert_rate(priced: PricedTurn, expected: f64, what: &str) {
        assert_eq!(
            priced.source,
            PriceSource::Table,
            "{what} must come from the table, not a guess: {priced:?}"
        );
        assert!(
            (priced.usd - expected).abs() < 1e-9,
            "{what}: expected ${expected}, got {priced:?}"
        );
    }

    #[test]
    fn openai_models_price_at_their_published_rates() {
        assert_rate(headline("gpt-5.6-sol"), 5.0 + 30.0, "gpt-5.6-sol");
        assert_rate(headline("gpt-5.6-terra"), 2.0 + 12.0, "gpt-5.6-terra");
        assert_rate(headline("gpt-5.6-luna"), 0.2 + 1.2, "gpt-5.6-luna");
        assert_rate(headline("gpt-5.3-codex"), 1.75 + 14.0, "gpt-5.3-codex");
        assert_rate(headline("gpt-5.4-mini"), 0.75 + 4.5, "gpt-5.4-mini");
        assert_rate(headline("gpt-5-nano"), 0.05 + 0.4, "gpt-5-nano");
        assert!(
            (headline("gpt-5.6-sol").usd - FALLBACK_HEADLINE).abs() > 1.0,
            "the whole point is that these no longer read as the fallback"
        );

        // Cached input is 0.1x: 100k fresh at $5 + 900k cached at $0.50.
        assert_rate(warm("gpt-5.6-sol"), 0.5 + 0.45, "gpt-5.6-sol cache read");

        // A cache *write* on an automatic cache costs exactly what a fresh
        // input token costs — there is no seeding premium to charge, unlike
        // Anthropic's 1.25x.
        let seeding = priced(
            "gpt-5.6-sol",
            TurnTokens {
                prompt: 1_000_000,
                completion: 0,
                cache_read: 0,
                cache_write: 900_000,
            },
        );
        assert!(
            (seeding.usd - headline("gpt-5.6-sol").usd + 30.0).abs() < 1e-9,
            "seeding an automatic cache is priced as plain input: {seeding:?}"
        );
    }

    #[test]
    fn xai_models_price_at_their_published_rates() {
        assert_rate(headline("grok-4.5"), 2.0 + 6.0, "grok-4.5");
        assert_rate(headline("grok-4.3"), 1.25 + 2.5, "grok-4.3");
        assert_rate(
            headline("grok-4.20-0309-reasoning"),
            1.25 + 2.5,
            "grok-4.20 snapshot",
        );
        assert_rate(headline("grok-build-0.1"), 1.0 + 2.0, "grok-build-0.1");

        // xAI's cached rate is 0.15x, not the 0.1x the Anthropic-derived
        // constructor would have assumed: 100k fresh at $2 + 900k at $0.30.
        let cached = warm("grok-4.5");
        assert_rate(cached, 0.2 + 0.27, "grok-4.5 cache read");
        assert!(
            cached.usd > 0.2 + 0.18,
            "the derived 0.1x rate would have under-billed a cached Grok turn: {cached:?}"
        );
    }

    #[test]
    fn gemini_models_price_at_their_published_rates() {
        assert_rate(headline("gemini-3.5-flash"), 1.5 + 9.0, "gemini-3.5-flash");
        assert_rate(headline("gemini-3.6-flash"), 1.5 + 7.5, "gemini-3.6-flash");
        assert_rate(
            headline("gemini-3.1-pro-preview"),
            2.0 + 12.0,
            "gemini-3.1-pro-preview",
        );
        assert_rate(headline("gemini-2.5-pro"), 1.25 + 10.0, "gemini-2.5-pro");
        assert_rate(headline("gemini-2.5-flash"), 0.3 + 2.5, "gemini-2.5-flash");
        assert_rate(
            warm("gemini-3.5-flash"),
            0.15 + 0.135,
            "gemini-3.5-flash cache",
        );

        // The substring hazard the `-lite` rows exist to close: without its
        // own entry, `gemini-2.5-flash-lite` matches `gemini-2.5-flash` and
        // is billed three times its real input rate.
        assert_rate(
            headline("gemini-2.5-flash-lite"),
            0.1 + 0.4,
            "gemini-2.5-flash-lite",
        );
        assert_rate(
            headline("gemini-3.5-flash-lite"),
            0.3 + 2.5,
            "gemini-3.5-flash-lite",
        );
        assert!(
            headline("gemini-2.5-flash-lite").usd < headline("gemini-2.5-flash").usd,
            "a lite model must not inherit its full sibling's price"
        );
    }

    #[test]
    fn deepseek_models_price_at_their_published_rates() {
        assert_rate(headline("deepseek-v4-pro"), 0.435 + 0.87, "deepseek-v4-pro");
        assert_rate(
            headline("deepseek-v4-flash"),
            0.14 + 0.28,
            "deepseek-v4-flash",
        );

        // DeepSeek's disk cache is the extreme case: a hit is 0.0083x a miss.
        // 100k at $0.435 + 900k at $0.003625.
        let cached = warm("deepseek-v4-pro");
        assert_rate(cached, 0.0435 + 0.003_262_5, "deepseek-v4-pro cache read");
        assert!(
            cached.usd < (0.0435 + 900_000.0 / 1e6 * 0.0435) * 0.6,
            "the derived 0.1x rate would have over-billed a cached DeepSeek \
             turn by an order of magnitude: {cached:?}"
        );
    }

    #[test]
    fn mistral_models_price_at_their_published_rates() {
        assert_rate(
            headline("mistral-medium-latest"),
            1.5 + 7.5,
            "mistral-medium-latest",
        );
        assert_rate(
            headline("mistral-large-latest"),
            0.5 + 1.5,
            "mistral-large-latest",
        );
        assert_rate(
            headline("mistral-small-latest"),
            0.15 + 0.6,
            "mistral-small-latest",
        );
        assert_rate(
            headline("devstral-medium-latest"),
            0.4 + 2.0,
            "devstral-medium-latest",
        );
        assert_rate(
            headline("devstral-small-latest"),
            0.1 + 0.3,
            "devstral-small-latest",
        );
        assert_rate(warm("mistral-medium-latest"), 0.15 + 0.135, "mistral cache");
    }

    #[test]
    fn groq_and_moonshot_price_at_their_published_rates() {
        assert_rate(
            headline("llama-3.3-70b-versatile"),
            0.59 + 0.79,
            "llama-3.3-70b-versatile on Groq",
        );
        // Groq publishes no cached-input rate for this model, so a cache read
        // costs what fresh input costs. Assuming a discount that may not
        // exist would under-bill, and an under-bill is the invisible error.
        assert_rate(
            warm("llama-3.3-70b-versatile"),
            0.59,
            "llama-3.3-70b-versatile cache read",
        );

        assert_rate(headline("kimi-k3"), 3.0 + 15.0, "kimi-k3");
        assert_rate(warm("kimi-k3"), 0.3 + 0.27, "kimi-k3 cache read");
    }

    /// Two ways a table entry can be worse than no entry. The first is
    /// [`lookup_price`] matching on a substring of the id; the second is it
    /// having no idea who sold the tokens, which [`SELLER_PRICES`] answers
    /// only for the pairings with a published rate — every other seller of a
    /// multi-seller id still has to land here.
    #[test]
    fn models_with_a_pricier_sibling_stay_on_the_fallback() {
        // Substring shadowing. Each of these is several times its base model,
        // so tabling the base id would bill a Pro turn at the base rate and
        // flag it `table`, which reads as fact. The fallback reads as a
        // guess, which is what it is.
        for model in ["gpt-5-pro", "gpt-5.2-pro", "gpt-5.4-pro", "gpt-5.5-pro"] {
            assert_eq!(
                lookup_price(model, ""),
                None,
                "{model} costs several times the base id whose key would match it"
            );
            assert_eq!(headline(model).source, PriceSource::Fallback);
        }

        // An open-weight model that four hosts sell at four prices is
        // unpriceable from the id alone. It is priced per seller in
        // SELLER_PRICES, and *only* per seller: with no endpoint to name one,
        // there is still no answer to give.
        for model in [
            "gpt-oss-120b",
            "openai/gpt-oss-120b",
            "accounts/fireworks/models/gpt-oss-120b",
        ] {
            assert_eq!(
                lookup_price(model, ""),
                None,
                "{model} has no single seller"
            );
        }

        // Offered by Wizard's Mistral preset but absent from Mistral's
        // published price list under that name.
        assert_eq!(lookup_price("devstral-2512", ""), None);
        // And the providers left whole on the fallback by policy.
        assert_eq!(lookup_price("@cf/zai-org/glm-5.2", ""), None);
        assert_eq!(lookup_price("minimax-m2.7", ""), None);
    }

    /// The motivating case for [`SELLER_PRICES`]: one model id, four sellers,
    /// four published rate cards. Every assertion here is a number no
    /// provider-blind table could produce, because satisfying any one of them
    /// with a single row breaks the others.
    #[test]
    fn one_model_id_prices_at_each_sellers_own_published_rate() {
        // The id spelling each preset actually sends (see `llm::compat`), so
        // this also pins that the key matches all three of them.
        let groq = headline_from("openai/gpt-oss-120b", "https://api.groq.com/openai/v1");
        let together = headline_from("openai/gpt-oss-120b", "https://api.together.xyz/v1");
        let fireworks = headline_from(
            "accounts/fireworks/models/gpt-oss-120b",
            "https://api.fireworks.ai/inference/v1",
        );
        let cerebras = headline_from("gpt-oss-120b", "https://api.cerebras.ai/v1");

        assert_rate(groq, 0.15 + 0.6, "gpt-oss-120b on Groq");
        assert_rate(together, 0.15 + 0.6, "gpt-oss-120b on Together");
        assert_rate(fireworks, 0.15 + 0.6, "gpt-oss-120b on Fireworks");
        assert_rate(cerebras, 0.35 + 0.75, "gpt-oss-120b on Cerebras");

        // The headline difference, which is the part a seller-blind lookup
        // cannot express at all: Cerebras charges 1.47x the others for the
        // same weights, so any single row is wrong by that much for someone.
        assert!(
            cerebras.usd > groq.usd * 1.4,
            "Cerebras is materially pricier than Groq for the same model; a \
             single row for the id would have to lie about one of them: \
             {cerebras:?} vs {groq:?}"
        );

        // And the cached-input rates, which differ even where the headline
        // rates agree — three sellers at the same $0.15/$0.60 and three
        // different answers for a 90%-cached turn. 100k fresh + 900k cached:
        //   Groq      0.015 + 0.9 * 0.075  (50% off, docs/prompt-caching)
        //   Fireworks 0.015 + 0.9 * 0.015  (its own cached column)
        //   Together  0.015 + 0.9 * 0.15   (publishes no cached rate)
        assert_rate(
            warm_from("openai/gpt-oss-120b", "https://api.groq.com/openai/v1"),
            0.015 + 0.067_5,
            "gpt-oss-120b cache read on Groq",
        );
        assert_rate(
            warm_from(
                "accounts/fireworks/models/gpt-oss-120b",
                "https://api.fireworks.ai/inference/v1",
            ),
            0.015 + 0.013_5,
            "gpt-oss-120b cache read on Fireworks",
        );
        assert_rate(
            warm_from("openai/gpt-oss-120b", "https://api.together.xyz/v1"),
            0.15,
            "gpt-oss-120b cache read on Together",
        );
    }

    /// The other half of the bargain. Keying on the seller is only an
    /// improvement if an *unknown* seller of a tabled id gets the visible
    /// fallback rather than the nearest row: a rate that belongs to someone
    /// else, flagged `table`, is the one error nothing downstream can spot.
    #[test]
    fn a_seller_with_no_row_falls_back_instead_of_borrowing_one() {
        for endpoint in [
            // Sells gpt-oss-120b, but at a margin over the host's own rate
            // and with no first-party per-token page of its own.
            "https://openrouter.ai/api/v1",
            // Sells it as the model's author, at rates not checked here.
            "https://api.openai.com/v1",
            // A gateway, a mirror, or a proxy in front of any of the four.
            "https://llm.internal.example/v1",
            // A lookalike host. Matching is host equality, not `contains`,
            // so this is a stranger and not Groq.
            "https://api.groq.com.someone-else.example/v1",
            // No endpoint offered at all.
            "",
        ] {
            let priced = headline_from("gpt-oss-120b", endpoint);
            assert_eq!(
                priced.source,
                PriceSource::Fallback,
                "{endpoint} has no published rate for gpt-oss-120b, so it must \
                 read as a guess and not as Groq's or Cerebras's number: {priced:?}"
            );
            assert!(
                (priced.usd - FALLBACK_HEADLINE).abs() < 1e-9,
                "{endpoint}: {priced:?}"
            );
            assert_eq!(lookup_price("gpt-oss-120b", endpoint), None);
        }

        // The converse, and the reason the seller column is not on every row:
        // an id that one vendor sells still prices from an endpoint nobody
        // tabled. Bedrock, Vertex, a corporate gateway — none of them change
        // what Anthropic charges, and requiring a known host would have
        // turned all of them into fallbacks.
        for endpoint in [
            "",
            "https://api.anthropic.com",
            "https://llm.internal.example/v1",
            "https://bedrock-runtime.us-east-1.amazonaws.com",
        ] {
            assert_rate(
                headline_from("claude-opus-5", endpoint),
                5.0 + 25.0,
                "claude-opus-5 costs what Anthropic charges wherever it is bought",
            );
        }
    }

    #[test]
    fn api_host_reads_the_seller_out_of_a_base_url() {
        assert_eq!(
            api_host("https://api.groq.com/openai/v1").as_deref(),
            Some("api.groq.com")
        );
        assert_eq!(
            api_host("https://API.Groq.COM/v1").as_deref(),
            Some("api.groq.com")
        );
        assert_eq!(
            api_host("https://api.groq.com:8443/v1").as_deref(),
            Some("api.groq.com")
        );
        assert_eq!(
            api_host("https://key:secret@api.groq.com/v1").as_deref(),
            Some("api.groq.com")
        );
        assert_eq!(
            api_host("  https://api.groq.com  ").as_deref(),
            Some("api.groq.com")
        );
        assert_eq!(api_host("api.groq.com/v1").as_deref(), Some("api.groq.com"));
        // A self-hosted backend: parsed fine, matches nothing, and never
        // reaches the table anyway because `self_hosted` short-circuits first.
        assert_eq!(
            api_host("http://127.0.0.1:11434/v1").as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(api_host("http://[::1]:11434/v1").as_deref(), Some("[::1]"));
        // Nothing to name a seller with.
        assert_eq!(api_host(""), None);
        assert_eq!(api_host("   "), None);
        assert_eq!(api_host("/v1/chat/completions"), None);
    }

    /// A seller row is only reachable if some provider can actually be
    /// pointed at that host, and only useful if it matches the model id that
    /// provider sends. Both halves drift silently otherwise: a typo'd host
    /// or a key that misses the preset's spelling looks exactly like a
    /// correctly-tabled model right up until the bill.
    #[test]
    fn every_seller_row_matches_a_preset_wizard_can_reach() {
        for (seller, key, _) in SELLER_PRICES {
            let preset = crate::llm::compat::PRESETS
                .iter()
                .find(|preset| api_host(preset.base_url).as_deref() == Some(*seller))
                .unwrap_or_else(|| {
                    panic!("no compat preset points at {seller}, so this row can never match")
                });
            assert!(
                preset
                    .models
                    .iter()
                    .any(|model| model.to_ascii_lowercase().contains(key)),
                "{seller} offers {:?}, none of which contains the key {key:?}",
                preset.models
            );
        }
    }

    #[test]
    fn price_table_keys_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for (key, _) in PRICES {
            assert!(seen.insert(*key), "duplicate price-table key {key:?}");
            assert_eq!(
                *key,
                key.to_ascii_lowercase(),
                "{key:?} can never match: lookup_price lowercases the id first"
            );
        }

        // Seller rows are keyed by the pair, so the same id may appear once
        // per host and no more: two rows for one pairing would resolve by
        // whichever the iterator saw last.
        let mut seen_pairs = std::collections::HashSet::new();
        for (seller, key, _) in SELLER_PRICES {
            assert!(
                seen_pairs.insert((*seller, *key)),
                "duplicate seller-table key {seller:?}/{key:?}"
            );
            assert_eq!(
                *key,
                key.to_ascii_lowercase(),
                "{key:?} can never match: lookup_price lowercases the id first"
            );
            assert_eq!(
                *seller,
                seller.to_ascii_lowercase(),
                "{seller:?} can never match: api_host lowercases the host first"
            );
        }
    }

    /// The invariant that keeps the split honest. An id in both tables would
    /// price from the open row for every seller that has no row of its own —
    /// which is precisely hazard 1, re-opened by the table that exists to
    /// close it. Overlap either way round (`gpt-oss` shadowing
    /// `gpt-oss-120b`, or the reverse) is the same bug.
    #[test]
    fn a_seller_row_never_shares_its_model_with_the_open_table() {
        for (seller, seller_key, _) in SELLER_PRICES {
            for (open_key, _) in PRICES {
                assert!(
                    !seller_key.contains(open_key) && !open_key.contains(seller_key),
                    "{seller}'s {seller_key:?} overlaps the open-table key \
                     {open_key:?}, which would answer for every other seller of it"
                );
            }
        }
    }

    #[test]
    fn configured_rates_outrank_the_table_and_the_local_shortcut() {
        let tokens = TurnTokens {
            prompt: 1_000_000,
            completion: 1_000_000,
            cache_read: 0,
            cache_write: 0,
        };
        let configured = estimate_cost(
            tokens,
            &PriceInputs {
                model: "claude-opus-5",
                endpoint: "https://api.anthropic.com",
                usd_per_mtok_in: Some(1.0),
                usd_per_mtok_out: Some(2.0),
                self_hosted: false,
            },
        );
        assert_eq!(configured.source, PriceSource::Config);
        assert!((configured.usd - 3.0).abs() < 1e-9, "{configured:?}");

        // Even on a self-hosted backend: the user typed the numbers.
        let local_with_rates = estimate_cost(
            tokens,
            &PriceInputs {
                model: "qwen3-8b",
                endpoint: "http://127.0.0.1:11435",
                usd_per_mtok_in: Some(1.0),
                usd_per_mtok_out: Some(2.0),
                self_hosted: true,
            },
        );
        assert_eq!(local_with_rates.source, PriceSource::Config);
        assert!((local_with_rates.usd - 3.0).abs() < 1e-9);
    }

    #[test]
    fn self_hosted_backends_cost_nothing_and_say_so() {
        let plain = estimate_cost(
            TurnTokens {
                prompt: 5_000_000,
                completion: 1_000_000,
                cache_read: 0,
                cache_write: 0,
            },
            &PriceInputs {
                model: "qwen3-8b",
                endpoint: "http://127.0.0.1:11435",
                usd_per_mtok_in: None,
                usd_per_mtok_out: None,
                self_hosted: true,
            },
        );
        assert_eq!(plain.source, PriceSource::Local);
        assert!((plain.usd - 0.0).abs() < f64::EPSILON);

        assert!(self_hosted(ProviderKind::LlamaCpp));
        assert!(self_hosted(ProviderKind::Ollama));
        assert!(!self_hosted(ProviderKind::Anthropic));
        assert!(!self_hosted(ProviderKind::Openai));
        assert!(!self_hosted(ProviderKind::ChatgptOauth));
    }

    #[test]
    fn model_ids_match_through_prefixes_suffixes_and_case() {
        let opus5 = lookup_price("claude-opus-5", "").expect("bare id");
        for id in [
            "anthropic.claude-opus-5",
            "us.anthropic.claude-opus-5-v1:0",
            "anthropic/claude-opus-5",
            "Claude-Opus-5",
        ] {
            assert_eq!(lookup_price(id, ""), Some(opus5), "{id} is the same model");
        }

        // Longest match wins, so an older dated id keeps its own price
        // instead of being shadowed by a shorter key.
        let opus45 = lookup_price("claude-opus-4-5-20251101", "").expect("dated id");
        assert_eq!(
            opus45,
            lookup_price("claude-opus-4-5", "").expect("bare 4.5")
        );
        assert_ne!(
            lookup_price("claude-haiku-4-5", ""),
            lookup_price("claude-opus-4-5", ""),
            "haiku is not opus-priced"
        );
        assert_eq!(lookup_price("qwen3-8b", ""), None);
    }

    #[test]
    fn cache_counts_reported_the_anthropic_way_do_not_undercharge() {
        // Anthropic reports input_tokens EXCLUDING the cached counts, OpenAI
        // reports prompt_tokens INCLUDING them. Both shapes describe the same
        // 1000-token prompt with 800 tokens read from cache, so both must
        // cost the same; the un-summed one must not bill zero fresh tokens.
        let inclusive = priced(
            "claude-opus-5",
            TurnTokens {
                prompt: 1_000,
                completion: 0,
                cache_read: 800,
                cache_write: 0,
            },
        );
        let exclusive = priced(
            "claude-opus-5",
            TurnTokens {
                prompt: 200,
                completion: 0,
                cache_read: 800,
                cache_write: 0,
            },
        );
        assert!(
            (inclusive.usd - exclusive.usd).abs() < 1e-12,
            "both conventions price the same turn the same: {inclusive:?} vs {exclusive:?}"
        );
        // 200 fresh at $5/Mtok + 800 cached at $0.50/Mtok.
        assert!(
            (inclusive.usd - (0.001 + 0.000_4)).abs() < 1e-12,
            "{inclusive:?}"
        );
    }

    /// One deep project path cannot widen every other row off the terminal.
    ///
    /// The first column used to be as wide as the longest key, unbounded. A
    /// 110-character temp directory in the log pushed the five number columns
    /// out to column 112, so every row wrapped in an 80-column terminal —
    /// including the short ones, which were the readable ones. The mesh peer
    /// list already had this guard; this table did not.
    #[test]
    fn one_deep_project_path_cannot_wrap_the_whole_table() {
        let deep = format!("/tmp/{}/probe", "nested-directory".repeat(8));
        assert!(deep.chars().count() > 120, "the fixture must be long");
        let log = format!(
            "{}\n{}\n",
            serde_json::json!({
                "ts": 1, "project": deep, "provider": "xai", "model": "grok-4.5",
                "prompt_tokens": 10, "completion_tokens": 1, "mode": "genie"
            }),
            serde_json::json!({
                "ts": 2, "project": "/short", "provider": "xai", "model": "grok-4.5",
                "prompt_tokens": 10, "completion_tokens": 1, "mode": "genie"
            }),
        );

        let report = render_report(&log, None, 3);
        let widest = report.lines().map(|l| l.chars().count()).max().unwrap_or(0);
        assert!(
            widest <= 100,
            "the widest line is {widest} characters, which wraps a normal terminal:\n{report}"
        );
        assert!(
            report.contains('…'),
            "the long path should be elided, not dropped:\n{report}"
        );
        // Elided from the head, so the part that tells two sibling projects
        // apart survives.
        assert!(
            report.contains("/probe"),
            "the tail of the path is what distinguishes it:\n{report}"
        );
        assert!(
            report.contains("/short"),
            "a short path is untouched:\n{report}"
        );
    }

    #[test]
    fn usage_report_prints_a_real_cost_column() {
        let now = 1_800_000_000;
        let log = format!(
            "{{\"ts\":{now},\"project\":\"/proj/a\",\"model\":\"claude-opus-5\",\"provider\":\"claude\",\
              \"prompt_tokens\":100000,\"completion_tokens\":1000,\"cache_read_tokens\":90000,\
              \"cache_write_tokens\":0,\"cost_usd\":0.12,\"price_source\":\"table\",\"mode\":\"genie\"}}\n\
             {{\"ts\":{now},\"project\":\"/proj/b\",\"model\":\"mystery-1\",\"provider\":\"custom\",\
              \"prompt_tokens\":1000000,\"completion_tokens\":0,\"cost_usd\":10.0,\
              \"price_source\":\"fallback\",\"mode\":\"genie\"}}\n\
             {{\"ts\":{now},\"project\":\"/proj/c\",\"model\":\"qwen3-8b\",\"provider\":\"local\",\
              \"prompt_tokens\":50,\"completion_tokens\":5,\"mode\":\"genie\"}}\n"
        );

        let report = render_report(&log, None, now);
        assert!(report.contains("3 turn(s)"), "{report}");
        assert!(report.contains("cost"), "the column has a header: {report}");
        assert!(
            report.contains("cached"),
            "cache tokens are shown: {report}"
        );
        assert!(
            report.contains("$0.1200"),
            "sub-dollar costs keep enough precision to not read as free: {report}"
        );
        assert!(
            report.contains("$10.00"),
            "dollar costs round to cents: {report}"
        );
        assert!(
            report.contains("90.0k tok"),
            "the cached column carries the cache-read total: {report}"
        );
        assert!(
            report.contains("$10.00*") && report.contains("* estimated"),
            "a fallback-priced row is flagged and footnoted: {report}"
        );
        assert!(
            report.contains("  -"),
            "a record written before cost accounting stays blank, not $0: {report}"
        );

        // The window still filters, and an empty window says so.
        let older = format!("{}\n", log.replace(&format!("\"ts\":{now}"), "\"ts\":10"));
        let report = render_report(&older, Some(7), now);
        assert_eq!(report, "no usage recorded in the last 7 day(s)\n");
        assert_eq!(render_report("", None, now), "no usage recorded yet\n");
    }

    #[test]
    fn a_sub_cent_total_renders_as_visible_not_as_zero() {
        assert_eq!(format_usd(12.345), "$12.35");
        assert_eq!(format_usd(1.0), "$1.00");
        assert_eq!(format_usd(0.12), "$0.1200");
        assert_eq!(format_usd(0.0001), "$0.0001");
        // A few hundred cheap tokens still has to read as "some money spent".
        // Rendering it `$0.0000` says *free*, which is the same lie as the
        // blank column this replaced, just with more decimal places.
        assert_eq!(format_usd(0.000_004), "<$0.0001");
        assert_eq!(format_usd(0.000_099), "<$0.0001");
        // Exactly zero is the one honest zero: a self-hosted turn cost nothing.
        assert_eq!(format_usd(0.0), "$0.0000");
    }

    #[test]
    fn usage_report_reads_a_fixture_log_from_disk() {
        let dir = std::env::temp_dir().join(format!("wizard-usage-cli-{}", uuid::Uuid::new_v4()));
        let path = dir.join("usage.jsonl");
        let now = 1_800_000_000;

        // A log that does not exist yet is the state of every fresh install,
        // and is not an error.
        let missing = report_for_log(&path, None, now).expect("missing log is not an error");
        assert!(missing.contains("no usage recorded yet"), "{missing}");

        // Build the fixture the way the agent does: price the turn, then
        // append the record. Driving the writer keeps this test honest about
        // the whole pipeline instead of about a hand-written line.
        let write =
            |project: &str, model: &str, provider: &str, tokens: TurnTokens, local: bool| {
                let priced = estimate_cost(
                    tokens,
                    &PriceInputs {
                        model,
                        endpoint: "",
                        usd_per_mtok_in: None,
                        usd_per_mtok_out: None,
                        self_hosted: local,
                    },
                );
                let record = UsageRecord {
                    ts: now,
                    project: project.to_string(),
                    model: model.to_string(),
                    provider: provider.to_string(),
                    prompt_tokens: tokens.prompt,
                    completion_tokens: tokens.completion,
                    cache_read_tokens: tokens.cache_read,
                    cache_write_tokens: tokens.cache_write,
                    cost_usd: Some(priced.usd),
                    price_source: priced.source,
                    mode: "genie".to_string(),
                };
                append(&path, &record).expect("appending the fixture");
            };

        write(
            "/proj/warm",
            "claude-opus-5",
            "anthropic",
            TurnTokens {
                prompt: 100_000,
                completion: 1_000,
                cache_read: 90_000,
                cache_write: 0,
            },
            false,
        );
        write(
            "/proj/mystery",
            "mystery-1",
            "custom",
            TurnTokens {
                prompt: 1_000_000,
                completion: 0,
                cache_read: 0,
                cache_write: 0,
            },
            false,
        );
        write(
            "/proj/local",
            "qwen3-8b",
            "local",
            TurnTokens {
                prompt: 5_000,
                completion: 100,
                cache_read: 0,
                cache_write: 0,
            },
            true,
        );

        let report = report_for_log(&path, None, now).expect("readable fixture");
        assert!(report.contains("3 turn(s)"), "{report}");
        // 10k fresh at $5/Mtok + 90k cached at $0.50/Mtok + 1k out at $25/Mtok.
        assert!(
            report.contains("$0.1200"),
            "the cached turn's real cost reaches the column: {report}"
        );
        assert!(
            report.contains("$10.00*") && report.contains("* estimated"),
            "an unknown model is priced and flagged, never blank: {report}"
        );
        assert!(
            report.contains("$0.0000"),
            "a self-hosted turn is the one honest zero: {report}"
        );
        assert!(
            !report.contains(" -\n"),
            "every row this version wrote carries a cost: {report}"
        );
        assert!(
            report.contains("90.0k tok"),
            "the cached-token column is populated: {report}"
        );

        // The `--since` window still filters the same file.
        let empty = report_for_log(&path, Some(1), now + 30 * 86_400).expect("readable");
        assert_eq!(empty, "no usage recorded in the last 1 day(s)\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn logged_turn_parses_current_records_and_tolerates_extras() {
        // A record exactly as `append` writes it today (no cost_usd).
        let line = r#"{"ts":1700000000,"project":"/p","model":"m","provider":"local","prompt_tokens":5,"completion_tokens":2,"mode":"genie"}"#;
        let turn: LoggedTurn = serde_json::from_str(line).expect("parses");
        assert_eq!(turn.prompt_tokens, 5);
        assert_eq!(turn.cost_usd, None);

        // Future records may carry cost_usd and new fields.
        let line = r#"{"ts":1,"project":"/p","provider":"x","prompt_tokens":1,"completion_tokens":1,"cost_usd":0.1,"new_field":true}"#;
        let turn: LoggedTurn = serde_json::from_str(line).expect("parses");
        assert_eq!(turn.cost_usd, Some(0.1));

        // A price_source this version has never heard of must not cost the
        // reader the whole line: it is read as an opaque string.
        let line = r#"{"ts":1,"project":"/p","provider":"x","prompt_tokens":1,"completion_tokens":1,"cache_read_tokens":1,"cost_usd":0.1,"price_source":"some_future_source"}"#;
        let turn: LoggedTurn = serde_json::from_str(line).expect("unknown source still parses");
        assert_eq!(turn.price_source.as_deref(), Some("some_future_source"));
        assert_eq!(turn.cache_read_tokens, 1);
        let mut rollup = Rollup::default();
        rollup.add(&turn);
        assert!(!rollup.estimated, "only 'fallback' flags a row as a guess");
    }
}
