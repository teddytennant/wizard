//! What a node says it can do, and what this machine will let a peer spend.
//!
//! Two halves of the same question, kept in one file because they are read
//! together at every admission decision:
//!
//! - [`Capability`] is *advertised*. It arrives over the wire, from a machine
//!   this one does not control, so every string in it is [`PeerText`] and
//!   every list is length-capped. Nothing in here is evidence: a peer claiming
//!   `accepts_work: true` has said something, not proved something.
//! - [`Limits`] is *ours*. It is the ceiling on what a peer can cause this
//!   machine to spend, and it applies to trusted peers too, because the plan's
//!   threat model is not only a hostile peer: "a trusted peer with a bug can
//!   spend your API budget", and a retry loop on the other side of a mesh link
//!   looks exactly like an attack until the bill arrives. A ceiling and not a
//!   tripwire: admission needs a whole request's worth of budget still
//!   unspent, because what a request costs is not known until after it has
//!   run. See [`Limits::max_usd_per_request`].
//!
//! `accepts_work` defaults to `false` in every direction a default can be
//! taken: the [`Default`] impl, a record that omits the field, and a record
//! written by a version of Wizard that had not invented the field yet.

use chrono::{DateTime, Utc};
use serde::de::{IgnoredAny, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use super::PeerText;

/// Most entries kept per capability list. A peer with more models than this
/// is either unusual or hostile, and the graph explorer has to draw whatever
/// arrives, so the cap is what stops one announcement from being a rendering
/// denial of service.
pub const MAX_ENTRIES: usize = 64;

/// Which list an entry came from. The graph explorer draws one capability
/// vertex per (kind, name), so the kind travels with the name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityKind {
    Model,
    Tool,
    Skill,
    Subagent,
}

impl CapabilityKind {
    /// Every kind, in the order the explorer groups them.
    pub const ALL: [CapabilityKind; 4] = [
        CapabilityKind::Model,
        CapabilityKind::Tool,
        CapabilityKind::Skill,
        CapabilityKind::Subagent,
    ];

    /// Singular label, for a graph legend.
    pub fn label(self) -> &'static str {
        match self {
            CapabilityKind::Model => "model",
            CapabilityKind::Tool => "tool",
            CapabilityKind::Skill => "skill",
            CapabilityKind::Subagent => "subagent",
        }
    }
}

/// What a node advertises.
///
/// Deserialisation goes through [`CapabilityWire`], so a record from the
/// network, from disk, or from a hand edit is normalised on the way in rather
/// than at each read: empty entries dropped, duplicates collapsed, lists
/// truncated to [`MAX_ENTRIES`], every string sanitised by [`PeerText`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "CapabilityWire")]
pub struct Capability {
    pub models: Vec<PeerText>,
    pub tools: Vec<PeerText>,
    pub skills: Vec<PeerText>,
    pub subagents: Vec<PeerText>,
    /// Whether this node will run work submitted by other nodes.
    ///
    /// False by default, everywhere. The plan's words are "default posture is
    /// deny", and this codebase has shipped the other kind twice (the Telegram
    /// allowlist that defaulted to allow-all, project hooks that executed
    /// themselves), so the default is pinned by a test rather than by this
    /// comment.
    pub accepts_work: bool,
}

impl Capability {
    /// The capability of a node that advertises nothing and accepts nothing.
    pub fn none() -> Self {
        Self::default()
    }

    /// Build a capability from strings this machine produced itself (its own
    /// model list, its own tool registry). Still sanitised and still capped:
    /// a local model name comes from a config file, and config files get
    /// pasted into from the internet too.
    pub fn advertise(
        models: &[&str],
        tools: &[&str],
        skills: &[&str],
        subagents: &[&str],
        accepts_work: bool,
    ) -> Self {
        Self::from(CapabilityWire {
            models: to_text(models),
            tools: to_text(tools),
            skills: to_text(skills),
            subagents: to_text(subagents),
            accepts_work,
        })
    }

    /// Re-run the ingest normalisation.
    ///
    /// Idempotent, and for the one path serde does not cover: a transport that
    /// builds the struct itself rather than decoding it. "The transport is
    /// supposed to have done it" is how unsanitised input gets in the day a
    /// second transport appears, so [`super::Mesh::refresh`] calls this on
    /// everything it is handed.
    pub fn normalised(self) -> Self {
        Self {
            models: normalise(self.models),
            tools: normalise(self.tools),
            skills: normalise(self.skills),
            subagents: normalise(self.subagents),
            accepts_work: self.accepts_work,
        }
    }

    /// The entries of one list.
    pub fn entries(&self, kind: CapabilityKind) -> &[PeerText] {
        match kind {
            CapabilityKind::Model => &self.models,
            CapabilityKind::Tool => &self.tools,
            CapabilityKind::Skill => &self.skills,
            CapabilityKind::Subagent => &self.subagents,
        }
    }

    /// Total advertised entries across every list, for a graph label.
    pub fn len(&self) -> usize {
        CapabilityKind::ALL
            .iter()
            .map(|kind| self.entries(*kind).len())
            .sum()
    }

    /// Whether the node advertises nothing at all.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The shape a capability is decoded from, before normalisation.
///
/// Every field is `#[serde(default)]`: a truncated record, a record from an
/// older peer, or `{}` decodes to "advertises nothing, accepts nothing"
/// instead of failing the whole announcement. Failing would be no safer (the
/// peer simply would not appear) and would make an older peer's presence
/// depend on this node's version.
///
/// Every list is decoded through [`capped_list`] rather than as a plain
/// `Vec<PeerText>`, so [`MAX_ENTRIES`] bounds the *memory* and not only the
/// value that comes out the other end.
#[derive(Deserialize)]
struct CapabilityWire {
    #[serde(default, deserialize_with = "capped_list")]
    models: Vec<PeerText>,
    #[serde(default, deserialize_with = "capped_list")]
    tools: Vec<PeerText>,
    #[serde(default, deserialize_with = "capped_list")]
    skills: Vec<PeerText>,
    #[serde(default, deserialize_with = "capped_list")]
    subagents: Vec<PeerText>,
    #[serde(default)]
    accepts_work: bool,
}

/// Decode a capability list, keeping at most [`MAX_ENTRIES`] of it.
///
/// The cap has to be applied *during* the decode, not after it. A plain
/// `Vec<PeerText>` field makes serde materialise every element of an
/// announcement before anything gets to truncate it, so a peer that sends
/// half a gigabyte of model names costs half a gigabyte of this machine's
/// memory to reject. Here the elements past the cap are skipped with
/// [`serde::de::IgnoredAny`], which walks the input to find its end (it has
/// to; the record's own framing depends on it) without building a value out
/// of any of it.
///
/// A consequence worth naming: an element past the cap is never decoded, so a
/// malformed one there does not fail the announcement. That is the same trade
/// as the `#[serde(default)]` fields above, in the same direction, and it is
/// the one that keeps the cost of a hostile list proportional to what is kept.
///
/// Dropping empties and duplicates here as well as in [`normalise`] is not
/// redundancy for its own sake: without it, `MAX_ENTRIES` copies of one name
/// would fill the cap and crowd out everything real.
fn capped_list<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<PeerText>, D::Error> {
    struct CappedList;

    impl<'de> Visitor<'de> for CappedList {
        type Value = Vec<PeerText>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "a list of at most {MAX_ENTRIES} capability names")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            // The size hint is the peer's claim about the length of its own
            // list, so it is capped before it becomes an allocation.
            let mut kept: Vec<PeerText> =
                Vec::with_capacity(seq.size_hint().unwrap_or(0).min(MAX_ENTRIES));
            while kept.len() < MAX_ENTRIES {
                match seq.next_element::<PeerText>()? {
                    Some(entry) => {
                        if !entry.is_empty() && !kept.contains(&entry) {
                            kept.push(entry);
                        }
                    }
                    None => return Ok(kept),
                }
            }
            while seq.next_element::<IgnoredAny>()?.is_some() {}
            Ok(kept)
        }
    }

    deserializer.deserialize_seq(CappedList)
}

impl From<CapabilityWire> for Capability {
    fn from(wire: CapabilityWire) -> Self {
        Self {
            models: wire.models,
            tools: wire.tools,
            skills: wire.skills,
            subagents: wire.subagents,
            accepts_work: wire.accepts_work,
        }
        .normalised()
    }
}

/// Drop the empties, collapse duplicates, keep the first [`MAX_ENTRIES`].
///
/// Order is preserved rather than sorted: the peer's own ordering is the only
/// hint available about which model it would rather be asked for.
fn normalise(entries: Vec<PeerText>) -> Vec<PeerText> {
    let mut seen: Vec<PeerText> = Vec::new();
    for entry in entries {
        if entry.is_empty() || seen.contains(&entry) {
            continue;
        }
        seen.push(entry);
        if seen.len() == MAX_ENTRIES {
            break;
        }
    }
    seen
}

fn to_text(raw: &[&str]) -> Vec<PeerText> {
    raw.iter().map(|entry| PeerText::sanitize(entry)).collect()
}

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

/// Per-peer ceilings on what a peer may cause this machine to spend.
///
/// Deliberately small defaults. A mesh peer is not a user of this machine; it
/// is another machine that was allowed to ask. Raising these is a decision the
/// operator makes per peer, and the failure mode of a too-low limit is a
/// refusal the operator can see, while the failure mode of a too-high one is
/// a bill.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    /// Admitted requests per rolling minute.
    pub requests_per_minute: u32,
    /// API spend attributable to this peer, per rolling day, in US dollars.
    pub cost_usd_per_day: f64,
    /// Most a single admitted request may cost, in US dollars.
    ///
    /// The field that makes [`Limits::cost_usd_per_day`] a ceiling instead of
    /// a tripwire. Spend is only known *after* the work runs, so a day limit
    /// checked alone answers "has this peer already overspent?", which is a
    /// question that arrives too late: a peer one cent under a $0.50 day can
    /// still be admitted into a 200k-token turn and hand back a $12 bill.
    ///
    /// [`Meter::try_admit`] therefore refuses unless the day has this much
    /// room left in it, so the worst case is the limit plus one request rather
    /// than the limit plus whatever the model felt like. That bound holds as
    /// long as the caller running the work bounds it at this figure too, which
    /// is what this field is for: it is the number to hand the provider as a
    /// budget, not a number to read back afterwards.
    pub max_usd_per_request: f64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            requests_per_minute: 6,
            cost_usd_per_day: 0.50,
            max_usd_per_request: 0.25,
        }
    }
}

/// Why a request was not admitted. Carries the numbers so the refusal can say
/// what to raise instead of just saying no.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LimitExceeded {
    /// The per-minute request budget is spent.
    Rate { limit: u32 },
    /// The per-day cost budget has no room left for another request.
    Cost {
        spent_usd: f64,
        limit_usd: f64,
        /// What one more request could cost: [`Limits::max_usd_per_request`].
        /// Present so the refusal can explain a peer being turned away with
        /// money still on the clock.
        per_request_usd: f64,
    },
}

impl std::fmt::Display for LimitExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LimitExceeded::Rate { limit } => write!(
                f,
                "rate limit reached ({limit} request(s) per minute for this peer)"
            ),
            LimitExceeded::Cost {
                spent_usd,
                limit_usd,
                per_request_usd,
            } => write!(
                f,
                "cost limit reached (${spent_usd:.2} of ${limit_usd:.2} spent for this peer \
                 today, and one more request may cost ${per_request_usd:.2})"
            ),
        }
    }
}

/// Runtime accounting for one peer: requests this minute, spend today.
///
/// Not persisted, on purpose. A restart resets the windows, which is the
/// forgiving direction, and persisting it would put a counter a peer can move
/// into a file every startup path reads. The durable half of the decision is
/// [`Limits`], which lives with the peer record.
#[derive(Debug, Clone, Copy)]
pub struct Meter {
    minute_start: DateTime<Utc>,
    requests_this_minute: u32,
    day_start: DateTime<Utc>,
    cost_today_usd: f64,
}

impl Meter {
    /// A fresh meter with both windows opening at `now`.
    pub fn new(now: DateTime<Utc>) -> Self {
        Self {
            minute_start: now,
            requests_this_minute: 0,
            day_start: now,
            cost_today_usd: 0.0,
        }
    }

    /// Count one request against `limits`, or refuse it.
    ///
    /// The cost check runs first: a peer that has already spent the day's
    /// budget should be told about the money rather than about the rate, since
    /// that is the limit that will still be there in a minute.
    ///
    /// The check is for *headroom*, not for exhaustion. Refusing only once
    /// spend has already passed the limit would make the limit a record of
    /// what was overspent rather than a bound on it, because the size of a
    /// request is not known until it has run. See
    /// [`Limits::max_usd_per_request`].
    pub fn try_admit(&mut self, limits: &Limits, now: DateTime<Utc>) -> Result<(), LimitExceeded> {
        self.roll(now);
        if self.cost_today_usd + limits.max_usd_per_request > limits.cost_usd_per_day {
            return Err(LimitExceeded::Cost {
                spent_usd: self.cost_today_usd,
                limit_usd: limits.cost_usd_per_day,
                per_request_usd: limits.max_usd_per_request,
            });
        }
        if self.requests_this_minute >= limits.requests_per_minute {
            return Err(LimitExceeded::Rate {
                limit: limits.requests_per_minute,
            });
        }
        self.requests_this_minute += 1;
        Ok(())
    }

    /// Attribute `usd` of API spend to this peer.
    pub fn charge(&mut self, usd: f64, now: DateTime<Utc>) {
        self.roll(now);
        // A negative or non-finite charge is a caller bug, not a refund.
        if usd.is_finite() && usd > 0.0 {
            self.cost_today_usd += usd;
        }
    }

    /// Spend attributed to this peer in the current day window.
    pub fn spent_usd(&self) -> f64 {
        self.cost_today_usd
    }

    /// Requests admitted in the current minute window.
    pub fn requests_this_minute(&self) -> u32 {
        self.requests_this_minute
    }

    /// Advance the windows to cover `now`.
    ///
    /// A `now` *before* a window start (the clock stepped back, or an NTP
    /// correction landed mid-session) reopens the window rather than leaving a
    /// counter that can never expire. That is the forgiving direction and it
    /// is the right one: the alternative is a peer locked out until the
    /// machine reboots, decided by a clock nobody looked at.
    fn roll(&mut self, now: DateTime<Utc>) {
        let minute = now.signed_duration_since(self.minute_start).num_seconds();
        if !(0..60).contains(&minute) {
            self.minute_start = now;
            self.requests_this_minute = 0;
        }
        let day = now.signed_duration_since(self.day_start).num_seconds();
        if !(0..86_400).contains(&day) {
            self.day_start = now;
            self.cost_today_usd = 0.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;

    use super::*;

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + seconds, 0).expect("timestamp")
    }

    #[test]
    fn accepts_work_is_false_by_default_in_every_direction() {
        // The default value.
        assert!(!Capability::default().accepts_work);
        assert!(!Capability::none().accepts_work);
        // A record that never mentions the field.
        let decoded: Capability = serde_json::from_str("{}").expect("decode");
        assert!(!decoded.accepts_work);
        let decoded: Capability =
            serde_json::from_str(r#"{"models":["qwen3.6:27b"]}"#).expect("decode");
        assert!(!decoded.accepts_work);
        assert!(!decoded.is_empty(), "the rest of the record still decodes");
        // And the one way it can be true: the peer said so.
        let decoded: Capability = serde_json::from_str(r#"{"accepts_work":true}"#).expect("decode");
        assert!(decoded.accepts_work);
    }

    #[test]
    fn an_announcement_is_normalised_on_the_way_in() {
        let raw = serde_json::json!({
            "models": ["  qwen3.6:27b  ", "qwen3.6:27b", "", "   ", "gpt\u{0007}-5"],
            "tools": ["read\nfile"],
            "accepts_work": true,
        });
        let caps: Capability = serde_json::from_value(raw).expect("decode");
        assert_eq!(
            caps.models.iter().map(PeerText::as_str).collect::<Vec<_>>(),
            vec!["qwen3.6:27b", "gpt -5"],
            "trimmed, de-duplicated, control characters neutralised"
        );
        assert_eq!(caps.tools[0].as_str(), "read file");
        assert!(caps.skills.is_empty());
    }

    #[test]
    fn a_flood_of_entries_is_truncated_rather_than_rendered() {
        let models: Vec<String> = (0..MAX_ENTRIES * 4).map(|i| format!("model-{i}")).collect();
        let caps: Capability =
            serde_json::from_value(serde_json::json!({ "models": models })).expect("decode");
        assert_eq!(caps.models.len(), MAX_ENTRIES);
        assert_eq!(caps.models[0].as_str(), "model-0");
        assert_eq!(caps.len(), MAX_ENTRIES);
    }

    #[test]
    fn advertise_builds_the_same_normalised_shape() {
        let caps = Capability::advertise(
            &["qwen3.6:27b", "qwen3.6:27b"],
            &["read_file", ""],
            &[],
            &["reviewer"],
            true,
        );
        assert_eq!(caps.models.len(), 1);
        assert_eq!(caps.tools.len(), 1);
        assert_eq!(
            caps.entries(CapabilityKind::Subagent)[0].as_str(),
            "reviewer"
        );
        assert_eq!(caps.entries(CapabilityKind::Skill).len(), 0);
        assert_eq!(caps.len(), 3);
        assert!(caps.accepts_work, "the local node may opt itself in");
    }

    #[test]
    fn a_capability_survives_a_json_round_trip() {
        let caps = Capability::advertise(&["m"], &["t"], &["s"], &["a"], false);
        let json = serde_json::to_string(&caps).expect("encode");
        let back: Capability = serde_json::from_str(&json).expect("decode");
        assert_eq!(back, caps);
    }

    #[test]
    fn the_rate_limit_admits_exactly_the_budget_then_refuses() {
        let limits = Limits {
            requests_per_minute: 3,
            cost_usd_per_day: 10.0,
            ..Limits::default()
        };
        let mut meter = Meter::new(at(0));
        for i in 0..3 {
            meter
                .try_admit(&limits, at(i))
                .unwrap_or_else(|err| panic!("request {i} must be admitted: {err}"));
        }
        let err = meter.try_admit(&limits, at(3)).expect_err("over budget");
        assert_eq!(err, LimitExceeded::Rate { limit: 3 });
        assert!(err.to_string().contains("per minute"), "{err}");
        assert_eq!(meter.requests_this_minute(), 3);

        // Still inside the window a second before it closes.
        assert!(meter.try_admit(&limits, at(59)).is_err());
        // …and open again once it has.
        meter.try_admit(&limits, at(60)).expect("new window");
        assert_eq!(meter.requests_this_minute(), 1);
    }

    #[test]
    fn the_cost_limit_refuses_before_the_rate_limit_does() {
        let limits = Limits {
            requests_per_minute: 100,
            cost_usd_per_day: 0.50,
            // A request this small keeps the headroom rule out of the way, so
            // what this test measures is the order of the two refusals.
            max_usd_per_request: 0.01,
        };
        let mut meter = Meter::new(at(0));
        meter.try_admit(&limits, at(0)).expect("first request");
        meter.charge(0.49, at(1));
        meter.try_admit(&limits, at(2)).expect("still under budget");
        meter.charge(0.02, at(3));
        let err = meter.try_admit(&limits, at(4)).expect_err("over budget");
        assert!(
            matches!(err, LimitExceeded::Cost { .. }),
            "the money is the limit that will still be there in a minute: {err:?}"
        );
        assert!(err.to_string().contains("$0.51"), "{err}");

        // The day window rolls.
        meter.try_admit(&limits, at(86_400)).expect("new day");
        assert_eq!(meter.spent_usd(), 0.0);
    }

    #[test]
    fn one_request_cannot_be_admitted_into_a_budget_too_small_to_hold_it() {
        // The reason `cost_usd_per_day` is a ceiling and not a record of what
        // was overspent. A peer a penny under its day limit used to be
        // admitted, and what it was admitted *into* is a turn whose cost
        // nobody knows until it is over.
        let limits = Limits {
            requests_per_minute: 100,
            cost_usd_per_day: 0.50,
            max_usd_per_request: 0.25,
        };
        let mut meter = Meter::new(at(0));
        meter.try_admit(&limits, at(0)).expect("nothing spent yet");
        meter.charge(0.25, at(1));
        meter
            .try_admit(&limits, at(2))
            .expect("exactly one more request fits");
        meter.charge(0.25, at(3));

        // Spend is now *at* the limit rather than past it, which the old
        // exhaustion check would have waved through for one more request.
        assert_eq!(meter.spent_usd(), 0.50);
        let err = meter.try_admit(&limits, at(4)).expect_err("no room left");
        assert_eq!(
            err,
            LimitExceeded::Cost {
                spent_usd: 0.50,
                limit_usd: 0.50,
                per_request_usd: 0.25,
            }
        );

        // And a peer with money on the clock is still refused when what is
        // left cannot cover one request, with a refusal that says why rather
        // than reading as an arithmetic error.
        let mut meter = Meter::new(at(0));
        meter.charge(0.40, at(0));
        let err = meter
            .try_admit(&limits, at(1))
            .expect_err("not enough room");
        let message = err.to_string();
        assert!(message.contains("$0.40 of $0.50"), "{message}");
        assert!(message.contains("may cost $0.25"), "{message}");
    }

    #[test]
    fn a_clock_that_steps_backwards_reopens_the_window_instead_of_wedging_it() {
        let limits = Limits {
            requests_per_minute: 1,
            cost_usd_per_day: 1.0,
            ..Limits::default()
        };
        let mut meter = Meter::new(at(1_000));
        meter.try_admit(&limits, at(1_000)).expect("first request");
        assert!(meter.try_admit(&limits, at(1_001)).is_err());
        // NTP drags the clock back an hour: the peer must not be locked out
        // until the machine reboots.
        meter
            .try_admit(&limits, at(1_000) - TimeDelta::seconds(3_600))
            .expect("window reopens");
    }

    #[test]
    fn a_nonsense_charge_cannot_refund_a_peers_budget() {
        let mut meter = Meter::new(at(0));
        meter.charge(1.0, at(0));
        meter.charge(-5.0, at(1));
        meter.charge(f64::NAN, at(2));
        meter.charge(f64::INFINITY, at(3));
        assert_eq!(meter.spent_usd(), 1.0);
    }

    #[test]
    fn limits_default_small_and_survive_a_round_trip() {
        let limits = Limits::default();
        assert_eq!(limits.requests_per_minute, 6);
        assert!(limits.cost_usd_per_day <= 1.0, "{limits:?}");
        assert!(limits.max_usd_per_request > 0.0, "{limits:?}");
        assert!(
            limits.max_usd_per_request <= limits.cost_usd_per_day,
            "a request nobody could afford twice is a day limit of one request: {limits:?}"
        );
        let json = serde_json::to_string(&limits).expect("encode");
        assert_eq!(
            serde_json::from_str::<Limits>(&json).expect("decode"),
            limits
        );
        // A record written before a field existed takes the default, not zero
        // (a zero rate limit would refuse every request from every peer, and a
        // zero per-request ceiling would put the day limit back to being a
        // tripwire a single request can walk straight past).
        let partial: Limits = serde_json::from_str("{}").expect("decode");
        assert_eq!(partial, limits);
        let older: Limits =
            serde_json::from_str(r#"{"requests_per_minute":2,"cost_usd_per_day":0.2}"#)
                .expect("decode");
        assert_eq!(older.max_usd_per_request, limits.max_usd_per_request);
    }

    #[test]
    fn entries_past_the_cap_are_never_decoded_at_all() {
        // The cap has to bite *during* the decode, not after it: a plain
        // `Vec<PeerText>` field materialises a hostile list in full before
        // anything truncates it. Nothing observable distinguishes "decoded
        // then dropped" from "never decoded" except this: the elements past
        // the cap are skipped without being interpreted, so ones that are not
        // even strings cost nothing and do not fail the announcement.
        let mut models: Vec<serde_json::Value> = (0..MAX_ENTRIES)
            .map(|i| serde_json::json!(format!("model-{i}")))
            .collect();
        models.push(serde_json::json!(12_345));
        models.push(serde_json::json!({ "not": "a string" }));
        models.push(serde_json::json!([1, 2, 3]));

        let caps: Capability =
            serde_json::from_value(serde_json::json!({ "models": models })).expect("decode");
        assert_eq!(caps.models.len(), MAX_ENTRIES);
        assert_eq!(caps.models[MAX_ENTRIES - 1].as_str(), "model-63");

        // Inside the cap, a non-string is still a malformed record and still
        // fails: skipping is what happens past the bound, not a new lenience.
        assert!(
            serde_json::from_value::<Capability>(serde_json::json!({ "models": [12_345] }))
                .is_err()
        );
        // Duplicates cannot crowd out the cap either, or a peer would fill all
        // 64 slots with one name and hide everything real behind it.
        let flood: Vec<String> = std::iter::repeat_n("same".to_string(), MAX_ENTRIES * 4)
            .chain((0..4).map(|i| format!("real-{i}")))
            .collect();
        let caps: Capability =
            serde_json::from_value(serde_json::json!({ "tools": flood })).expect("decode");
        assert_eq!(
            caps.tools.iter().map(PeerText::as_str).collect::<Vec<_>>(),
            vec!["same", "real-0", "real-1", "real-2", "real-3"]
        );
    }
}
