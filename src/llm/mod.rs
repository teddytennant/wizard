//! LLM wire types matching Ollama's **native** `/api/chat` schema
//! (not the OpenAI-compatible shim). Shared by the agent loop, the tool
//! registry, and the TUI.

pub mod compat;
pub mod fusion;
pub mod oauth_callback;
pub mod provider;
pub mod registry;
#[cfg(test)]
pub(crate) mod test_support;
pub mod wire;
pub mod xai_oauth;

use std::cell::Cell;
use std::pin::Pin;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use futures_util::Stream;
use serde::{Deserialize, Serialize};

/// Boxed stream of [`ChatChunk`]s yielded by every provider's `chat_stream`.
/// Shared by [`wire`] and by every provider plugin.
pub type ChatStream = Pin<Box<dyn Stream<Item = Result<ChatChunk>> + Send>>;

/// How long to wait for a TCP connection before giving up. Reaching the peer
/// is fast or it is not happening, wherever the peer lives, so both
/// localities share this.
const CHAT_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// How often idle connections are keepalive-probed, so a dead peer is noticed
/// rather than held open.
const CHAT_TCP_KEEPALIVE: Duration = Duration::from_secs(30);
/// How long a *cloud* chat client tolerates a completely silent socket
/// mid-response. A live SSE stream never goes minutes without a frame (even
/// keep-alive comments count as reads), so five minutes of silence from a
/// hosted API means the connection is dead, not busy.
const CLOUD_READ_TIMEOUT: Duration = Duration::from_secs(300);

/// Where the endpoint a chat client talks to actually runs. The two
/// localities differ on exactly one thing: what a silent socket means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Locality {
    /// A hosted API across the network. Silence is a stall, and the read
    /// timeout is what turns a hung connection into a transient error the
    /// agent's backoff-retry loop can act on.
    Cloud,
    /// An inference server on this machine (llama.cpp and friends). Silence
    /// is *normal* here: a large GGUF on weak hardware can prefill a long
    /// prompt for many minutes before it emits its first token, and there is
    /// no network in between to go wrong. A read timeout does not detect a
    /// dead peer in that setting, it kills a working one, so the local
    /// policy keeps only the connect timeout, which still fails fast when the
    /// server is not up at all.
    Local,
}

thread_local! {
    /// Locality the next chat client built on this thread is given. Cloud is
    /// the default because every hosted provider builds its client eagerly at
    /// construction; [`with_local_inference_timeouts`] flips it for the
    /// duration of one constructor.
    static CLIENT_LOCALITY: Cell<Locality> = const { Cell::new(Locality::Cloud) };
}

/// Build the chat client(s) `build` constructs under the local-inference
/// timeout policy instead of the cloud one.
///
/// The provider adapters build their `reqwest::Client` inside their own
/// constructors and expose no seam to inject one, so the locality is
/// announced *around* the construction rather than passed into it. `build`
/// runs to completion synchronously on this thread, so the scope covers
/// exactly the clients it creates, and the previous locality is restored even
/// if `build` panics.
///
/// The only caller is the llama.cpp plugin, so a build without it has nothing
/// to flip the locality for — dead code, not a mistake. The tests below still
/// exercise the scope itself either way.
#[cfg_attr(not(feature = "provider-llamacpp"), allow(dead_code))]
pub(crate) fn with_local_inference_timeouts<T>(build: impl FnOnce() -> T) -> T {
    struct Restore(Locality);
    impl Drop for Restore {
        fn drop(&mut self) {
            CLIENT_LOCALITY.set(self.0);
        }
    }
    let _restore = Restore(CLIENT_LOCALITY.get());
    CLIENT_LOCALITY.set(Locality::Local);
    build()
}

/// The read timeout the next chat client built on this thread will carry;
/// `None` when silence is not treated as failure (see [`Locality::Local`]).
pub(crate) fn client_read_timeout() -> Option<Duration> {
    match CLIENT_LOCALITY.get() {
        Locality::Cloud => Some(CLOUD_READ_TIMEOUT),
        Locality::Local => None,
    }
}

/// Where the endpoint at `base_url` runs, judged from the address alone.
///
/// The provider *kind* is not enough to answer this. llama.cpp is not the
/// only inference server that runs on the user's own machine: LM Studio,
/// vLLM and text-generation-webui all speak the OpenAI wire shape, so they
/// are configured as an `openai` provider pointed at
/// `http://127.0.0.1:1234/v1` and would otherwise inherit the cloud read
/// timeout that [`Locality::Local`] exists to remove.
///
/// What actually decides it is the address: loopback, a private or
/// link-local range, and the name forms that cannot resolve past the LAN.
pub(crate) fn endpoint_locality(base_url: &str) -> Locality {
    match url_host(base_url) {
        Some(host) if host_is_local(&host) => Locality::Local,
        _ => Locality::Cloud,
    }
}

/// Host of `base_url`, lowercased, with the scheme, userinfo, port and path
/// stripped (and IPv6 brackets removed). `None` when the string carries
/// nothing host-shaped, which is treated as [`Locality::Cloud`]: the
/// policy that fails safe.
fn url_host(base_url: &str) -> Option<String> {
    let rest = base_url
        .split_once("://")
        .map_or(base_url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = if let Some(inner) = authority.strip_prefix('[') {
        // `[::1]:8080`: the brackets exist precisely because a bare IPv6
        // literal cannot be split on the port colon.
        inner.split_once(']').map(|(host, _)| host)?
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Whether `host` (already lowercased) names something that cannot be reached
/// past this machine or its LAN.
fn host_is_local(host: &str) -> bool {
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    if let Ok(ip) = host.parse::<std::net::Ipv4Addr>() {
        // `0.0.0.0` is how a good many people write down the address of a
        // server they started on this machine.
        return ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_unspecified();
    }
    if let Ok(ip) = host.parse::<std::net::Ipv6Addr>() {
        // `is_unique_local` and `is_unicast_link_local` are still unstable, so
        // fc00::/7 and fe80::/10 are matched on the prefix directly.
        let first = ip.segments()[0];
        return ip.is_loopback()
            || ip.is_unspecified()
            || (first & 0xfe00) == 0xfc00
            || (first & 0xffc0) == 0xfe80;
    }
    if [".local", ".lan", ".internal", ".home.arpa"]
        .iter()
        .any(|suffix| host.ends_with(suffix))
    {
        return true;
    }
    // A single-label name (`gpu-box`) has no public TLD to be reached
    // through, so it resolves only via /etc/hosts, mDNS, or a LAN search
    // domain.
    !host.contains('.')
}

/// The read timeout a chat client for `base_url` should carry.
///
/// `None` (no stall detector) when *either* signal says the endpoint is
/// local: the address itself, or the [`with_local_inference_timeouts`] scope
/// the client is being built in. Neither subsumes the other: a hosted
/// provider kind can be pointed at loopback, and a llama-server can be
/// reached over a public name (an SSH tunnel, a Tailscale hostname), so a
/// client is given the cloud policy only when both say cloud.
pub(crate) fn client_read_timeout_for(base_url: &str) -> Option<Duration> {
    match endpoint_locality(base_url) {
        Locality::Local => None,
        Locality::Cloud => client_read_timeout(),
    }
}

/// HTTP client builder for the chat backends. A generation can legitimately
/// stream for many minutes, so there is never an overall request timeout;
/// instead the client fails fast when it can't connect and keepalive-probes
/// idle connections. Whether it *also* errors out of a silent stream is the
/// caller's `read_timeout`, which comes from [`client_read_timeout_for`];
/// see [`Locality`]. Every chat backend builds through here, Ollama included:
/// "the local one" points at another machine often enough that it needs the
/// same stall detector, and `client_read_timeout_for` is what keeps a genuinely
/// local host exempt.
pub(crate) fn chat_http_builder(read_timeout: Option<Duration>) -> reqwest::ClientBuilder {
    let builder = reqwest::Client::builder()
        .connect_timeout(CHAT_CONNECT_TIMEOUT)
        .tcp_keepalive(CHAT_TCP_KEEPALIVE);
    match read_timeout {
        Some(read_timeout) => builder.read_timeout(read_timeout),
        None => builder,
    }
}

/// Whole-request ceiling for an OAuth token endpoint call.
///
/// A token exchange or refresh is a small form POST answered in well under a
/// second; nothing about it streams, so unlike a chat completion it *can* be
/// given a total timeout, and it has to be. The refresh happens on the way
/// into a model call, under the token source's mutex, so a token endpoint that
/// accepts the connection and then never answers — a black-holed route, a
/// half-open NAT binding, a load balancer holding the socket — parks the turn
/// forever with no error to classify and no retry to make. That is
/// indistinguishable from the agent having decided to stop.
const OAUTH_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// HTTP client builder for an OAuth token endpoint, with `timeout` as the
/// whole-request ceiling. Separate from [`oauth_http_client`] only so a test
/// can build the same policy with a ceiling it can wait for; production has
/// exactly one caller and exactly one number.
pub(crate) fn oauth_http_builder(timeout: Duration) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .connect_timeout(CHAT_CONNECT_TIMEOUT)
        .timeout(timeout)
}

/// The HTTP client every OAuth token-endpoint call goes through.
///
/// Built per call site rather than shared, because these are rare (a sign-in,
/// a refresh every hour or so) and a client is cheap next to the TLS handshake
/// it is about to do. What matters is that none of them is a bare
/// `reqwest::Client::new()`: that client has **no** connect timeout and **no**
/// request timeout, which is the one configuration under which a refresh can
/// hang for the lifetime of the process.
pub(crate) fn oauth_http_client() -> reqwest::Client {
    oauth_http_builder(OAUTH_REQUEST_TIMEOUT)
        .build()
        // Builder construction only fails when the TLS backend cannot
        // initialize; a default client is worse but still works, and a panic
        // here would take down a sign-in that might otherwise have succeeded.
        .unwrap_or_default()
}

/// Hard cap on a server-stated `Retry-After`. A hostile or buggy header must
/// not be able to park a turn for an hour: past two minutes we stop believing
/// the header and fall back to our own backoff ladder, which will come round
/// again soon enough.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(120);

/// A wait the server itself asked for (HTTP `Retry-After`), already capped at
/// [`MAX_RETRY_AFTER`]. The agent's retry loop treats it as a floor: retrying
/// before a 429's stated deadline just burns another billed prompt.
///
/// It rides the `anyhow` chain *underneath* the [`ProviderError`] rather than
/// living on it as a field, so the provider error stays the head of the chain
/// and the message users see is unchanged. Reach it with
/// `err.downcast_ref::<RetryAfter>()`, the same way `ProviderError` is
/// reached.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("server asked to wait {}s before retrying", .0.as_secs())]
pub struct RetryAfter(pub Duration);

/// Parse an HTTP `Retry-After` header value, in either form the spec allows:
/// delta-seconds (`120`) or an HTTP-date (`Wed, 21 Oct 2015 07:28:00 GMT`).
/// `now` is the reference the date form is measured against: the caller
/// passes `SystemTime::now()`; tests pass a fixed instant.
///
/// The result is always clamped into `0..=`[`MAX_RETRY_AFTER`]: a date in the
/// past means "retry now", and a date far in the future is not allowed to
/// park the turn. Anything unparseable yields `None`, which falls back to the
/// agent's own backoff.
pub fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    // delta-seconds: the overwhelmingly common form, and the only one most
    // APIs send.
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds).min(MAX_RETRY_AFTER));
    }
    let deadline = parse_http_date(value)?;
    // A deadline already in the past is not an error, it is "go ahead".
    let wait = deadline.duration_since(now).unwrap_or(Duration::ZERO);
    Some(wait.min(MAX_RETRY_AFTER))
}

/// Parse an HTTP-date. IMF-fixdate is what servers actually send; the RFC
/// 2822 spellings with a numeric offset are accepted too because proxies
/// sometimes rewrite the zone.
fn parse_http_date(value: &str) -> Option<SystemTime> {
    use chrono::{NaiveDateTime, TimeZone, Utc};

    let stamp = chrono::DateTime::parse_from_rfc2822(value)
        .map(|parsed| parsed.timestamp())
        .or_else(|_| {
            NaiveDateTime::parse_from_str(value, "%a, %d %b %Y %H:%M:%S GMT")
                .map(|naive| Utc.from_utc_datetime(&naive).timestamp())
        })
        .ok()?;
    // Pre-epoch dates are nonsense here and would underflow the addition.
    let seconds = u64::try_from(stamp).ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds))
}

/// The `Retry-After` a response carried, if it sent a usable one. Providers
/// call this before consuming the response body, then hand the result to
/// [`http_error_with_retry_after`].
pub fn retry_after_from_headers(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    parse_retry_after(value, SystemTime::now())
}

/// Typed error for a non-success HTTP `status` that also carries the server's
/// `Retry-After` when it sent one. The [`ProviderError`] stays the head of
/// the chain (so `is_transient` and the user-facing message are unchanged);
/// the [`RetryAfter`] rides underneath for the agent's retry loop.
pub fn http_error_with_retry_after(
    status: u16,
    message: impl Into<String>,
    retry_after: Option<Duration>,
) -> anyhow::Error {
    let error = ProviderError::http(status, message);
    match retry_after {
        Some(delay) => anyhow::Error::new(RetryAfter(delay)).context(error),
        None => anyhow::Error::new(error),
    }
}

/// Extra window spread over a server-stated `Retry-After`. The server's time
/// is a floor we must not retry *before*, so the jitter is added on top of it
/// rather than sampled from zero; a second is enough to unbunch a fleet
/// without meaningfully delaying anyone.
pub const RETRY_AFTER_JITTER: Duration = Duration::from_millis(1_000);

/// How long to wait before retry attempt `attempt` (0-based) against an LLM
/// endpoint, given the configured backoff ladder and whatever the server said.
///
/// Two things happen here that a plain `min(max, base * 2^attempt)` does not.
///
/// **A server-stated [`RetryAfter`] raises the floor.** A 429 that names a
/// deadline is the endpoint telling us when it will serve us again; retrying
/// earlier just re-bills the whole prompt for another 429. So it is taken as
/// a lower bound on the wait, capped at [`MAX_RETRY_AFTER`] so a hostile or
/// buggy header cannot park a turn for an hour. It does not *replace* the
/// ladder: a `Retry-After: 0` (or an HTTP-date already in the past, which
/// parses to the same zero) would otherwise hand a misconfigured proxy full
/// control of the cadence and pin the loop at sub-second retries no matter
/// how many attempts had already failed.
///
/// **Jitter above a floor.** The ladder alone is deterministic, so a fleet of
/// workers or a batch of parallel subagents pointed at one endpoint retry in
/// lockstep: they fail together, sleep the same number of seconds, and hit it
/// together again. Sampling uniformly from `[base, ceiling]` spreads them
/// out. The draw starts at `base_secs` rather than at zero (AWS's "full
/// jitter") because full jitter assumes many callers sharing one endpoint: for
/// the single interactive user this usually is, it turns the configured
/// `retry_base_secs` into an average of half that and can re-hit a
/// rate-limited endpoint milliseconds after it said no.
pub fn retry_delay(
    attempt: u32,
    base_secs: u64,
    max_secs: u64,
    retry_after: Option<Duration>,
) -> Duration {
    let ladder = ladder_delay(attempt, base_secs, max_secs);
    match retry_after {
        // Capped when parsed; capped again here so a hand-built value cannot
        // slip past this function.
        Some(after) => {
            after.min(MAX_RETRY_AFTER).max(ladder) + scale(RETRY_AFTER_JITTER, unit_random())
        }
        None => ladder,
    }
}

/// The backoff ladder's own wait for `attempt`: a uniform draw from
/// `[base_secs, min(max_secs, base_secs * 2^attempt)]`.
fn ladder_delay(attempt: u32, base_secs: u64, max_secs: u64) -> Duration {
    let ceiling = max_secs.min(base_secs.saturating_mul(2u64.saturating_pow(attempt)));
    // A ladder configured with `max < base` (and the zero ladder the tests
    // use) still has to produce a coherent window.
    let floor = base_secs.min(ceiling);
    Duration::from_secs(floor) + scale(Duration::from_secs(ceiling - floor), unit_random())
}

/// `window` scaled by `fraction` (`0.0..1.0`), at millisecond resolution,
/// fine enough that two retriers drawing at the same moment land on different
/// values.
fn scale(window: Duration, fraction: f64) -> Duration {
    Duration::from_millis((window.as_millis() as f64 * fraction) as u64)
}

/// A uniform random `f64` in `[0.0, 1.0)`, drawn from the OS entropy source.
/// `getrandom` is already a dependency (OAuth PKCE, sync keys) and the project
/// takes no `rand` dependency, so this is the cheapest honest source.
fn unit_random() -> f64 {
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        // Entropy unavailable (a locked-down sandbox with no /dev/urandom):
        // take the full window rather than a fixed fraction, so a failed draw
        // becomes a longer wait and never a thundering herd at zero.
        return 1.0;
    }
    // 53 bits is the whole f64 mantissa, so the division is exact.
    (u64::from_le_bytes(bytes) >> 11) as f64 / (1u64 << 53) as f64
}

/// Conservative `max_tokens` for an Anthropic-compatible model this table
/// does not know. Every Claude model since 3.5 accepts at least this much, so
/// an unrecognized tag (a new release, a proxy's own naming) gets a request
/// that is smaller than it could be rather than a hard 400 that kills the
/// turn with no retry.
pub const DEFAULT_ANTHROPIC_MAX_TOKENS: u32 = 8_192;

/// Largest `max_tokens` an Anthropic-compatible model will accept.
///
/// The Messages API has no implicit cap (`max_tokens` is required on every
/// request), and a value above the model's own ceiling is a 400, which
/// [`ProviderError::is_transient`] correctly classifies as permanent, so the
/// turn dies outright. One hardcoded number therefore cannot serve a fleet
/// that spans model generations; the ceiling belongs here, next to the other
/// shared per-model tables, so every caller sees the same answer.
///
/// Matching is on a lowercased *substring* so vendor-prefixed tags
/// (`anthropic.claude-opus-5` on Bedrock) and dated snapshots
/// (`claude-3-5-sonnet-20241022`) resolve to the same entry as the bare
/// alias. Order is most-specific-first; unknown tags fall through to
/// [`DEFAULT_ANTHROPIC_MAX_TOKENS`].
pub fn anthropic_max_output_tokens(model: &str) -> u32 {
    let model = model.to_ascii_lowercase();
    // 128k output: the 4.6-and-newer Opus/Sonnet line, plus Fable and Mythos.
    if model.contains("fable")
        || model.contains("mythos")
        || model.contains("opus-5")
        || model.contains("opus-4-8")
        || model.contains("opus-4-7")
        || model.contains("opus-4-6")
        || model.contains("sonnet-5")
        || model.contains("sonnet-4-6")
    {
        return 128_000;
    }
    // 64k output: Haiku 4.5, the 4.5 Opus/Sonnet pair, Sonnet 4, Sonnet 3.7.
    if model.contains("haiku-4-5")
        || model.contains("opus-4-5")
        || model.contains("sonnet-4-5")
        || model.contains("sonnet-4")
        || model.contains("3-7-sonnet")
    {
        return 64_000;
    }
    // 32k output: Opus 4 and 4.1.
    if model.contains("opus-4") {
        return 32_000;
    }
    // 8k output: the 3.5 generation.
    if model.contains("3-5-sonnet") || model.contains("3-5-haiku") {
        return 8_192;
    }
    // 4k output: the Claude 3 generation.
    if model.contains("3-opus") || model.contains("3-sonnet") || model.contains("3-haiku") {
        return 4_096;
    }
    DEFAULT_ANTHROPIC_MAX_TOKENS
}

/// Whether a provider's finish reason means "the model ran out of room"
/// rather than "the model finished". Every backend spells it differently:
/// OpenAI-compatible endpoints and Ollama say `length`, Anthropic says
/// `max_tokens`, the Responses API says `max_output_tokens`, and a context
/// window that overflowed mid-reply is `model_context_window_exceeded` or
/// `context_length_exceeded`.
///
/// The two kinds of "no room" have different remedies, which is what
/// [`is_context_overflow`] is for, but they have the same consequence for the
/// only caller of this predicate: the reply stopped in the middle, so a tool
/// call inside it was cut off in the middle, and dispatching a half-decoded
/// arguments object runs a *different* action (see [`TruncatedToolCall`]).
/// Narrowing this to the output ceiling alone did not make the overflow case
/// recoverable, it made it dispatch: `execute` ran with `{}`. Refusing both is
/// the safe half of the trade, and the error says which condition it was and
/// what fixes it.
pub fn is_length_cutoff(done_reason: &str) -> bool {
    let reason = done_reason.trim().to_ascii_lowercase();
    matches!(
        reason.as_str(),
        "length" | "max_tokens" | "max_output_tokens"
    ) || is_context_overflow(done_reason)
}

/// Whether a finish reason means the *conversation* no longer fits the
/// model's context window, as opposed to the reply hitting its own
/// output-token ceiling.
///
/// The distinction is the user's next move, and it is the opposite one in
/// each case: an output ceiling is a per-reply budget the next request gets
/// again in full (ask for less in one go), while an overflow is the history
/// itself being too big (compact it, or start a new session). Exposed so a
/// caller that can drive compaction, rather than only report, can tell them
/// apart.
pub fn is_context_overflow(done_reason: &str) -> bool {
    matches!(
        done_reason.trim().to_ascii_lowercase().as_str(),
        "model_context_window_exceeded" | "context_length_exceeded"
    )
}

/// A completion the provider cut off while the model was still writing a tool
/// call, either at its output-token ceiling or because the context window
/// overflowed.
///
/// This is the one truncation that cannot be handled by just keeping what
/// arrived. Every provider's decoder has to turn a half-written arguments
/// string into *some* JSON value, so a cut-off call silently degrades to `{}`
/// or to a bare string, and dispatching that runs the tool with empty
/// arguments, which for a shell command or a file edit is not a smaller
/// version of what the model meant, it is a different action.
///
/// `Display` is written by hand rather than derived because the two cutoffs
/// need opposite advice: one asks for a smaller reply, the other for a smaller
/// history. A user who is told "the output limit" when their context window
/// filled up will retry until they give up.
#[derive(Debug)]
pub struct TruncatedToolCall {
    /// The provider's own finish reason, verbatim.
    pub reason: String,
    /// Name of the tool whose arguments were cut off.
    pub tool: String,
}

impl std::fmt::Display for TruncatedToolCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (what, remedy) = if is_context_overflow(&self.reason) {
            (
                "ran out of context window",
                "compact the conversation (`/compact`) or start a new session, then ask again",
            )
        } else {
            (
                "hit the output-token limit",
                "ask for the work in smaller steps, or raise the model's output limit",
            )
        };
        write!(
            f,
            "the model's reply {what} ({}) while it was still writing the arguments for `{}`; \
             the truncated call was discarded rather than run with whatever arguments survived \
             {remedy}",
            self.reason, self.tool
        )
    }
}

impl std::error::Error for TruncatedToolCall {}

/// Typed error returned by every HTTP-based provider adapter for failed
/// responses and transport failures. Always reachable from the `anyhow`
/// chain via `err.downcast_ref::<ProviderError>()`, so the agent loop can
/// classify retries uniformly across providers.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ProviderError {
    /// HTTP status of the failed response; `None` for transport failures
    /// (connect/timeout/mid-stream drop) where no status was received.
    pub status: Option<u16>,
    /// Human-readable error, including a snippet of the API's response body
    /// so users see the real API message.
    pub message: String,
}

impl ProviderError {
    /// Error for a non-success HTTP response `status`.
    pub fn http(status: u16, message: impl Into<String>) -> Self {
        Self {
            status: Some(status),
            message: message.into(),
        }
    }

    /// Error for a transport failure (no HTTP status was received).
    pub fn transport(message: impl Into<String>) -> Self {
        Self {
            status: None,
            message: message.into(),
        }
    }

    /// Whether a retry after backoff may succeed: transport failures
    /// (`status == None`), timeouts (408), rate limits (429), and server
    /// errors (5xx) are transient; other 4xx (bad request, auth, missing
    /// model) are not.
    ///
    /// `status == None` is the load-bearing arm and the reason every adapter
    /// funnels its transport failures through [`ProviderError::transport`]: a
    /// connection reset, a refused connect, a failed TLS handshake, a DNS
    /// miss, a read timeout and a stream that ended mid-response all arrive
    /// without a status, and every one of them is a retry away from working.
    /// An untyped error would be classified by
    /// [`crate::agent::error_is_transient`]'s permissive fallback and get the
    /// same answer, but only by accident; the type is what makes it a
    /// decision.
    pub fn is_transient(&self) -> bool {
        match self.status {
            None => true,
            Some(408) | Some(429) => true,
            // 5xx as a class, which is what carries 529 ("overloaded",
            // Anthropic's own spelling and not a registered code) without a
            // table of every vendor's extension.
            Some(status) => status >= 500,
        }
    }

    /// Whether this failure is the *conversation* no longer fitting the
    /// model's context window, rather than any other bad request.
    ///
    /// Every provider in the tree reports it as a plain HTTP 400, which
    /// [`ProviderError::is_transient`] correctly calls permanent: re-sending
    /// the identical oversized prompt cannot work. But it is the one permanent
    /// failure with a *mechanical* remedy — compact the history and send
    /// again — so a caller that can drive compaction needs to tell it apart
    /// from a 400 that means the request was malformed, and the only place the
    /// distinction survives is the body text.
    ///
    /// Matched on a lowercased substring of the message, which already carries
    /// the API's own body (see [`ProviderError::http`]'s callers). The
    /// spellings are the ones the four wire shapes in this tree actually
    /// return; anything unrecognized reads as `false`, so a caller that acts
    /// on this fails towards reporting the error rather than towards
    /// compacting a history that was never the problem.
    ///
    /// Nothing in `src/llm` consumes this: it is the typed half of a fix whose
    /// other half (compact, then retry the turn) lives in the agent loop.
    pub fn is_context_overflow(&self) -> bool {
        if self.status != Some(400) && self.status != Some(413) {
            return false;
        }
        let message = self.message.to_ascii_lowercase();
        [
            // OpenAI and the compatible endpoints, including the error `code`
            // that rides in the JSON body verbatim.
            "context_length_exceeded",
            "maximum context length",
            // Anthropic: "prompt is too long: 213000 tokens > 200000 maximum".
            "prompt is too long",
            // xAI / Grok, and the Responses API's own spelling.
            "model_context_window_exceeded",
            "context window",
            // llama.cpp and Ollama in front of a fixed `num_ctx`.
            "exceeds the available context",
            "exceed context window",
        ]
        .iter()
        .any(|needle| message.contains(needle))
    }
}

/// The typed failure for a response stream that stopped before the provider
/// said it was finished.
///
/// Every streaming decoder in this module synthesizes its own `done: true`
/// chunk, so it has to decide what an unterminated stream means. Treating it
/// as a normal ending is the worst available answer: the agent receives a
/// perfectly well-formed completion that happens to be short, or empty, and
/// ends the turn — which is what "it randomly stops" looks like from the
/// outside. A dropped connection, a proxy that timed the upstream out
/// mid-generation and a server that was restarted under the request all land
/// here, and all three are worth another attempt, so it carries no status and
/// is therefore transient.
///
/// `what` names the transport for the message the user sees ("the xAI
/// response stream ...").
pub(crate) fn stream_ended_early(what: &str) -> anyhow::Error {
    anyhow::Error::new(ProviderError::transport(format!(
        "{what} ended before the model finished (no completion marker); \
         the connection was cut mid-response"
    )))
}

/// Message role on the Ollama `/api/chat` wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    /// Tool result fed back to the model.
    Tool,
}

/// Largest decoded image Wizard carries. Anything bigger is dropped at the
/// seam it arrives on ([`Image::from_bytes`], [`Image::from_path`], the
/// providers' stream decoders, and [`crate::agent::absorb_images`] for
/// anything hand-built) rather than pushed through history, the session file
/// and every surface.
pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

/// Why an image could not be taken in.
#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("unrecognized image data (not PNG, JPEG, WebP or GIF)")]
    UnknownFormat,
    #[error("image data is not valid base64: {0}")]
    NotBase64(#[from] base64::DecodeError),
    #[error("image is {bytes} bytes, over the {MAX_IMAGE_BYTES} byte cap")]
    TooLarge { bytes: usize },
    #[error("cannot read image {path}: {source}")]
    Unreadable {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// An image travelling through Wizard, in either direction: attached to a
/// [`ChatMessage`] on the way *to* a vision model, or produced *by* a tool
/// ([`crate::tools::ToolOutput::images`]) or by the model itself
/// ([`ChatChunk::images`]).
///
/// `b64` is the base64 of the encoded file bytes with **no** `data:` prefix
/// (providers that want a data URI build one with [`Image::data_uri`]); `mime`
/// is its media type, e.g. `image/png`.
///
/// This diverges from `feat/computer-use`, where images are a bare
/// `Vec<String>` of base64 PNGs: a *generated* image is not always a PNG, so
/// the media type has to ride with the bytes. Reconciling the two branches
/// when that one merges is mechanical — its `Vec<String>` becomes
/// `Image::new(b64, "image/png")` at each construction site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    /// Base64 of the encoded image file (no `data:` prefix).
    pub b64: String,
    /// Media type of the encoded bytes, e.g. `image/png`.
    pub mime: String,
    /// Where the image was written, once the session's image store took it in
    /// ([`crate::agent::absorb_images`]). `None` before then — a tool that has
    /// just produced an image does not know, and does not need to.
    ///
    /// It is recorded in the session file alongside the base64 purely for
    /// *replay*: a surface rebuilding a transcript from disk (the GUI's, the
    /// TUI's on `--resume`) gets the same path the live
    /// [`AgentEvent::Images`](crate::agent::AgentEvent::Images) carried,
    /// instead of re-deriving it. No provider ever sees this field — every
    /// provider translates `images` into its own shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<std::path::PathBuf>,
}

impl Image {
    /// An image whose base64 and media type are already known (a provider
    /// decoding its own wire format, a tool that knows what it encoded).
    pub fn new(b64: impl Into<String>, mime: impl Into<String>) -> Self {
        Self {
            b64: b64.into(),
            mime: mime.into(),
            path: None,
        }
    }

    /// This image, tagged with where the image store wrote it.
    pub fn at_path(mut self, path: std::path::PathBuf) -> Self {
        self.path = Some(path);
        self
    }

    /// Take in raw encoded image bytes: the media type is sniffed from the
    /// magic number and the bytes are base64-encoded. The natural constructor
    /// for a tool that has just produced or read an image file.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ImageError> {
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(ImageError::TooLarge { bytes: bytes.len() });
        }
        let mime = sniff_mime(bytes).ok_or(ImageError::UnknownFormat)?;
        use base64::Engine as _;
        Ok(Self::new(
            base64::engine::general_purpose::STANDARD.encode(bytes),
            mime,
        ))
    }

    /// Take in an image file from disk: a user attaching a screenshot, a
    /// pasted file path. The size cap is enforced against the file's metadata
    /// *before* any bytes are read, so an oversized file is refused without
    /// being pulled into memory, and the media type is sniffed from the bytes
    /// rather than guessed from the extension — a `.png` that is really a
    /// JPEG is tagged as what it is.
    ///
    /// The returned image is tagged with the path it came from, so a surface
    /// replaying the transcript can render the file it already has on disk.
    pub fn from_path(path: &std::path::Path) -> Result<Self, ImageError> {
        let unreadable = |source: std::io::Error| ImageError::Unreadable {
            path: path.display().to_string(),
            source,
        };
        let meta = std::fs::metadata(path).map_err(unreadable)?;
        if !meta.is_file() {
            return Err(unreadable(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "not a file",
            )));
        }
        if meta.len() > MAX_IMAGE_BYTES as u64 {
            return Err(ImageError::TooLarge {
                bytes: usize::try_from(meta.len()).unwrap_or(usize::MAX),
            });
        }
        let bytes = std::fs::read(path).map_err(unreadable)?;
        Ok(Self::from_bytes(&bytes)?.at_path(path.to_path_buf()))
    }

    /// Take in a `data:` URI (`data:image/png;base64,iVBOR...`), the shape
    /// OpenAI-compatible endpoints use for image content parts. `None` when
    /// the string is not a base64 `data:` URI of an image.
    pub fn from_data_uri(uri: &str) -> Result<Self, ImageError> {
        let rest = uri.strip_prefix("data:").ok_or(ImageError::UnknownFormat)?;
        let (mime, payload) = rest.split_once(',').ok_or(ImageError::UnknownFormat)?;
        let mime = mime
            .strip_suffix(";base64")
            .ok_or(ImageError::UnknownFormat)?;
        if !mime.starts_with("image/") {
            return Err(ImageError::UnknownFormat);
        }
        Self::from_base64(payload, mime)
    }

    /// Take in base64 that arrived with its media type stated separately
    /// (`b64_json` payloads). Validates the base64 and the size cap.
    pub fn from_base64(b64: &str, mime: &str) -> Result<Self, ImageError> {
        let image = Self::new(b64.trim(), mime);
        let bytes = image.decoded_len();
        if bytes > MAX_IMAGE_BYTES {
            return Err(ImageError::TooLarge { bytes });
        }
        // Decode once, up front: a provider must never hand a broken payload
        // to a surface that will try to write it to disk.
        image.decode()?;
        Ok(image)
    }

    /// The encoded file bytes.
    pub fn decode(&self) -> Result<Vec<u8>, base64::DecodeError> {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.decode(self.b64.trim())
    }

    /// Size of the decoded image, derived from the base64 length — no decode,
    /// so the size cap can be checked without allocating the payload.
    pub fn decoded_len(&self) -> usize {
        let b64 = self.b64.trim();
        let padding = b64.bytes().rev().take_while(|&byte| byte == b'=').count();
        b64.len().saturating_sub(padding) * 3 / 4
    }

    /// `data:<mime>;base64,<b64>` — how OpenAI-compatible endpoints (and the
    /// GUI's `<img src>`) want an inline image.
    pub fn data_uri(&self) -> String {
        format!("data:{};base64,{}", self.mime, self.b64)
    }

    /// File extension for this media type, for naming the image on disk.
    pub fn extension(&self) -> &'static str {
        match self.mime.as_str() {
            "image/jpeg" => "jpg",
            "image/webp" => "webp",
            "image/gif" => "gif",
            // PNG is both the common case and the safe default: an image whose
            // type we could not name is still written, just conservatively.
            _ => "png",
        }
    }
}

/// Media type of `bytes` from its magic number. Covers the formats every
/// vision model and image endpoint in use speaks; `None` for anything else,
/// which is refused rather than guessed at.
pub fn sniff_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && bytes[8..12] == *b"WEBP" {
        return Some("image/webp");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    None
}

/// One block of a [`ChatMessage`]'s content.
///
/// History is a *list of blocks* rather than a string because both frontier
/// APIs are: a single assistant turn interleaves text, reasoning and any
/// number of `tool_use` blocks in an order the model chose, and flattening
/// that into a string plus a sidecar `Vec<ToolCall>` is exactly what made a
/// parallel tool-call batch unrepresentable.
///
/// Every variant wraps a payload *struct* instead of spelling its fields
/// inline. That is deliberate: a later field on one kind of block (a
/// `cache_control` breakpoint, a reasoning signature) is then one struct's
/// change rather than an edit to every `match` arm in the tree. A whole new
/// *variant* still has to break every match, which is what we want, so this
/// enum is deliberately not `#[non_exhaustive]` and matches over it must stay
/// exhaustive: no wildcard arm.
///
/// The serialized shape is the internally tagged `{"type": "...", ...}`
/// object both Anthropic and OpenAI already speak, which keeps session files
/// legible and each adapter's translation close to an identity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain text: what the user typed, what the model wrote.
    Text(TextBlock),
    /// An image travelling in either direction: input for a vision model, or
    /// one the model produced.
    Image(Image),
    /// A tool invocation the model requested (assistant messages only).
    ToolUse(ToolCall),
    /// The answer to exactly one [`ContentBlock::ToolUse`], bound to it by id
    /// (`Role::Tool` messages only).
    ToolResult(ToolResultBlock),
    /// Model reasoning ("thinking"), kept in the shape the provider needs to
    /// see it replayed in.
    Thinking(ThinkingBlock),
}

/// A [`ContentBlock::Text`] payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextBlock {
    pub text: String,
}

/// A [`ContentBlock::ToolResult`] payload: one tool's answer, bound to the
/// call it answers by id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultBlock {
    /// Id of the [`ToolCall`] this answers, verbatim as the provider issued
    /// it. This is the *only* correlation there is: nothing keys off the tool
    /// name or off dispatch order any more.
    pub tool_use_id: String,
    /// Name of the tool that produced the result. No provider's `tool_result`
    /// shape carries one, so it never reaches the wire; it is kept because
    /// every Wizard surface labels the result card with it and because a
    /// session file has to stay readable on its own.
    pub name: String,
    /// The result text handed back to the model.
    pub content: String,
}

/// A [`ContentBlock::Thinking`] payload.
///
/// Reasoning is not decoration on the frontier APIs: Anthropic accepts a
/// replayed thinking block only when its `signature` comes back untouched,
/// and both vendors have a redacted form whose payload is an opaque blob
/// rather than text. Both are representable here, so reasoning passthrough is
/// a matter of filling these fields in rather than of changing the type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThinkingBlock {
    /// The reasoning as text. Empty for a redacted block, whose payload lives
    /// in `data`.
    #[serde(default)]
    pub thinking: String,
    /// Provider signature over the block (Anthropic's `thinking.signature`),
    /// which has to be echoed back byte for byte for the block to be accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Opaque encrypted reasoning (Anthropic's `redacted_thinking.data`,
    /// OpenAI's `reasoning.encrypted_content`), carried verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
}

/// Flat token allowance for one image. Vision costs are model-specific and
/// this only feeds the status-bar meter, so a fixed number is enough.
const IMAGE_TOKEN_ALLOWANCE: u64 = 1_000;

impl ContentBlock {
    /// A text block.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(TextBlock { text: text.into() })
    }

    /// A tool-result block answering the call with id `tool_use_id`.
    pub fn tool_result(
        tool_use_id: impl Into<String>,
        name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self::ToolResult(ToolResultBlock {
            tool_use_id: tool_use_id.into(),
            name: name.into(),
            content: content.into(),
        })
    }

    /// A thinking block carrying reasoning text and, when the provider sent
    /// one, the signature that lets it be replayed.
    pub fn thinking(thinking: impl Into<String>, signature: Option<String>) -> Self {
        Self::Thinking(ThinkingBlock {
            thinking: thinking.into(),
            signature,
            data: None,
        })
    }

    /// The text this block contributes to [`ChatMessage::text`]: its own for a
    /// text block, the result body for a tool result (which is what every
    /// surface renders as a `tool`-role message's content). `None` for
    /// everything else, reasoning included, because it is not the answer.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(block) => Some(block.text.as_str()),
            Self::ToolResult(block) => Some(block.content.as_str()),
            Self::Image(_) | Self::ToolUse(_) | Self::Thinking(_) => None,
        }
    }

    /// Rough token cost of this block, for [`ChatMessage::estimated_tokens`].
    fn estimated_tokens(&self) -> u64 {
        match self {
            Self::Text(block) => estimate_tokens_from_chars(block.text.len()),
            Self::Image(_) => IMAGE_TOKEN_ALLOWANCE,
            // `arguments` is already a JSON value; its string form is roughly
            // what the wire payload costs.
            Self::ToolUse(call) => estimate_tokens_from_chars(
                call.function
                    .name
                    .len()
                    .saturating_add(call.function.arguments.to_string().len()),
            ),
            Self::ToolResult(block) => {
                estimate_tokens_from_chars(block.name.len().saturating_add(block.content.len()))
            }
            Self::Thinking(block) => estimate_tokens_from_chars(
                block
                    .thinking
                    .len()
                    .saturating_add(block.data.as_ref().map_or(0, String::len)),
            ),
        }
    }
}

/// A single chat message. Session files and in-memory history use this shape;
/// provider adapters translate it to each backend's wire format.
///
/// `content` is a [`ContentBlock`] list, not a string. The string
/// constructors ([`ChatMessage::user`] and friends) wrap their argument in a
/// single text block, so the overwhelmingly common text-only message is still
/// one call, and [`ChatMessage::text`] reads it back.
#[derive(Debug, Clone, Serialize)]
pub struct ChatMessage {
    pub role: Role,
    /// Everything this message says, in the order it was said.
    pub content: Vec<ContentBlock>,
}

impl ChatMessage {
    /// Rough token estimate for this message (`~4` chars per token, plus a
    /// flat allowance for each attached image). Used for the TUI context
    /// meter when the backend has not yet reported a real prompt size
    /// (fresh session, post-`/clear`, post-compaction).
    pub fn estimated_tokens(&self) -> u64 {
        self.content
            .iter()
            .map(ContentBlock::estimated_tokens)
            .fold(0u64, u64::saturating_add)
    }

    /// A message with `role` carrying exactly `content`. The general
    /// constructor: use it when the blocks are not just a line of text (a
    /// provider decoding a tool-call-only reply, a caller interleaving
    /// reasoning with text).
    pub fn new(role: Role, content: Vec<ContentBlock>) -> Self {
        Self { role, content }
    }

    /// A message carrying exactly one text block. Every string constructor
    /// funnels through here.
    fn text_message(role: Role, content: impl Into<String>) -> Self {
        Self::new(role, vec![ContentBlock::text(content)])
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self::text_message(Role::System, content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::text_message(Role::User, content)
    }

    /// User message carrying images alongside its text. This is how a tool's
    /// images reach the model: a `tool`-role message cannot carry image blocks
    /// on OpenAI, but a user message can on every provider, so the tool result
    /// carries the text and the images follow on a user message (see
    /// `Agent::dispatch_call`). A non-vision model simply ignores them.
    pub fn user_with_images(content: impl Into<String>, images: Vec<Image>) -> Self {
        let mut message = Self::user(content);
        message
            .content
            .extend(images.into_iter().map(ContentBlock::Image));
        message
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::text_message(Role::Assistant, content)
    }

    /// An assistant turn as an agent loop assembles it: the reply text, then
    /// the images the model produced, then the tool calls it made.
    ///
    /// The text block is dropped when the reply said nothing *and* carried
    /// something else, so a tool-call-only turn does not gain a stray empty
    /// block that a provider would have to be taught to ignore.
    pub fn assistant_turn(
        text: impl Into<String>,
        images: Vec<Image>,
        tool_calls: Vec<ToolCall>,
    ) -> Self {
        let text = text.into();
        let mut content = Vec::with_capacity(1 + images.len() + tool_calls.len());
        if !text.is_empty() || (images.is_empty() && tool_calls.is_empty()) {
            content.push(ContentBlock::text(text));
        }
        content.extend(images.into_iter().map(ContentBlock::Image));
        content.extend(tool_calls.into_iter().map(ContentBlock::ToolUse));
        Self::new(Role::Assistant, content)
    }

    /// Tool result message answering the [`ToolCall`] with id `tool_use_id`.
    /// A batch of parallel calls is answered by *one* message holding one
    /// block per call ([`ChatMessage::push_tool_result`]), which is what
    /// Anthropic requires and what every other API accepts.
    pub fn tool_result(
        tool_use_id: impl Into<String>,
        tool_name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            role: Role::Tool,
            content: vec![ContentBlock::tool_result(tool_use_id, tool_name, content)],
        }
    }

    /// The message's text: every text block and tool-result body joined in
    /// order. The read-side counterpart of the string constructors, and what
    /// a surface renders when it wants "what this message said".
    ///
    /// Reasoning is deliberately not part of it: a thinking block is how the
    /// model got to the answer, not the answer.
    pub fn text(&self) -> String {
        let mut text = String::new();
        for block in self.content.iter().filter_map(ContentBlock::as_text) {
            text.push_str(block);
        }
        text
    }

    /// Tool calls this message requests, in order. Empty for every role but
    /// assistant.
    pub fn tool_calls(&self) -> Vec<&ToolCall> {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse(call) => Some(call),
                ContentBlock::Text(_)
                | ContentBlock::Image(_)
                | ContentBlock::ToolResult(_)
                | ContentBlock::Thinking(_) => None,
            })
            .collect()
    }

    /// Append a tool call to this message's content.
    pub fn push_tool_call(&mut self, call: ToolCall) {
        self.content.push(ContentBlock::ToolUse(call));
    }

    /// Tool results this message carries, in order. A parallel batch's
    /// results all live on one message, so this is how many calls it answers.
    pub fn tool_results(&self) -> Vec<&ToolResultBlock> {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolResult(result) => Some(result),
                ContentBlock::Text(_)
                | ContentBlock::Image(_)
                | ContentBlock::ToolUse(_)
                | ContentBlock::Thinking(_) => None,
            })
            .collect()
    }

    /// Append another tool result to this message, so one `tool`-role message
    /// answers a whole parallel batch.
    pub fn push_tool_result(
        &mut self,
        tool_use_id: impl Into<String>,
        tool_name: impl Into<String>,
        content: impl Into<String>,
    ) {
        self.content
            .push(ContentBlock::tool_result(tool_use_id, tool_name, content));
    }

    /// Name of the tool that produced this message's *first* result, when it
    /// has one. Surfaces that label a single result row use it; anything
    /// handling a batch wants [`ChatMessage::tool_results`].
    pub fn tool_name(&self) -> Option<&str> {
        self.tool_results()
            .first()
            .map(|result| result.name.as_str())
    }

    /// Images attached to this message, in order. On a user message they are
    /// input for a vision model (a screenshot, or an image a tool just
    /// returned); on an assistant message they are what the model itself
    /// produced. Every provider translates them into its own shape
    /// (Ollama's sibling base64 array, OpenAI's `image_url` parts,
    /// Anthropic's base64 `image` blocks).
    pub fn images(&self) -> Vec<&Image> {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Image(image) => Some(image),
                ContentBlock::Text(_)
                | ContentBlock::ToolUse(_)
                | ContentBlock::ToolResult(_)
                | ContentBlock::Thinking(_) => None,
            })
            .collect()
    }

    /// Attach an image to this message.
    pub fn push_image(&mut self, image: Image) {
        self.content.push(ContentBlock::Image(image));
    }

    /// Take this message's images by value, leaving the rest of its content
    /// in place. How the agent loop accumulates a streamed reply's images.
    pub fn take_images(&mut self) -> Vec<Image> {
        let mut images = Vec::new();
        for block in std::mem::take(&mut self.content) {
            match block {
                ContentBlock::Image(image) => images.push(image),
                kept @ (ContentBlock::Text(_)
                | ContentBlock::ToolUse(_)
                | ContentBlock::ToolResult(_)
                | ContentBlock::Thinking(_)) => self.content.push(kept),
            }
        }
        images
    }

    /// Take this message's tool calls by value, leaving the rest of its
    /// content in place.
    pub fn take_tool_calls(&mut self) -> Vec<ToolCall> {
        let mut calls = Vec::new();
        for block in std::mem::take(&mut self.content) {
            match block {
                ContentBlock::ToolUse(call) => calls.push(call),
                kept @ (ContentBlock::Text(_)
                | ContentBlock::Image(_)
                | ContentBlock::ToolResult(_)
                | ContentBlock::Thinking(_)) => self.content.push(kept),
            }
        }
        calls
    }
}

/// `content` as it appears on the wire, in **either** format a session file
/// can hold: the block list Wizard writes now, or the bare string it wrote
/// before content blocks existed.
#[derive(Deserialize)]
#[serde(untagged)]
enum WireContent {
    Blocks(Vec<ContentBlock>),
    Text(String),
}

/// Deserialization shape for [`ChatMessage`], covering both formats.
///
/// Wizard wrote `content` as a bare string with `tool_calls`, `tool_name` and
/// `images` as sibling fields until v2. Those files are on users' disks and
/// must keep loading, so the legacy fields are read here and folded into the
/// block list in the order a v2 message would have carried them. Nothing
/// writes this shape any more.
///
/// A legacy tool call has no id (none was ever recorded) and neither has the
/// result answering it; both come out with an empty one, and
/// [`crate::agent::session::assign_legacy_tool_call_ids`] pairs them up once,
/// at load, where the whole message sequence is in view.
#[derive(Deserialize)]
struct ChatMessageWire {
    role: Role,
    /// Absent or `null` is an empty message rather than an error: some
    /// OpenAI-compatible servers send `"content": null` on a tool-call-only
    /// delta.
    #[serde(default)]
    content: Option<WireContent>,
    #[serde(default)]
    tool_calls: Vec<ToolCall>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    images: Vec<Image>,
}

impl<'de> Deserialize<'de> for ChatMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = ChatMessageWire::deserialize(deserializer)?;
        let text = match wire.content {
            Some(WireContent::Blocks(blocks)) => {
                // Already v2: the sibling fields cannot be present, so the
                // block list is the whole message.
                return Ok(ChatMessage {
                    role: wire.role,
                    content: blocks,
                });
            }
            Some(WireContent::Text(text)) => Some(text),
            // No `content` key at all (or an explicit `null`): the message
            // said nothing, so it gets no text block rather than an empty
            // one. Any sibling fields still fold in below.
            None => None,
        };
        let mut content = Vec::new();
        match (wire.role, wire.tool_name) {
            // A pre-v2 `tool` record: its text *is* the result body.
            (Role::Tool, name) => content.push(ContentBlock::tool_result(
                String::new(),
                name.unwrap_or_default(),
                text.unwrap_or_default(),
            )),
            // Every other role keeps an empty text block only when it has
            // nothing else to say, so an empty assistant turn that only made
            // tool calls does not gain a stray block.
            _ => {
                if let Some(text) = text
                    && (!text.is_empty() || (wire.images.is_empty() && wire.tool_calls.is_empty()))
                {
                    content.push(ContentBlock::text(text));
                }
            }
        }
        content.extend(wire.images.into_iter().map(ContentBlock::Image));
        content.extend(wire.tool_calls.into_iter().map(ContentBlock::ToolUse));
        Ok(ChatMessage {
            role: wire.role,
            content,
        })
    }
}

/// The text of an assistant message as it goes back on the wire, with any
/// images it produced named in it.
///
/// No chat API accepts image content *inside* an assistant turn — images are
/// user-role input everywhere — so a model's own generated images cannot be
/// replayed as they were produced. They are dropped from the request and named
/// in the text instead: the model still knows what it made (and the user still
/// sees the file, which the surfaces render from
/// [`AgentEvent::Images`](crate::agent::AgentEvent::Images)), and the request
/// stays valid rather than 400-ing on a block the API will not take.
pub(crate) fn assistant_content(message: &ChatMessage) -> String {
    let images = message.images();
    let text = message.text();
    if images.is_empty() {
        return text;
    }
    let kinds: Vec<&str> = images.iter().map(|image| image.mime.as_str()).collect();
    let note = format!(
        "[generated {} image(s) ({}) - delivered to the user]",
        images.len(),
        kinds.join(", ")
    );
    if text.is_empty() {
        note
    } else {
        format!("{text}\n\n{note}")
    }
}

/// Rough token estimate from a character count (`~4` chars per token). Used
/// only when a backend has not reported real usage; never for billing.
pub fn estimate_tokens_from_chars(chars: usize) -> u64 {
    (chars as u64).div_ceil(4)
}

/// Sum of [`ChatMessage::estimated_tokens`] over a history. The status bar
/// falls back to this after `/clear` or compaction, when the last real
/// prompt size is stale or unknown.
pub fn estimate_history_tokens(messages: &[ChatMessage]) -> u64 {
    messages.iter().map(ChatMessage::estimated_tokens).sum()
}

/// A tool invocation requested by the model.
/// Wire shape: `{ "id": ..., "function": { "name": ..., "arguments": {...} } }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// The provider's own id for this call (`toolu_…` on Anthropic, `call_…`
    /// on OpenAI), carried verbatim. It is the sole link between a call and
    /// the [`ContentBlock::ToolResult`] that answers it: without it, results
    /// could only be matched by tool name plus dispatch order, which is wrong
    /// the moment a model emits two calls to the same tool in one turn, and
    /// both Claude and GPT do that by default.
    ///
    /// Empty only in the window between decoding a provider that sent no id
    /// (Ollama's native endpoint) and [`ensure_tool_call_ids`] filling one
    /// in, and on calls read out of a pre-v2 session file.
    #[serde(default)]
    pub id: String,
    pub function: FunctionCall,
}

impl ToolCall {
    /// A call with a synthetic id, for the paths that invent one: a provider
    /// that sent no id, and the JSON-in-text fallback used when a model has
    /// no native tool calling.
    pub fn new(name: impl Into<String>, arguments: serde_json::Value) -> Self {
        Self {
            id: synthetic_tool_call_id(),
            function: FunctionCall {
                name: name.into(),
                arguments,
            },
        }
    }
}

/// Give every call in `calls` that arrived without an id one of ours.
///
/// Called by each adapter on the batch it decoded, so the invariant "a
/// [`ToolCall`] reaching the agent has an id" holds from the seam inward and
/// nothing downstream has to re-check it. Ollama's native endpoint is the
/// provider that needs it (its `tool_calls` have no ids at all); the others
/// use it only as a fallback for a server that omitted one.
pub fn ensure_tool_call_ids(calls: &mut [ToolCall]) {
    for call in calls.iter_mut().filter(|call| call.id.is_empty()) {
        call.id = synthetic_tool_call_id();
    }
}

/// An id for a tool call whose provider did not give it one.
///
/// The counter is seeded from the wall clock rather than from zero because
/// ids have to stay unique across *runs*: a resumed session brings the
/// previous process's synthetic ids back from disk, and a fresh counter would
/// hand the same `wz_1` to a different call in the same conversation.
pub fn synthetic_tool_call_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let seed = *SEED.get_or_init(|| {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |since| since.as_millis() as u64)
    });
    format!("wz_{seed:x}_{:x}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// The function half of a [`ToolCall`]. `arguments` is a JSON object
/// (already parsed — Ollama's native endpoint sends structured arguments,
/// not a string).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
}

/// A tool advertised to the model in the request's `tools` array.
/// Wire shape: `{ "type": "function", "function": { name, description, parameters } }`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionSpec,
}

impl ToolSpec {
    /// Build a `"function"`-typed spec. `parameters` must be a JSON Schema
    /// object describing the arguments.
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            kind: "function".to_string(),
            function: FunctionSpec {
                name: name.into(),
                description: description.into(),
                parameters,
            },
        }
    }
}

/// The function half of a [`ToolSpec`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's arguments.
    pub parameters: serde_json::Value,
}

/// Model sampling options forwarded as Ollama's `options` object.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_ctx: Option<u32>,
    /// Reasoning effort (`"low"`/`"medium"`/`"high"`) for models that accept a
    /// `reasoning_effort` request field. Carried as a string so this module
    /// stays decoupled from [`crate::config::ReasoningEffort`]; the
    /// OpenAI-compatible client forwards it only for supporting models.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

/// Request body for `POST /api/chat`.
#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSpec>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<ChatOptions>,
}

/// One streamed JSON line from `POST /api/chat` (`stream: true`).
/// Text arrives as deltas in `message.content`; tool calls arrive complete
/// in `message.tool_calls`; the final chunk has `done == true`.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatChunk {
    #[serde(default)]
    pub message: Option<ChatMessage>,
    /// Images the model produced in this chunk.
    ///
    /// **This is the seam for an image-generating endpoint.** A provider that
    /// receives image content while streaming (an `image_url` part, a
    /// `b64_json` payload — see [`wire::decode_sse`] for the working
    /// example) decodes it into an [`Image`] and emits it here, on the chunk
    /// it arrived in; the chunk may carry images, text, tool calls, or any
    /// combination. The agent loop accumulates them onto the assistant
    /// [`ChatMessage`], writes them to the session's image directory, and
    /// announces them to the surfaces as [`crate::agent::AgentEvent::Images`].
    /// Nothing else is required of the provider.
    #[serde(default)]
    pub images: Vec<Image>,
    /// True when `message.content` is model reasoning ("thinking") rather
    /// than answer text (Anthropic `thinking_delta`, xAI `reasoning_content`).
    /// The UI renders it dimmed; it is never fed back into history.
    #[serde(default)]
    pub thinking: bool,
    pub done: bool,
    #[serde(default)]
    pub done_reason: Option<String>,
    /// Output token count (final chunk only).
    #[serde(default)]
    pub eval_count: Option<u64>,
    /// Prompt token count (final chunk only).
    #[serde(default)]
    pub prompt_eval_count: Option<u64>,
    /// How `prompt_eval_count` splits between the provider's prompt cache and
    /// fresh input (final chunk only). See [`CacheTokens`].
    #[serde(default)]
    pub cache: CacheTokens,
}

/// The prompt-cache split of one model call's prompt tokens.
///
/// Both counts are **subsets** of [`ChatChunk::prompt_eval_count`], never
/// additions to it. Providers disagree about whether their own wire field is:
/// OpenAI's `prompt_tokens` already contains
/// `prompt_tokens_details.cached_tokens`, while Anthropic's `input_tokens`
/// excludes both `cache_read_input_tokens` and `cache_creation_input_tokens`.
/// An adapter reconciles that on the way in — it has to anyway, because the
/// context meter needs the real prompt size — and what reaches here is always
/// the subset form. [`crate::usage::TurnTokens`] states the same contract for
/// the counters these feed.
///
/// Zero means "no cache activity reported", which is the same thing as no
/// cache activity for every consumer: the counters ignore it and
/// [`crate::usage::estimate_cost`] prices the whole prompt fresh, which is
/// what a provider with no prompt cache should be billed.
///
/// **Why this is on the chunk rather than in a log line.** A cache read costs
/// a tenth of a fresh input token on Anthropic, so a turn that hits the cache
/// and reports no split is over-billed by up to 10x on the cached portion.
/// The price table and the rollup were both correct before this field
/// existed; the number simply had no channel to travel on, and both adapters
/// decoded it and dropped it into `tracing::debug!`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct CacheTokens {
    /// Prompt tokens the provider served from its cache (Anthropic
    /// `cache_read_input_tokens`, OpenAI and Responses
    /// `..._tokens_details.cached_tokens`).
    #[serde(default)]
    pub read: u64,
    /// Prompt tokens the provider wrote into its cache (Anthropic
    /// `cache_creation_input_tokens`). Providers that do not bill a separate
    /// cache write report 0.
    #[serde(default)]
    pub write: u64,
}

impl CacheTokens {
    /// Nothing was cached, or the backend does not report it.
    pub const NONE: Self = Self { read: 0, write: 0 };

    /// Whether the provider reported any cache activity at all.
    pub fn is_empty(self) -> bool {
        self.read == 0 && self.write == 0
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn chat_request_serializes_with_block_content() {
        let request = ChatRequest {
            model: "qwen3.6:27b".to_string(),
            messages: vec![
                ChatMessage::system("You are Wizard."),
                ChatMessage::user("hi"),
            ],
            tools: vec![ToolSpec::function(
                "read_file",
                "Read a file.",
                json!({ "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] }),
            )],
            stream: true,
            options: Some(ChatOptions {
                temperature: Some(0.8),
                num_ctx: None,
                reasoning_effort: None,
            }),
        };

        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["model"], "qwen3.6:27b");
        assert_eq!(value["stream"], true);
        assert_eq!(value["messages"][0]["role"], "system");
        assert_eq!(value["messages"][1]["role"], "user");
        // Content is a block array on the wire Wizard writes; the one backend
        // that wants a flat string (Ollama's native `/api/chat`) rebuilds the
        // messages itself, see `ollama::build_request_body`.
        assert_eq!(value["messages"][1]["content"][0]["type"], "text");
        assert_eq!(value["messages"][1]["content"][0]["text"], "hi");
        assert_eq!(value["tools"][0]["type"], "function");
        assert_eq!(value["tools"][0]["function"]["name"], "read_file");
        assert_eq!(
            value["tools"][0]["function"]["parameters"]["required"][0],
            "path"
        );
        let temperature = value["options"]["temperature"]
            .as_f64()
            .expect("temperature is a number");
        assert!(
            (temperature - 0.8).abs() < 1e-6,
            "temperature survives the f32 round-trip: {temperature}"
        );
        assert!(
            value["options"].get("num_ctx").is_none(),
            "unset options are omitted"
        );
    }

    #[test]
    fn empty_tools_and_options_are_omitted_from_the_wire() {
        let request = ChatRequest {
            model: "m".to_string(),
            messages: vec![ChatMessage::user("hi")],
            tools: Vec::new(),
            stream: false,
            options: None,
        };
        let value = serde_json::to_value(&request).unwrap();
        assert!(value.get("tools").is_none(), "empty tools array is omitted");
        assert!(value.get("options").is_none(), "absent options are omitted");
    }

    #[test]
    fn plain_message_omits_tool_fields() {
        let value = serde_json::to_value(ChatMessage::assistant("done")).unwrap();
        assert!(value.get("tool_calls").is_none());
        assert!(value.get("tool_name").is_none());
        assert!(
            value.get("images").is_none(),
            "text-only traffic is unchanged on the wire"
        );
    }

    /// Smallest possible files of each format we sniff (header bytes are all
    /// that matters).
    fn png_bytes() -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.extend_from_slice(b"IHDR-and-the-rest");
        bytes
    }

    #[test]
    fn sniffs_the_media_type_from_magic_numbers() {
        assert_eq!(sniff_mime(&png_bytes()), Some("image/png"));
        assert_eq!(
            sniff_mime(&[0xff, 0xd8, 0xff, 0xe0, 0x00]),
            Some("image/jpeg")
        );
        assert_eq!(sniff_mime(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff_mime(b"GIF89a....."), Some("image/gif"));
        assert_eq!(sniff_mime(b"GIF87a....."), Some("image/gif"));
        // Not an image, and truncated headers that merely start right.
        assert_eq!(sniff_mime(b"not an image at all"), None);
        assert_eq!(sniff_mime(b"RIFF\0\0\0\0AVI "), None, "RIFF but not WebP");
        assert_eq!(sniff_mime(&[0x89, b'P']), None);
        assert_eq!(sniff_mime(&[]), None);
    }

    #[test]
    fn from_bytes_sniffs_encodes_and_round_trips() {
        let image = Image::from_bytes(&png_bytes()).expect("a PNG");
        assert_eq!(image.mime, "image/png");
        assert_eq!(image.extension(), "png");
        assert!(!image.b64.starts_with("data:"), "no data: prefix on b64");
        assert_eq!(image.decode().expect("decodes"), png_bytes());
        assert_eq!(image.decoded_len(), png_bytes().len());
        assert_eq!(
            image.data_uri(),
            format!("data:image/png;base64,{}", image.b64)
        );

        let err = Image::from_bytes(b"nonsense").expect_err("unknown format");
        assert!(matches!(err, ImageError::UnknownFormat), "{err}");
    }

    #[test]
    fn decoded_len_matches_the_real_payload_at_every_padding() {
        // 0, 1 and 2 padding chars — the three base64 alignments.
        for raw in [&b"abc"[..], &b"a"[..], &b"ab"[..]] {
            use base64::Engine as _;
            let b64 = base64::engine::general_purpose::STANDARD.encode(raw);
            let image = Image::new(b64, "image/png");
            assert_eq!(
                image.decoded_len(),
                raw.len(),
                "size is derived from the base64 without decoding"
            );
        }
    }

    #[test]
    fn from_data_uri_accepts_images_and_refuses_anything_else() {
        let image = Image::from_data_uri("data:image/webp;base64,UklGRg==").expect("a webp");
        assert_eq!(image.mime, "image/webp");
        assert_eq!(image.b64, "UklGRg==");
        assert_eq!(image.extension(), "webp");

        for bad in [
            "https://example.com/cat.png",
            "data:text/plain;base64,aGk=",
            "data:image/png,notbase64encoded",
            "data:image/png;base64,!!!not base64!!!",
        ] {
            assert!(Image::from_data_uri(bad).is_err(), "must refuse {bad}");
        }
    }

    #[test]
    fn oversized_images_are_refused_at_the_seam() {
        let huge = vec![0u8; MAX_IMAGE_BYTES + 1];
        let err = Image::from_bytes(&huge).expect_err("over the cap");
        assert!(
            matches!(err, ImageError::TooLarge { bytes } if bytes == MAX_IMAGE_BYTES + 1),
            "{err}"
        );

        // The base64 path caps too, without decoding the payload.
        let b64 = "A".repeat(MAX_IMAGE_BYTES / 3 * 4 + 8);
        let err = Image::from_base64(&b64, "image/png").expect_err("over the cap");
        assert!(matches!(err, ImageError::TooLarge { .. }), "{err}");
    }

    #[test]
    fn message_images_round_trip_through_the_session_format() {
        let message =
            ChatMessage::user_with_images("what is this?", vec![Image::new("QUJD", "image/jpeg")]);
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value["content"][0]["type"], "text");
        assert_eq!(value["content"][1]["type"], "image");
        assert_eq!(value["content"][1]["b64"], "QUJD");
        assert_eq!(value["content"][1]["mime"], "image/jpeg");

        let back: ChatMessage = serde_json::from_value(value).unwrap();
        assert_eq!(back.images(), message.images());
        assert_eq!(back.text(), "what is this?");
    }

    #[test]
    fn chat_chunk_without_images_deserializes_to_none() {
        // Every existing provider's chunks have no `images` field.
        let chunk: ChatChunk =
            serde_json::from_str(r#"{"message":{"role":"assistant","content":"hi"},"done":false}"#)
                .unwrap();
        assert!(chunk.images.is_empty());
    }

    #[test]
    fn from_path_sniffs_the_bytes_and_caps_the_file_size() {
        let dir = tempfile::tempdir().expect("tempdir");

        // The extension lies (a JPEG named .png): the bytes decide.
        let jpeg = dir.path().join("shot.png");
        std::fs::write(&jpeg, [0xff, 0xd8, 0xff, 0xe0, 0x00]).expect("write");
        let image = Image::from_path(&jpeg).expect("a JPEG");
        assert_eq!(image.mime, "image/jpeg");
        assert_eq!(image.path.as_deref(), Some(jpeg.as_path()));

        // Oversized files are refused on their metadata, before being read.
        let huge = dir.path().join("huge.png");
        std::fs::write(&huge, vec![0u8; MAX_IMAGE_BYTES + 1]).expect("write");
        assert!(matches!(
            Image::from_path(&huge).expect_err("over the cap"),
            ImageError::TooLarge { .. }
        ));

        // Not an image, and not a file at all.
        let text = dir.path().join("notes.txt");
        std::fs::write(&text, b"just words").expect("write");
        assert!(matches!(
            Image::from_path(&text).expect_err("not an image"),
            ImageError::UnknownFormat
        ));
        assert!(matches!(
            Image::from_path(dir.path()).expect_err("a directory"),
            ImageError::Unreadable { .. }
        ));
        assert!(matches!(
            Image::from_path(&dir.path().join("gone.png")).expect_err("missing"),
            ImageError::Unreadable { .. }
        ));
    }

    #[test]
    fn old_transcripts_without_images_still_load() {
        let legacy: ChatMessage =
            serde_json::from_str(r#"{"role":"user","content":"hi"}"#).unwrap();
        assert!(legacy.images().is_empty());
        assert_eq!(legacy.text(), "hi");
    }

    /// The pre-v2 wire shape, folded into blocks in the order a v2 message
    /// would have carried them. Users' session files are full of this and
    /// have to keep loading.
    #[test]
    fn a_pre_v2_message_folds_its_sibling_fields_into_blocks() {
        let legacy: ChatMessage = serde_json::from_str(
            r#"{"role":"assistant","content":"on it","tool_calls":[{"function":{"name":"execute","arguments":{"command":"ls"}}}]}"#,
        )
        .unwrap();
        assert_eq!(legacy.text(), "on it");
        assert_eq!(legacy.tool_calls().len(), 1);
        assert_eq!(legacy.tool_calls()[0].function.name, "execute");
        assert!(
            legacy.tool_calls()[0].id.is_empty(),
            "a legacy file records no id; the session loader pairs them up"
        );
        assert!(
            matches!(legacy.content.first(), Some(ContentBlock::Text(_))),
            "text first, then the calls: the order a v2 message has"
        );

        // A legacy `tool` record: its text *is* the result body.
        let result: ChatMessage = serde_json::from_str(
            r#"{"role":"tool","tool_name":"read_file","content":"fn main() {}"}"#,
        )
        .unwrap();
        assert_eq!(result.tool_results().len(), 1);
        assert_eq!(result.tool_results()[0].name, "read_file");
        assert_eq!(result.tool_results()[0].content, "fn main() {}");
        assert_eq!(result.text(), "fn main() {}");

        // A tool-call-only assistant turn does not gain a stray empty text
        // block on the way in, matching what `assistant_turn` writes.
        let empty: ChatMessage = serde_json::from_str(
            r#"{"role":"assistant","content":"","tool_calls":[{"function":{"name":"go","arguments":{}}}]}"#,
        )
        .unwrap();
        assert_eq!(empty.content.len(), 1);
        assert!(matches!(empty.content[0], ContentBlock::ToolUse(_)));

        // Some OpenAI-compatible servers send `content: null` on a
        // tool-call-only delta; that is an empty message, not a parse error.
        let null: ChatMessage =
            serde_json::from_str(r#"{"role":"assistant","content":null}"#).unwrap();
        assert!(null.content.is_empty());
        assert_eq!(null.text(), "");
    }

    #[test]
    fn every_block_kind_round_trips_through_the_session_format() {
        let message = ChatMessage::new(
            Role::Assistant,
            vec![
                ContentBlock::thinking("weighing it", Some("sig-abc".to_string())),
                ContentBlock::Thinking(ThinkingBlock {
                    thinking: String::new(),
                    signature: None,
                    data: Some("encrypted-payload".to_string()),
                }),
                ContentBlock::text("here goes"),
                ContentBlock::Image(Image::new("QUJD", "image/png")),
                ContentBlock::ToolUse(ToolCall::new("execute", json!({ "command": "ls" }))),
                ContentBlock::tool_result("toolu_1", "execute", "ok"),
            ],
        );
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value["content"][0]["type"], "thinking");
        assert_eq!(value["content"][0]["signature"], "sig-abc");
        assert_eq!(value["content"][1]["data"], "encrypted-payload");
        assert!(
            value["content"][1].get("signature").is_none(),
            "an absent signature is absent on the wire"
        );
        assert_eq!(value["content"][2]["type"], "text");
        assert_eq!(value["content"][3]["type"], "image");
        assert_eq!(value["content"][4]["type"], "tool_use");
        assert_eq!(value["content"][5]["type"], "tool_result");

        let back: ChatMessage = serde_json::from_value(value).unwrap();
        assert_eq!(back.content, message.content);
        // Reasoning is how the model got to the answer, not the answer.
        assert_eq!(back.text(), "here goesok");
    }

    #[test]
    fn synthetic_tool_call_ids_are_unique() {
        let ids: std::collections::BTreeSet<String> =
            (0..64).map(|_| synthetic_tool_call_id()).collect();
        assert_eq!(ids.len(), 64, "two calls must never share an id");

        // Only the calls that arrived without one are given one.
        let mut calls = vec![
            ToolCall {
                id: "toolu_kept".to_string(),
                function: FunctionCall {
                    name: "execute".to_string(),
                    arguments: json!({}),
                },
            },
            ToolCall {
                id: String::new(),
                function: FunctionCall {
                    name: "execute".to_string(),
                    arguments: json!({}),
                },
            },
        ];
        ensure_tool_call_ids(&mut calls);
        assert_eq!(calls[0].id, "toolu_kept");
        assert!(!calls[1].id.is_empty());
    }

    #[test]
    fn estimated_tokens_scales_with_content_and_images() {
        let short = ChatMessage::user("abcd"); // 4 chars → 1 token
        assert_eq!(short.estimated_tokens(), 1);
        let long = ChatMessage::user("a".repeat(400)); // 400 chars → 100 tokens
        assert_eq!(long.estimated_tokens(), 100);
        let with_image =
            ChatMessage::user_with_images("see", vec![Image::new("QUJD", "image/png")]);
        // 3 chars → 1 token + 1000 image allowance
        assert_eq!(with_image.estimated_tokens(), 1_001);
        assert_eq!(
            estimate_history_tokens(&[short, long]),
            101,
            "history sums message estimates"
        );
    }

    #[test]
    fn assistant_tool_call_round_trips() {
        let mut message = ChatMessage::assistant("");
        message.push_tool_call(ToolCall::new("execute", json!({ "command": "cargo test" })));
        let id = message.tool_calls()[0].id.clone();

        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value["content"][0]["type"], "text");
        assert_eq!(value["content"][1]["type"], "tool_use");
        assert_eq!(value["content"][1]["id"], id);
        assert_eq!(value["content"][1]["function"]["name"], "execute");
        assert_eq!(
            value["content"][1]["function"]["arguments"]["command"],
            "cargo test"
        );

        let back: ChatMessage = serde_json::from_value(value).unwrap();
        assert_eq!(back.tool_calls().len(), 1);
        assert_eq!(back.tool_calls()[0].function.name, "execute");
        assert_eq!(
            back.tool_calls()[0].id,
            id,
            "the id survives the session file; it is the only correlation there is"
        );
    }

    #[test]
    fn tool_result_message_carries_its_call_id_and_tool_name() {
        let value =
            serde_json::to_value(ChatMessage::tool_result("toolu_7", "read_file", "contents"))
                .unwrap();
        assert_eq!(value["role"], "tool");
        assert_eq!(value["content"][0]["type"], "tool_result");
        assert_eq!(value["content"][0]["tool_use_id"], "toolu_7");
        assert_eq!(value["content"][0]["name"], "read_file");
        assert_eq!(value["content"][0]["content"], "contents");
    }

    #[test]
    fn provider_error_transient_classification() {
        // No status: transport failure (connect refused, timeout, dropped
        // stream) — retryable.
        assert!(ProviderError::transport("connection reset").is_transient());
        // Retryable statuses.
        for status in [408, 429, 500, 502, 503, 529] {
            assert!(
                ProviderError::http(status, "x").is_transient(),
                "HTTP {status} must be transient"
            );
        }
        // Client errors: retrying the same request cannot succeed.
        for status in [400, 401, 403, 404, 409, 413, 422] {
            assert!(
                !ProviderError::http(status, "x").is_transient(),
                "HTTP {status} must not be transient"
            );
        }
    }

    #[test]
    fn provider_error_downcasts_through_anyhow_context() {
        let err = anyhow::Error::new(ProviderError::http(429, "rate limited"))
            .context("chat request failed");
        let provider = err
            .downcast_ref::<ProviderError>()
            .expect("downcast through context");
        assert_eq!(provider.status, Some(429));
        assert!(provider.is_transient());
        assert_eq!(provider.message, "rate limited");
    }

    /// `2015-10-21T07:28:00Z`, the canonical HTTP-date example.
    const HTTP_DATE_EXAMPLE: &str = "Wed, 21 Oct 2015 07:28:00 GMT";
    const HTTP_DATE_EXAMPLE_EPOCH: u64 = 1_445_412_480;

    fn at_epoch(seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn retry_after_is_honoured_in_both_forms_and_capped() {
        // Delta-seconds, the form nearly every API sends.
        assert_eq!(
            parse_retry_after("30", at_epoch(0)),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            parse_retry_after("  30  ", at_epoch(0)),
            Some(Duration::from_secs(30)),
            "surrounding whitespace is not part of the value"
        );

        // HTTP-date, measured against the clock the caller passes in.
        assert_eq!(
            parse_retry_after(HTTP_DATE_EXAMPLE, at_epoch(HTTP_DATE_EXAMPLE_EPOCH - 45)),
            Some(Duration::from_secs(45))
        );
        // Numeric-offset spelling, which proxies sometimes rewrite to.
        assert_eq!(
            parse_retry_after(
                "Wed, 21 Oct 2015 07:28:00 +0000",
                at_epoch(HTTP_DATE_EXAMPLE_EPOCH - 45)
            ),
            Some(Duration::from_secs(45))
        );
        // A deadline already past means "go ahead", not an error.
        assert_eq!(
            parse_retry_after(HTTP_DATE_EXAMPLE, at_epoch(HTTP_DATE_EXAMPLE_EPOCH + 600)),
            Some(Duration::ZERO)
        );

        // Both forms are capped: a hostile or buggy header cannot park a turn.
        assert_eq!(
            parse_retry_after("3600", at_epoch(0)),
            Some(MAX_RETRY_AFTER),
            "an hour-long delta is clamped"
        );
        assert_eq!(
            parse_retry_after(
                HTTP_DATE_EXAMPLE,
                at_epoch(HTTP_DATE_EXAMPLE_EPOCH - 86_400)
            ),
            Some(MAX_RETRY_AFTER),
            "a day-away date is clamped"
        );

        // Unparseable values fall back to our own backoff rather than
        // guessing.
        for bad in [
            "",
            "   ",
            "soon",
            "-5",
            "12.5",
            "Wed, 32 Oct 2015 07:28:00 GMT",
        ] {
            assert_eq!(
                parse_retry_after(bad, at_epoch(0)),
                None,
                "must not invent a delay for {bad:?}"
            );
        }
    }

    #[test]
    fn retry_after_rides_under_the_provider_error_on_the_chain() {
        let err = http_error_with_retry_after(429, "rate limited", Some(Duration::from_secs(30)));
        let provider = err
            .downcast_ref::<ProviderError>()
            .expect("the provider error is still the head of the chain");
        assert_eq!(provider.status, Some(429));
        assert!(provider.is_transient());
        assert_eq!(
            err.to_string(),
            "rate limited",
            "the message users see is the provider's, not the retry hint's"
        );
        assert_eq!(
            err.downcast_ref::<RetryAfter>().map(|hint| hint.0),
            Some(Duration::from_secs(30))
        );

        // No header: nothing extra on the chain.
        let err = http_error_with_retry_after(500, "boom", None);
        assert!(err.downcast_ref::<RetryAfter>().is_none());
        assert_eq!(
            err.downcast_ref::<ProviderError>().unwrap().status,
            Some(500)
        );
    }

    #[test]
    fn retry_after_is_read_off_a_header_map() {
        use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};

        let mut headers = HeaderMap::new();
        assert_eq!(retry_after_from_headers(&headers), None);
        headers.insert(RETRY_AFTER, HeaderValue::from_static("12"));
        assert_eq!(
            retry_after_from_headers(&headers),
            Some(Duration::from_secs(12))
        );
    }

    #[test]
    fn backoff_is_jittered_so_parallel_retriers_do_not_wake_together() {
        // attempt 3 on the shipped ladder: min(300, 5 * 2^3) = 40s.
        let ceiling = Duration::from_secs(40);
        let delays: std::collections::BTreeSet<u128> = (0..32)
            .map(|_| retry_delay(3, 5, 300, None).as_millis())
            .collect();
        assert!(
            delays.len() > 1,
            "two retriers drawing at the same moment must not get the same delay"
        );
        assert!(
            delays.iter().all(|&millis| millis <= ceiling.as_millis()),
            "the jitter samples within the ladder's ceiling, never past it"
        );
        // A zero ladder (what the tests configure) still sleeps not at all.
        assert_eq!(retry_delay(4, 0, 0, None), Duration::ZERO);
    }

    #[test]
    fn the_jitter_never_eats_the_configured_backoff_floor() {
        // Full jitter (a draw from [0, ceiling)) would let the very first
        // retry after a 429 fire in milliseconds, re-hitting a rate-limited
        // endpoint for a single interactive user. `retry_base_secs` is a
        // floor: every draw waits at least that long.
        for attempt in 0..4 {
            for _ in 0..64 {
                let delay = retry_delay(attempt, 5, 300, None);
                assert!(
                    delay >= Duration::from_secs(5),
                    "attempt {attempt} slept {delay:?}, under the configured base"
                );
            }
        }
        // The floor is not the whole window: attempt 3 still spreads across
        // [5s, 40s], which is what unbunches a fleet.
        let delays: std::collections::BTreeSet<u128> = (0..64)
            .map(|_| retry_delay(3, 5, 300, None).as_millis())
            .collect();
        assert!(
            delays.iter().any(|&millis| millis > 5_000),
            "the draw is a window above the floor, not the floor itself: {delays:?}"
        );
    }

    #[test]
    fn a_server_stated_retry_after_is_a_floor_and_is_capped() {
        for _ in 0..16 {
            let delay = retry_delay(0, 5, 300, Some(Duration::from_secs(30)));
            assert!(
                delay >= Duration::from_secs(30),
                "never retry before the server's own deadline: {delay:?}"
            );
            assert!(
                delay <= Duration::from_secs(30) + RETRY_AFTER_JITTER,
                "only the jitter window is added on top: {delay:?}"
            );
            // A hostile header cannot park the turn, even if it reaches this
            // function without going through the parser's cap.
            let capped = retry_delay(0, 5, 300, Some(Duration::from_secs(3_600)));
            assert!(capped <= MAX_RETRY_AFTER + RETRY_AFTER_JITTER, "{capped:?}");
        }
    }

    #[test]
    fn a_retry_after_of_zero_cannot_pin_the_loop_at_the_endpoints_cadence() {
        // A misconfigured proxy answering every request `429` +
        // `Retry-After: 0` (and an HTTP-date already past, which parses to
        // exactly this) must not bypass the ladder: the header raises the
        // floor, it does not replace it.
        assert_eq!(parse_retry_after("0", at_epoch(0)), Some(Duration::ZERO));
        for _ in 0..16 {
            let first = retry_delay(0, 5, 300, Some(Duration::ZERO));
            assert!(
                first >= Duration::from_secs(5),
                "attempt 0 still waits the configured base: {first:?}"
            );
            let sixth = retry_delay(6, 5, 300, Some(Duration::ZERO));
            assert!(
                sixth >= Duration::from_secs(5),
                "attempt 6 still climbs its own ladder: {sixth:?}"
            );
        }
        // And the climb is real: the sixth attempt's window (min(300, 320) =
        // 300s) reaches far past the first attempt's fixed 5s.
        let sixth: Duration = (0..64)
            .map(|_| retry_delay(6, 5, 300, Some(Duration::ZERO)))
            .max()
            .expect("64 draws");
        assert!(
            sixth > Duration::from_secs(6),
            "the ladder still governs the cadence: {sixth:?}"
        );
    }

    #[test]
    fn anthropic_max_tokens_tracks_each_model_generation() {
        // The 128k line, including the vendor-prefixed Bedrock spelling.
        for model in [
            "claude-opus-5",
            "anthropic.claude-opus-5",
            "claude-opus-4-8",
            "claude-opus-4-6",
            "claude-sonnet-5",
            "claude-sonnet-4-6",
            "claude-fable-5",
            "claude-mythos-5",
        ] {
            assert_eq!(anthropic_max_output_tokens(model), 128_000, "{model}");
        }
        // 64k, 32k and the older generations.
        for (model, ceiling) in [
            ("claude-haiku-4-5", 64_000),
            ("claude-haiku-4-5-20251001", 64_000),
            ("claude-opus-4-5", 64_000),
            ("claude-sonnet-4-5-20250929", 64_000),
            ("claude-sonnet-4-20250514", 64_000),
            ("claude-3-7-sonnet-20250219", 64_000),
            ("claude-opus-4-1-20250805", 32_000),
            ("claude-opus-4-20250514", 32_000),
            ("claude-3-5-sonnet-20241022", 8_192),
            ("claude-3-5-haiku-20241022", 8_192),
            ("claude-3-opus-20240229", 4_096),
            ("claude-3-haiku-20240307", 4_096),
        ] {
            assert_eq!(anthropic_max_output_tokens(model), ceiling, "{model}");
        }
        // Case is not load-bearing.
        assert_eq!(anthropic_max_output_tokens("CLAUDE-OPUS-5"), 128_000);
        // Anything unrecognized gets the conservative floor rather than a
        // number that might 400.
        for unknown in ["claude-next", "some-proxy/model", ""] {
            assert_eq!(
                anthropic_max_output_tokens(unknown),
                DEFAULT_ANTHROPIC_MAX_TOKENS,
                "{unknown}"
            );
        }
    }

    #[test]
    fn length_cutoffs_are_recognized_across_provider_spellings() {
        // `length` is the OpenAI-compatible and Ollama spelling, `max_tokens`
        // is Anthropic's, `max_output_tokens` is the Responses API's.
        for reason in [
            "length",
            "max_tokens",
            "max_output_tokens",
            "MAX_TOKENS",
            "Max_Output_Tokens",
            " length ",
        ] {
            assert!(is_length_cutoff(reason), "{reason}");
        }
        for reason in ["stop", "tool_calls", "tool_use", "end_turn", ""] {
            assert!(!is_length_cutoff(reason), "{reason}");
        }
        // A context-window overflow is a different *condition* with a
        // different remedy, but the same consequence for a reply carrying a
        // tool call: it stopped mid-arguments. Leaving it out of this
        // predicate did not make the turn recoverable, it made the half-
        // decoded call dispatch, and `execute` with `{}` for a shell command
        // or a file edit is a different action, not a smaller one.
        for reason in [
            "model_context_window_exceeded",
            "context_length_exceeded",
            "MODEL_CONTEXT_WINDOW_EXCEEDED",
            " context_length_exceeded ",
        ] {
            assert!(is_length_cutoff(reason), "{reason} is a cutoff too");
            assert!(is_context_overflow(reason), "{reason}");
        }
        // The kinds stay distinguishable, which is what lets the two get
        // different advice.
        for reason in ["length", "max_tokens", "max_output_tokens"] {
            assert!(!is_context_overflow(reason), "{reason}");
        }
        for reason in ["stop", "tool_use", ""] {
            assert!(!is_context_overflow(reason), "{reason}");
        }
    }

    /// Adversarial: the refusal is only useful if the user can act on it, and
    /// the two cutoffs need opposite moves. A context overflow told to "ask
    /// for a smaller reply" sends the user round the same loop until they
    /// give up, because the reply was never the problem.
    #[test]
    fn a_truncated_call_names_the_remedy_for_the_cutoff_it_hit() {
        let overflow = TruncatedToolCall {
            reason: "model_context_window_exceeded".to_string(),
            tool: "execute".to_string(),
        }
        .to_string();
        assert!(overflow.contains("context window"), "{overflow}");
        assert!(overflow.contains("/compact"), "{overflow}");
        assert!(overflow.contains("execute"), "{overflow}");
        assert!(
            !overflow.contains("output-token limit"),
            "the wrong remedy is worse than none: {overflow}"
        );

        let ceiling = TruncatedToolCall {
            reason: "length".to_string(),
            tool: "edit_file".to_string(),
        }
        .to_string();
        assert!(ceiling.contains("output-token limit"), "{ceiling}");
        assert!(ceiling.contains("smaller steps"), "{ceiling}");
        assert!(!ceiling.contains("/compact"), "{ceiling}");
        // Both say what was discarded and why, which is the part the agent
        // loop turns into the user-visible failure.
        for message in [overflow, ceiling] {
            assert!(
                message.contains("truncated call was discarded"),
                "{message}"
            );
        }
    }

    #[test]
    fn an_endpoint_on_this_machine_is_local_whatever_provider_kind_serves_it() {
        // The failing case: LM Studio / vLLM / text-generation-webui on
        // loopback, configured as an `openai` provider, used to inherit the
        // 300s cloud read timeout and have a long local prefill killed.
        for local in [
            "http://127.0.0.1:1234/v1",
            "http://localhost:11435/v1",
            "http://[::1]:8080/v1",
            "http://10.0.0.5:8080/v1",
            "http://172.16.4.4:8080",
            "http://192.168.1.50:11434/v1",
            "http://169.254.7.7:8080",
            "http://0.0.0.0:8080/v1",
            "http://[::]:8080/v1",
            "http://[fd00::1]:8080/v1",
            "http://[fe80::1]:8080/v1",
            "http://gpu-box:8080/v1",
            "http://workstation.local:8080/v1",
            "http://inference.lan/v1",
            "http://llm.internal/v1",
            "http://box.home.arpa/v1",
            "http://user:pass@127.0.0.1:1234/v1",
        ] {
            assert_eq!(endpoint_locality(local), Locality::Local, "{local}");
            assert_eq!(client_read_timeout_for(local), None, "{local}");
        }

        // Hosted APIs (including anything whose host we cannot read) keep the
        // stall detector: silence there really is a dead connection.
        for cloud in [
            "https://api.openai.com/v1",
            "https://openrouter.ai/api/v1",
            "https://api.anthropic.com",
            "https://api.cloudflare.com/client/v4/accounts/acc/ai/v1",
            "https://gpu.example.com:8443/v1",
            // `.localhost` is local, `localhost.example.com` is not.
            "https://localhost.example.com/v1",
            "",
        ] {
            assert_eq!(endpoint_locality(cloud), Locality::Cloud, "{cloud}");
            assert_eq!(
                client_read_timeout_for(cloud),
                Some(CLOUD_READ_TIMEOUT),
                "{cloud}"
            );
        }

        // The scope still wins for a local-inference server reached over a
        // public name (an SSH tunnel, a Tailscale hostname): the two signals
        // are independent and either one is enough.
        assert_eq!(
            with_local_inference_timeouts(|| client_read_timeout_for("https://gpu.example.com")),
            None
        );
    }

    #[test]
    fn local_inference_drops_the_cloud_read_timeout_for_its_scope_only() {
        assert_eq!(
            client_read_timeout(),
            Some(CLOUD_READ_TIMEOUT),
            "cloud is the default policy"
        );
        let inside = with_local_inference_timeouts(client_read_timeout);
        assert_eq!(
            inside, None,
            "a slow local prefill is not a stalled connection"
        );
        assert_eq!(
            client_read_timeout(),
            Some(CLOUD_READ_TIMEOUT),
            "the scope is restored on the way out"
        );

        // A panic inside the scope must not leave the thread stuck on the
        // local policy for every client it builds afterwards.
        let panicked = std::panic::catch_unwind(|| {
            with_local_inference_timeouts(|| -> Option<Duration> {
                panic!("provider construction blew up")
            })
        });
        assert!(panicked.is_err());
        assert_eq!(client_read_timeout(), Some(CLOUD_READ_TIMEOUT));
    }

    /// A loopback server that answers with headers promising a body and then
    /// never sends one, holding the connection open. That is what a stalled
    /// stream looks like from the client side, and it is the only way to
    /// observe a read timeout: `reqwest` exposes no accessor for the client's
    /// own configuration.
    async fn stalling_server() -> String {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("binding a loopback port");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 4096];
                    let _ = socket.read(&mut buf).await;
                    let _ = socket
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\n\
                              content-length: 64\r\n\r\n",
                        )
                        .await;
                    let _ = socket.flush().await;
                    // The body never arrives, and the socket stays open.
                    tokio::time::sleep(Duration::from_secs(30)).await;
                });
            }
        });
        format!("http://{addr}/")
    }

    /// Adversarial: the builder itself, not the number that was passed to it.
    /// Every other test here asserts a recorded field, so inverting
    /// `chat_http_builder`'s match (cloud endpoints losing the stall detector,
    /// local ones gaining it, exactly the failure `Locality` exists to
    /// prevent) left the whole suite green.
    #[tokio::test]
    async fn the_read_timeout_reaches_the_client_that_was_built() {
        let url = stalling_server().await;

        // With a stall detector: the body read gives up on its own.
        let detector = chat_http_builder(Some(Duration::from_millis(200)))
            .build()
            .expect("building a client");
        let response = detector.get(&url).send().await.expect("headers arrive");
        let err = response
            .text()
            .await
            .expect_err("a stalled body must time out");
        assert!(
            err.is_timeout() || err.is_body() || err.is_decode(),
            "expected a read timeout, got {err}"
        );

        // Without one: the same stall is simply waited on, which is what a
        // long local prefill needs. Asserted as "still pending well past the
        // window the detector used", so it cannot pass by timing luck.
        let patient = chat_http_builder(None).build().expect("building a client");
        let response = patient.get(&url).send().await.expect("headers arrive");
        assert!(
            tokio::time::timeout(Duration::from_millis(700), response.text())
                .await
                .is_err(),
            "a client with no read timeout must still be waiting"
        );
    }

    /// The one client policy in this module that does have a whole-request
    /// ceiling, and the reason it needs one.
    ///
    /// A token refresh runs on the way into a model call, under the token
    /// source's mutex. A bare `reqwest::Client::new()` — which is what every
    /// OAuth call site used to build — has no connect timeout and no request
    /// timeout at all, so a token host that accepts the connection and then
    /// goes silent parks the turn for the lifetime of the process: no error to
    /// classify, no retry to make, and a spinner that never moves. That is
    /// indistinguishable from the agent having stopped.
    ///
    /// Asserted through the builder rather than the shipped constant because
    /// `reqwest` exposes no accessor for a built client's timeouts, and a test
    /// that waited out the real sixty seconds would not be run.
    #[tokio::test]
    async fn an_oauth_client_gives_up_on_a_silent_token_endpoint() {
        let url = stalling_server().await;
        let client = oauth_http_builder(Duration::from_millis(200))
            .build()
            .expect("building a client");
        // `send` resolves on the headers; the ceiling covers the whole
        // exchange, which is where a wedged endpoint actually strands a
        // caller — headers promising a body that never comes.
        let response = client.get(&url).send().await.expect("headers arrive");
        let err = response
            .text()
            .await
            .expect_err("a token endpoint that never answers must time out");
        assert!(
            err.is_timeout() || err.is_body(),
            "expected a timeout, got {err}"
        );

        // And the shipped ceiling is a real one: generous enough for a slow
        // handshake, finite enough that a wedged endpoint ends the call.
        assert!(OAUTH_REQUEST_TIMEOUT >= Duration::from_secs(30));
        assert!(OAUTH_REQUEST_TIMEOUT <= Duration::from_secs(120));
    }

    /// A context-window overflow is the one permanent failure with a
    /// mechanical remedy, so it has to be distinguishable from every other
    /// 400. It stays permanent either way — re-sending the identical
    /// oversized prompt cannot work — but a caller that can compact needs to
    /// know which 400 this is.
    #[test]
    fn a_context_overflow_is_told_apart_from_an_ordinary_bad_request() {
        for body in [
            "https://api.openai.com/v1 returned HTTP 400: {\"error\":{\"code\":\"context_length_exceeded\"}}",
            "This model's maximum context length is 128000 tokens, however you requested 131000",
            "prompt is too long: 213000 tokens > 200000 maximum",
            "MODEL_CONTEXT_WINDOW_EXCEEDED",
            "the request exceeds the available context size",
        ] {
            let err = ProviderError::http(400, body);
            assert!(err.is_context_overflow(), "{body}");
            assert!(
                !err.is_transient(),
                "still permanent: the same prompt cannot fit on a second try"
            );
        }
        // A 413 is how a couple of gateways spell the same thing.
        assert!(ProviderError::http(413, "prompt is too long: 9 > 8").is_context_overflow());

        // Every other 400, and every status that is not a 400 at all. Nothing
        // here may read as an overflow: a caller acting on this compacts the
        // history, and compacting in response to a malformed request throws
        // away the conversation and fails again.
        for (status, body) in [
            (400, "invalid value for 'temperature'"),
            (400, "unknown tool 'execute'"),
            (401, "prompt is too long"),
            (429, "maximum context length"),
            (500, "context_length_exceeded"),
        ] {
            assert!(
                !ProviderError::http(status, body).is_context_overflow(),
                "HTTP {status}: {body}"
            );
        }
        assert!(!ProviderError::transport("connection reset").is_context_overflow());
    }

    /// A stream that stopped before the provider said it was finished is
    /// transient, and typed so the ladder can see that without guessing.
    #[test]
    fn a_stream_that_ended_early_is_a_transient_transport_failure() {
        let err = stream_ended_early("the response stream");
        let provider = err
            .downcast_ref::<ProviderError>()
            .expect("typed, or the ladder classifies it by fallback");
        assert_eq!(provider.status, None);
        assert!(provider.is_transient());
        assert!(provider.message.contains("the response stream"), "{err:#}");
        assert!(
            provider.message.contains("cut mid-response"),
            "the message has to say what happened, not just that something did: {err:#}"
        );
    }

    #[test]
    fn tool_call_with_missing_arguments_deserializes_to_null() {
        // Ollama may omit `arguments` entirely; the agent normalizes null later.
        let call: ToolCall = serde_json::from_str(r#"{"function":{"name":"git_status"}}"#).unwrap();
        assert_eq!(call.function.name, "git_status");
        assert!(call.function.arguments.is_null());
    }
}
