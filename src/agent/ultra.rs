//! The council: fan out N candidates, adjudicate, hand the result to the one
//! thing that acts.
//!
//! `/ultra` and `/fusion` were the same algorithm written twice. Both fanned a
//! request out to several answerers, had something rule on the answers, and
//! injected the result as guidance for a single tool-capable actor. They
//! differed only in where a candidate came from (a subagent under a lens, or a
//! provider), in what adjudicated (a judge subagent, or the candidates
//! critiquing each other), and in which layer they lived at. Because each owned
//! its own fan-out, the two could not be stacked, and each refused to turn on
//! over the other, so "three lenses across two providers", the obvious thing
//! to want, was not a configuration but an unsupported combination.
//!
//! So there is one primitive here, and both commands are front ends onto it:
//!
//! ```text
//! Council { candidates: [Candidate { kind, seat }], adjudicator, timeout }
//!     confer(brief, bench?) -> CouncilResult { candidates, verdicts }
//! ```
//!
//! - A [`Candidate`]'s [`CandidateKind`] says where its answer comes from: a
//!   subagent under a lens prompt with read-only tools, or a bare completion.
//! - Its [`Seat`] says where it runs: the council's own client and model, or a
//!   named provider's. That is the whole of what a candidate is told about
//!   *where*, which is why a candidate that later runs on a mesh peer is a seat
//!   whose client speaks to that peer and not a fourth kind of fan-out.
//! - The [`Adjudicator`] says how disagreement is settled: judge subagents that
//!   rule head-to-head (`/ultra`), or the candidates themselves refining their
//!   answers over critique rounds (`/fusion`).
//! - The [`Bench`] is the agent the council is sitting inside, when it is
//!   inside one. Without it, a lens candidate degrades to a plain completion:
//!   it still answers, it just has nothing to read the repository with.
//!
//! `/ultra` is [`UltraEngine`] here: N lenses, judged, seated on the parent's
//! own client unless `/fusion` is also on, in which case the roster is dealt
//! across the panel's providers instead. `/fusion` is
//! [`FusionProvider`](crate::llm::fusion::FusionProvider): N providers,
//! debating, wrapped as an [`LlmProvider`] so the agent loop never learns about
//! it. Both names still work, and both mean what they always did.
//!
//! **Advisory, never fatal.** Every failure — a dead candidate, a step budget
//! hit, an empty draft, a timeout, an unreachable provider — degrades to the
//! ordinary single-agent turn. [`run`] therefore returns an [`UltraOutcome`]
//! and not a `Result`: a council must not be able to lose a turn that would
//! otherwise have worked.
//!
//! **Turn-scoped.** The guidance is N drafts and a verdict about *one* request,
//! so it lives exactly as long as the turn it was built for: the agent drops it
//! again on the way out ([`is_guidance`], [`GUIDANCE_HEADING`]) and never writes
//! it to the session. What the user keeps is the surface's copy —
//! `AgentEvent::UltraGuidance`, which the TUI folds into a collapsed transcript
//! card — because the candidates' rail panes retire within seconds of finishing
//! and a system message is never rendered anywhere.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use futures_util::StreamExt;
use futures_util::future::join_all;
use tokio::sync::mpsc;

use crate::config::{StepBudget, UltraConfig};
use crate::hooks::HookEngine;
use crate::llm::provider::LlmProvider;
use crate::llm::{ChatMessage, ChatRequest, ChatStream, Role};
use crate::tools::{ToolAccess, ToolContext, registry::ToolRegistry};

use super::subagent::{self, SpawnOptions, SubagentConfig, SubagentResult, SubagentStop};
use super::{AgentEvent, CancelHandle, breaker, cancelled, emit};

/// Lenses a fresh `[ultra]` runs. Deliberately three, not five: the pre-phase
/// is `lenses × candidate_max_steps` model calls before the main agent emits
/// its first token, and `edge-cases`/`architect` earn their keep only on gnarly
/// work — add them with `/ultra config`.
pub const DEFAULT_LENSES: &[&str] = &["implementer", "skeptic", "minimalist"];

/// Ceiling on candidates. Past this the pre-phase dominates the turn's cost and
/// latency, and the drafts start repeating each other.
pub const MAX_LENSES: usize = 6;

/// Ceiling on judges. Verdicts do not vote — the main agent decides — so past a
/// handful it is pure spend.
pub const MAX_JUDGES: u8 = 3;

/// Floor on the per-draft character cap. Below this a draft is not clipped, it
/// is destroyed.
pub const MIN_DRAFT_CHARS: usize = 500;

/// Name of the definition that compares the drafts. Resolves through the same
/// shadowing rule as a lens, so `~/.wizard/subagents/judge.toml` replaces it —
/// but it is not itself a lens, and never appears in the lens roster.
pub const JUDGE_NAME: &str = "judge";

/// Heading every injected guidance block opens with.
///
/// It is a sentinel, not decoration: guidance is advice about *one* request, so
/// the agent drops it again at the end of the turn it was injected for
/// ([`is_guidance`] is how it finds it). Left in, a turn's worth of drafts
/// (tens of KB) would ride in history forever, be re-sent on every later turn,
/// and — on Anthropic, where every `Role::System` message is hoisted into the
/// single top-level system prompt — describe a request that was answered three
/// turns ago as if it were the standing instruction.
pub const GUIDANCE_HEADING: &str = "[Ultra]";

/// Fraction of the model's context window the injected guidance may fill.
const GUIDANCE_WINDOW_FRACTION: usize = 15; // percent
/// Guidance budget when the provider reports no context window.
const GUIDANCE_FALLBACK_CHARS: usize = 24_000;
/// Hard bounds on the guidance budget, whatever the window says.
const GUIDANCE_MIN_CHARS: usize = 4_000;
const GUIDANCE_MAX_CHARS: usize = 40_000;
/// Rough chars-per-token, for turning a context window into a char budget.
const CHARS_PER_TOKEN: usize = 4;
/// Messages of conversation tail a candidate is given as context.
const CONTEXT_MESSAGES: usize = 8;
/// Per-tool-result cap inside that rendered tail.
const CONTEXT_TOOL_RESULT_CHARS: usize = 400;
/// Cap on one injected system note inside that tail. Roomier than a tool
/// result because the notes that land there are summaries of things the tail no
/// longer holds — above all the compaction summary, which is the session's only
/// record of everything it dropped.
const CONTEXT_NOTE_CHARS: usize = 2_000;

/// Left where a draft's middle was cut out. A fixed string, not a formatted
/// one: [`elide_middle`] budgets the head and the tail against its length, and
/// a marker whose length depended on how much it elided would make that
/// accounting circular.
const ELISION_MARKER: &str =
    "\n\n[... middle of this draft elided to fit the context window ...]\n\n";

/// Ultra's built-in lenses: the same request, five different postures toward it
/// (`implementer`, `skeptic`, `minimalist`, `edge-cases`, `architect`).
///
/// The `max_steps` and `tool_scope` fields here are placeholders —
/// [`UltraEngine::build`] overwrites both, because a lens contributes a posture
/// and nothing else. Every prompt states the read-only constraint itself:
/// [`subagent::spawn`] enforces it by stripping the tools, but a candidate that
/// does not know it cannot write spends its budget reaching for tools that are
/// not there.
pub fn builtin_lenses() -> Vec<SubagentConfig> {
    let lens = |name: &str, description: &str, posture: &str| SubagentConfig {
        name: name.to_string(),
        description: description.to_string(),
        system_prompt: format!(
            "You are one of several agents independently drafting an answer to the same request, \
             each under a different lens. Yours is: {posture}\n\n\
             You have read-only tools. Read the repository, check the claims you intend to make \
             against what is actually there, and cite the files and symbols you relied on — you \
             cannot write, run commands, or otherwise verify by execution, so anything you cannot \
             read is a claim you must mark as unverified rather than assert.\n\n\
             Another agent, with full tools, will carry out the work; you are advising it, not \
             doing it. Finish with your complete proposal: what to do, where, in what order, and \
             what could go wrong. Be concrete and specific to this repository — a plan that would \
             read the same for any codebase is worthless. Do not ask questions; state your \
             assumptions and proceed."
        ),
        tool_scope: None,
        max_steps: StepBudget::new(10),
    };
    vec![
        lens(
            "implementer",
            "Drafts the direct, complete implementation.",
            "propose the most direct implementation that actually solves the request, end to end, \
             with the concrete edits it needs.",
        ),
        lens(
            "skeptic",
            "Attacks the obvious approach and says what breaks.",
            "assume the obvious approach is wrong. Find what it breaks, what it misreads about \
             this codebase, and what the request is really asking for underneath, then propose \
             what to do instead.",
        ),
        lens(
            "minimalist",
            "Finds the smallest correct diff.",
            "find the smallest change that is genuinely correct. Prefer reusing what exists over \
             adding to it, and say plainly which parts of the obvious approach are unnecessary.",
        ),
        lens(
            "edge-cases",
            "Hunts the inputs and states the happy path misses.",
            "hunt the cases the happy path misses: empty and huge inputs, concurrency, \
             cancellation, failure and partial-failure paths, and the states this code can already \
             be in. Say how each should behave and where that has to be handled.",
        ),
        lens(
            "architect",
            "Weighs the change against the shape of the codebase.",
            "weigh the change against the shape this codebase already has. Say where it belongs, \
             which existing seam it should go through, and what it would cost later if it went in \
             the obvious place instead.",
        ),
    ]
}

/// The built-in judge: read-only, so that when two drafts disagree about the
/// repository it can go and check which one is right instead of splitting the
/// difference on the more confident prose.
pub fn builtin_judge() -> SubagentConfig {
    SubagentConfig {
        name: JUDGE_NAME.to_string(),
        description: "Compares the candidate drafts head-to-head and rules on them.".to_string(),
        system_prompt:
            "You are judging several drafts that other agents independently produced for the same \
             request. They could read this repository but not write to it or run anything, so a \
             draft can be confidently wrong: a line number that moved, a function that no longer \
             exists, a file that was never read.\n\n\
             You have the same read-only tools. Where two drafts disagree about the repository, go \
             and read it — settle the disagreement on the code, never on which draft sounds more \
             certain.\n\n\
             Rule head-to-head. Say which draft is best and why; for each draft, what it got right \
             and what it got wrong or could not have known; and then the merged best approach, \
             concretely, drawing the strongest parts of each. Be blunt about a draft that is \
             simply mistaken. Another agent, with full tools, will execute from your verdict — \
             write it for that reader."
                .to_string(),
        tool_scope: None,
        max_steps: StepBudget::new(6),
    }
}

/// Every definition `/ultra` can draw a lens from: [`builtin_lenses`] with
/// `~/.wizard/subagents/` (and the active harness bundle) layered over it by
/// name, reusing [`subagent::available_configs`]'s shadowing rule verbatim. So
/// a lens can be retuned or replaced with a TOML file, and any subagent the
/// user already wrote can serve as one. [`JUDGE_NAME`] is excluded — it has its
/// own row in `/ultra config`, not a lens row.
pub fn lens_catalog(user_dir: &Path) -> Vec<SubagentConfig> {
    let mut catalog = builtin_lenses();
    for config in subagent::available_configs(user_dir) {
        catalog.retain(|existing| existing.name != config.name);
        catalog.push(config);
    }
    catalog.retain(|config| config.name != JUDGE_NAME);
    catalog
}

/// The judge definition: the user's `judge.toml` if they wrote one, else
/// [`builtin_judge`]. Same shadowing rule as a lens, deliberately — retuning the
/// comparison is the second thing anyone will want to do after retuning a lens.
fn resolve_judge(user_dir: &Path) -> SubagentConfig {
    subagent::available_configs(user_dir)
        .into_iter()
        .find(|config| config.name == JUDGE_NAME)
        .unwrap_or_else(builtin_judge)
}

// ---------------------------------------------------------------------------
// The council
// ---------------------------------------------------------------------------

/// Where one candidate's answer comes from.
///
/// The council does not care which of these it is fanning out to, and that is
/// the point: a lens and a provider used to be two features that could not be
/// combined because each owned its own fan-out. Here they are two values of one
/// enum, so a roster mixing them is a roster.
#[derive(Debug, Clone)]
pub enum CandidateKind {
    /// A subagent under a lens prompt, running the read-only tools of the agent
    /// hosting the council: it reads the repository before it drafts, so
    /// candidates disagree about what the code *is* rather than about what it
    /// might be.
    ///
    /// Boxed because a [`SubagentConfig`] is most of a kilobyte of prompt and
    /// the other variant carries nothing.
    Lens(Box<SubagentConfig>),
    /// A bare completion: no tools, no history, one call. `/fusion`'s panel
    /// member, and what a [`CandidateKind::Lens`] degrades to when the council
    /// is not sitting inside an agent.
    Panel,
}

/// Which client and model one candidate runs on.
///
/// All-`None` means "the council's own", which is what pins an `/ultra` lens to
/// the model the parent is live on across a mid-session `/model`. A seat that
/// names a client is how one roster spreads across several providers.
///
/// This is also the seam the mesh lands on. A candidate that runs on a peer is
/// a seat whose client speaks to that peer, so nothing above this type has to
/// learn a second word for "somewhere else", and nothing below it has to learn
/// the first.
#[derive(Clone, Default)]
pub struct Seat {
    /// Provider name, for the pane label and the guidance header. `None` = the
    /// council's own, which needs no label because there is only one.
    pub provider: Option<String>,
    /// Client to run on. `None` = the council's own.
    pub client: Option<Arc<dyn LlmProvider>>,
    /// Model tag to request. `None` = the council's own.
    pub model: Option<String>,
}

impl std::fmt::Debug for Seat {
    /// A client is a live trait object with no useful `Debug`; what a reader of
    /// a `{:?}` wants is which provider and model the seat names.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Seat")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .finish()
    }
}

/// One candidate: a name, where its answer comes from, and where it runs.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// Shown on the rail pane, in the guidance header, and in the run log.
    pub name: String,
    pub kind: CandidateKind,
    pub seat: Seat,
}

/// How the council settles what its candidates disagreed about.
#[derive(Debug, Clone)]
pub enum Adjudicator {
    /// Nothing. The drafts reach the actor as they stand.
    ///
    /// Named `Nobody` and not `None` so that a `match` on an adjudicator never
    /// reads like a `match` on an [`Option`].
    Nobody,
    /// `count` read-only judges rule head-to-head on the drafts: which is best,
    /// what each got right and wrong, and the merged best approach. Verdicts do
    /// not vote (the actor decides), so more than one judge buys a second
    /// opinion and never a tie-break.
    Judges {
        config: Box<SubagentConfig>,
        count: u8,
    },
    /// The candidates themselves, over `rounds` critique rounds: each is shown
    /// its peers' latest answers and its own, and refines. Unlike
    /// [`Adjudicator::Judges`] this produces no separate verdict: it replaces
    /// the drafts with better ones.
    Debate { rounds: u32 },
}

/// What every candidate is asked about.
///
/// The front end renders `context`, because how much of a conversation matters
/// is a question about the conversation and not about the council: `/ultra`
/// hands over a bounded tail (a candidate needs to know what was discussed, not
/// to re-read a 50 KB grep through the parent's eyes) while `/fusion` flattens
/// the whole history including the request, which is why `request` may be
/// empty.
pub struct Brief {
    /// The conversation the candidates answer against, already rendered.
    pub context: String,
    /// This turn's request, verbatim. Empty when the front end folded it into
    /// `context`.
    pub request: String,
}

impl Brief {
    /// The brief as one flat query, for a candidate that gets a single user
    /// message and no structure.
    fn flatten(&self) -> String {
        match (self.context.is_empty(), self.request.is_empty()) {
            (_, true) => self.context.clone(),
            (true, false) => self.request.clone(),
            _ => format!("{}\n\nUser: {}", self.context, self.request),
        }
    }
}

/// One candidate's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    /// The candidate's name (a lens name, or a provider name).
    pub name: String,
    /// The provider the seat named, when it was not the council's own.
    pub seat: Option<String>,
    /// What it said.
    pub output: String,
    /// Model round trips it took. `1` for a candidate that is one completion.
    pub steps_used: u32,
    /// False when the run hit its step budget: the last message is still
    /// evidence, but it is a partial thought and is never presented as a
    /// finished answer.
    pub completed: bool,
}

/// What became of one candidate.
///
/// A failure is kept rather than dropped, because the two front ends need
/// opposite things from it and only they can know which: `/ultra` must not put
/// a non-answer in front of the model, while `/fusion` must still *name* a dead
/// member so the synthesis request keeps its shape on exactly the turns where
/// the panel is broken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateOutcome {
    Drafted(Draft),
    Failed {
        name: String,
        seat: Option<String>,
        why: String,
    },
}

impl CandidateOutcome {
    /// The candidate's name, whichever way it went.
    pub fn name(&self) -> &str {
        match self {
            Self::Drafted(draft) => &draft.name,
            Self::Failed { name, .. } => name,
        }
    }

    /// Its draft, when it produced one.
    pub fn draft(&self) -> Option<&Draft> {
        match self {
            Self::Drafted(draft) => Some(draft),
            Self::Failed { .. } => None,
        }
    }
}

/// What the council concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CouncilResult {
    /// One entry per candidate, in roster order, including the ones that died.
    pub candidates: Vec<CandidateOutcome>,
    /// The adjudicator's verdicts. Empty for [`Adjudicator::Nobody`], for
    /// [`Adjudicator::Debate`] (which refines the drafts in place rather than
    /// ruling on them), and whenever fewer than two candidates drafted: a lone
    /// draft judged against itself is a model call for a verdict the actor
    /// could have reached by reading the draft.
    pub verdicts: Vec<Draft>,
}

impl CouncilResult {
    /// The usable drafts, in roster order.
    pub fn drafts(&self) -> Vec<&Draft> {
        self.candidates
            .iter()
            .filter_map(CandidateOutcome::draft)
            .collect()
    }

    /// `name (why)` for every candidate that produced nothing, for the notice
    /// that explains why a fan-out the user paid for came back empty.
    pub fn failures(&self) -> Vec<String> {
        self.candidates
            .iter()
            .filter_map(|candidate| match candidate {
                CandidateOutcome::Failed { name, why, .. } => Some(format!("{name} ({why})")),
                CandidateOutcome::Drafted(_) => None,
            })
            .collect()
    }
}

/// The council sat, or the user interrupted it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CouncilOutcome {
    Concluded(CouncilResult),
    /// Every pane this sitting opened has already been closed out.
    Cancelled,
}

/// The agent a council is sitting inside.
///
/// `None` at [`Council::confer`] means it is not inside one (`/fusion` wraps a
/// bare provider and the agent loop never learns a council ran), and then a
/// [`CandidateKind::Lens`] candidate degrades to a [`CandidateKind::Panel`]
/// one.
pub struct Bench<'a> {
    /// The parent's full tool set. The council scopes it down itself (see
    /// [`candidate_registry`]); handing it pre-scoped would put the read-only
    /// invariant in the caller.
    pub registry: &'a ToolRegistry,
    /// The parent's lifecycle hooks, which apply to a candidate's tool calls
    /// exactly as they do to the parent's. Shared rather than borrowed bare
    /// because each candidate's run builds a dispatcher over them.
    pub hooks: &'a Arc<HookEngine>,
    /// The parent's tool context, with this turn's event channel already wired
    /// in: a candidate's pane hangs off it.
    pub ctx: &'a ToolContext,
    /// Where run-scoped events go, for the panes the council opens itself.
    pub events: &'a mpsc::Sender<AgentEvent>,
    /// The turn's interrupt. `None` for a council nobody is watching.
    pub cancel: Option<&'a CancelHandle>,
    /// The circuit breaker over the endpoint the parent turn is dialing.
    ///
    /// Shared with every candidate rather than one apiece, because a council
    /// is the worst case the breaker exists for: N sub-runs opening on the
    /// same provider at the same instant, each with its own retry ladder, is N
    /// × 7 requests spent independently rediscovering one outage. `None` gives
    /// each run a breaker of its own, which still bounds it.
    pub breaker: Option<&'a breaker::LlmBreaker>,
}

/// A record of every call a council made, for a front end that keeps one.
///
/// Sync and infallible on purpose. `/fusion`'s JSONL log is the only record a
/// fused turn leaves of its debate, and a logging failure must never lose a
/// turn, so an implementation swallows its own errors rather than handing the
/// council something it would have to decide what to do about.
pub trait CouncilJournal: Send + Sync {
    /// One call: which phase (`initial`, `review_1`, `verdict`), which
    /// candidate, which model actually answered, what it was asked, and what
    /// came back, with `Err` carrying the failure when nothing did.
    fn record(
        &self,
        phase: &str,
        candidate: &str,
        model: &str,
        prompt: &str,
        answer: Result<&str, &str>,
    );
}

/// Wall-clock cap on one candidate's call when the front end names none.
///
/// Not optional and not zero, for the reason `[ultra] timeout_secs` is not
/// either: without a deadline a throttled provider parks a candidate inside the
/// subagent retry ladder and the sitting hangs on a spinner.
pub const DEFAULT_COUNCIL_TIMEOUT: Duration = Duration::from_secs(300);

/// Fan out N candidates, adjudicate, hand back what they said.
///
/// Holds the client and model a seat that names none falls back to, so a
/// council built per turn from the agent's *live* client is what keeps
/// candidates on the model the user is actually using.
pub struct Council {
    /// The roster, in the order everything renders in.
    pub candidates: Vec<Candidate>,
    /// How disagreement is settled.
    pub adjudicator: Adjudicator,
    /// Wall-clock cap on one candidate's or one judge's call.
    pub timeout: Duration,
    /// Client a seat that names none runs on.
    client: Arc<dyn LlmProvider>,
    /// Model a seat that names none requests.
    model: String,
    /// Where every call is recorded, when the front end keeps a log.
    journal: Option<Arc<dyn CouncilJournal>>,
}

impl Council {
    /// A council with no candidates and no adjudicator, seated by default on
    /// `client`/`model`.
    pub fn new(client: Arc<dyn LlmProvider>, model: String) -> Self {
        Self {
            candidates: Vec::new(),
            adjudicator: Adjudicator::Nobody,
            timeout: DEFAULT_COUNCIL_TIMEOUT,
            client,
            model,
            journal: None,
        }
    }

    /// The roster.
    pub fn with_candidates(mut self, candidates: Vec<Candidate>) -> Self {
        self.candidates = candidates;
        self
    }

    /// How disagreement is settled.
    pub fn adjudicated_by(mut self, adjudicator: Adjudicator) -> Self {
        self.adjudicator = adjudicator;
        self
    }

    /// Wall-clock cap on one call.
    pub fn timed_out_after(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Where to record every call.
    pub fn journalled_by(mut self, journal: Arc<dyn CouncilJournal>) -> Self {
        self.journal = Some(journal);
        self
    }

    /// Sit: ask every candidate, adjudicate, hand back what they said.
    ///
    /// Candidates within a phase run concurrently and the phases are ordered,
    /// which is the only ordering anything downstream depends on: a review
    /// round has to see the initial answers, and a judge has to see the drafts.
    pub async fn confer<'b>(&self, brief: &Brief, bench: Option<&'b Bench<'b>>) -> CouncilOutcome {
        if self.candidates.is_empty() {
            return CouncilOutcome::Concluded(CouncilResult {
                candidates: Vec::new(),
                verdicts: Vec::new(),
            });
        }

        // The read-only tool set every lens investigates with, derived once for
        // the whole sitting rather than once per candidate per round.
        let sitting = Sitting {
            bench,
            tools: bench.map(|bench| candidate_registry(bench.registry)),
        };
        // The flattened conversation every bare-completion candidate sees. One
        // render, not one per candidate per round.
        let query = brief.flatten();

        // ---- Phase 1: every candidate answers, nobody has seen a peer -------
        let asked = join_all(self.candidates.iter().map(|candidate| {
            self.ask(
                candidate,
                "initial",
                self.opening_brief(candidate, brief, &query, &sitting),
                &sitting,
            )
        }))
        .await;
        let Some(mut candidates) = all_answered(asked) else {
            return CouncilOutcome::Cancelled;
        };

        // ---- Phase 2: adjudication ------------------------------------------
        let verdicts = match &self.adjudicator {
            Adjudicator::Nobody => Vec::new(),
            Adjudicator::Debate { rounds } => {
                for round in 1..=*rounds {
                    let phase = format!("review_{round}");
                    let refined = join_all(self.candidates.iter().enumerate().map(|(at, cand)| {
                        self.ask(
                            cand,
                            &phase,
                            review_prompt(
                                &query,
                                candidates[at].draft().map_or("", |d| d.output.as_str()),
                                &peers(&candidates, at),
                            ),
                            &sitting,
                        )
                    }))
                    .await;
                    let Some(refined) = all_answered(refined) else {
                        return CouncilOutcome::Cancelled;
                    };
                    // A review reply is a critique *and* an answer; only the
                    // answer is a draft, or the critique reaches the actor as
                    // if the candidate were recommending it.
                    candidates = refined.into_iter().map(refine).collect();
                }
                Vec::new()
            }
            Adjudicator::Judges { config, count } => {
                let drafts: Vec<&Draft> = candidates
                    .iter()
                    .filter_map(CandidateOutcome::draft)
                    .collect();
                if *count == 0 || drafts.len() < 2 {
                    Vec::new()
                } else {
                    let task = judge_brief(brief, &drafts);
                    let judges: Vec<Candidate> = (0..*count)
                        .map(|_| Candidate {
                            name: config.name.clone(),
                            kind: CandidateKind::Lens(config.clone()),
                            // A judge sits on the council's own seat: it reads
                            // drafts that already came from everywhere, and
                            // dealing it across providers would only make which
                            // model ruled depend on how many lenses there were.
                            seat: Seat::default(),
                        })
                        .collect();
                    let ruled = join_all(
                        judges
                            .iter()
                            .map(|judge| self.ask(judge, "verdict", task.clone(), &sitting)),
                    )
                    .await;
                    let Some(ruled) = all_answered(ruled) else {
                        return CouncilOutcome::Cancelled;
                    };
                    // A dead judge costs the sitting its verdict, not its
                    // drafts.
                    ruled
                        .into_iter()
                        .filter_map(|outcome| match outcome {
                            CandidateOutcome::Drafted(draft) => Some(draft),
                            CandidateOutcome::Failed { .. } => None,
                        })
                        .collect()
                }
            }
        };

        CouncilOutcome::Concluded(CouncilResult {
            candidates,
            verdicts,
        })
    }

    /// Ask one candidate one question.
    async fn ask(
        &self,
        candidate: &Candidate,
        phase: &str,
        brief: String,
        sitting: &Sitting<'_>,
    ) -> Asked {
        let client = candidate.seat.client.as_ref().unwrap_or(&self.client);
        let model = candidate
            .seat
            .model
            .as_deref()
            .unwrap_or(self.model.as_str());

        // A lens is a sub-loop only when there is an agent to run one in.
        // Everything else is one completion, which is also what a lens becomes
        // when the council is wrapping a bare provider: it still answers under
        // its posture, it just cannot check anything first.
        match (&candidate.kind, sitting.bench, &sitting.tools) {
            (CandidateKind::Lens(config), Some(bench), Some(tools)) => {
                self.ask_a_subagent(candidate, config, phase, brief, client, model, bench, tools)
                    .await
            }
            _ => {
                self.ask_for_a_completion(candidate, phase, brief, client, model, sitting)
                    .await
            }
        }
    }

    /// One lens (or judge) as a read-only subagent run, streaming into its own
    /// rail pane.
    #[allow(clippy::too_many_arguments)]
    async fn ask_a_subagent(
        &self,
        candidate: &Candidate,
        config: &SubagentConfig,
        phase: &str,
        brief: String,
        client: &Arc<dyn LlmProvider>,
        model: &str,
        bench: &Bench<'_>,
        tools: &ToolRegistry,
    ) -> Asked {
        let run = subagent::next_run_id();
        // The rail keys off `SubagentRunStarted`, which `spawn` does not emit
        // itself (it does not know the background id, when there is one). Every
        // event after it, including the terminal one on every failure path, is
        // spawn's.
        emit(
            bench.events,
            AgentEvent::SubagentRunStarted {
                run,
                // Not a background run: a council's candidates are the turn, and
                // there is no registry id for the surface to kill them by.
                bg: None,
                name: candidate.name.clone(),
                task: brief.clone(),
            },
        )
        .await;

        let options = SpawnOptions {
            model: Some(model.to_string()),
            read_only: true,
            // The interrupt and the deadline are spawn's to enforce, and its
            // pane is spawn's to close. This used to be a biased `select!`
            // wrapped around the call, which is how a caller ends up guessing
            // whether the pane it opened was already closed.
            cancel: bench.cancel.cloned(),
            deadline: Some(self.timeout),
            breaker: bench.breaker.cloned().unwrap_or_default(),
            ..Default::default()
        };

        match subagent::spawn(
            run,
            config,
            &brief,
            &options,
            client,
            tools,
            bench.hooks,
            bench.ctx,
        )
        .await
        {
            Ok(result) if draft_is_usable(&result) => {
                self.record(phase, candidate, model, &brief, Ok(result.output.as_str()));
                Asked::Answered(CandidateOutcome::Drafted(Draft {
                    name: result.name,
                    seat: candidate.seat.provider.clone(),
                    output: result.output,
                    steps_used: result.steps_used,
                    completed: result.completed,
                }))
            }
            Ok(result) => {
                self.record(phase, candidate, model, &brief, Ok(""));
                Asked::Answered(self.failed(candidate, "produced no final text", &result.name))
            }
            Err(err) => {
                if matches!(
                    err.downcast_ref::<SubagentStop>(),
                    Some(SubagentStop::Cancelled)
                ) {
                    return Asked::Cancelled;
                }
                let why = format!("{err:#}");
                self.record(phase, candidate, model, &brief, Err(why.as_str()));
                tracing::warn!("council candidate '{}' failed: {why}", candidate.name);
                Asked::Answered(self.failed(candidate, &why, &candidate.name))
            }
        }
    }

    /// One candidate as a single completion: no tools, no history, text out.
    async fn ask_for_a_completion(
        &self,
        candidate: &Candidate,
        phase: &str,
        brief: String,
        client: &Arc<dyn LlmProvider>,
        model: &str,
        sitting: &Sitting<'_>,
    ) -> Asked {
        let system = match &candidate.kind {
            CandidateKind::Lens(config) => config.system_prompt.clone(),
            CandidateKind::Panel => member_system_prompt(&candidate.name),
        };
        let request = ChatRequest {
            model: model.to_string(),
            messages: vec![
                ChatMessage::system(system),
                ChatMessage::user(brief.clone()),
            ],
            // Advisors advise: the actor downstream is the only thing that may
            // emit a tool call, which is what stops two models acting on one
            // turn.
            tools: Vec::new(),
            stream: true,
            options: None,
        };

        let cancel = sitting.bench.and_then(|bench| bench.cancel);
        let answered = tokio::select! {
            biased;
            () = cancelled(cancel) => return Asked::Cancelled,
            answered = tokio::time::timeout(self.timeout, async {
                match client.chat_stream(request).await {
                    Ok(stream) => collect_text(stream).await,
                    Err(err) => Err(err),
                }
            }) => answered,
        };

        match answered {
            Ok(Ok(text)) => {
                self.record(phase, candidate, model, &brief, Ok(text.as_str()));
                if text.trim().is_empty() {
                    return Asked::Answered(self.failed(
                        candidate,
                        "produced no final text",
                        &candidate.name,
                    ));
                }
                Asked::Answered(CandidateOutcome::Drafted(Draft {
                    name: candidate.name.clone(),
                    seat: candidate.seat.provider.clone(),
                    output: text,
                    steps_used: 1,
                    completed: true,
                }))
            }
            Ok(Err(err)) => {
                let why = format!("{err:#}");
                self.record(phase, candidate, model, &brief, Err(why.as_str()));
                tracing::warn!("council candidate '{}' failed: {why}", candidate.name);
                Asked::Answered(self.failed(candidate, &why, &candidate.name))
            }
            Err(_) => {
                let why = format!("timed out after {:?}", self.timeout);
                self.record(phase, candidate, model, &brief, Err(why.as_str()));
                Asked::Answered(self.failed(candidate, &why, &candidate.name))
            }
        }
    }

    /// The opening question, which depends on whether this candidate will
    /// actually have tools: telling a bare completion to "investigate this
    /// repository with your read-only tools" asks it to hallucinate.
    fn opening_brief(
        &self,
        candidate: &Candidate,
        brief: &Brief,
        query: &str,
        sitting: &Sitting<'_>,
    ) -> String {
        match (&candidate.kind, sitting.bench) {
            (CandidateKind::Lens(_), Some(_)) => lens_brief(brief),
            _ => initial_prompt(query),
        }
    }

    fn failed(&self, candidate: &Candidate, why: &str, name: &str) -> CandidateOutcome {
        CandidateOutcome::Failed {
            name: name.to_string(),
            seat: candidate.seat.provider.clone(),
            why: why.to_string(),
        }
    }

    fn record(
        &self,
        phase: &str,
        candidate: &Candidate,
        model: &str,
        prompt: &str,
        answer: Result<&str, &str>,
    ) {
        if let Some(journal) = &self.journal {
            journal.record(phase, &candidate.name, model, prompt, answer);
        }
    }
}

/// The bench plus the derivations a sitting would otherwise redo per candidate
/// per round.
struct Sitting<'a> {
    bench: Option<&'a Bench<'a>>,
    /// The read-only registry lenses investigate with; `None` without a bench.
    tools: Option<ToolRegistry>,
}

/// One candidate's outcome, plus the one thing that must not be folded into it.
///
/// A cancellation is not a failed candidate: it ends the whole sitting, and
/// treating it as one dead answer would let the remaining phases run on a turn
/// the user has already stopped.
enum Asked {
    Answered(CandidateOutcome),
    Cancelled,
}

/// Every outcome, or `None` if any of them was a cancellation.
///
/// Checks all of them rather than short-circuiting on the first: the futures
/// have already resolved, so there is nothing left to save, and a cancelled
/// sitting must not depend on roster order for what it reports.
fn all_answered(asked: Vec<Asked>) -> Option<Vec<CandidateOutcome>> {
    let mut outcomes = Vec::with_capacity(asked.len());
    let mut cancelled = false;
    for one in asked {
        match one {
            Asked::Answered(outcome) => outcomes.push(outcome),
            Asked::Cancelled => cancelled = true,
        }
    }
    if cancelled { None } else { Some(outcomes) }
}

/// Every *other* candidate's latest answer, for a critique round.
///
/// A candidate that failed contributes nothing rather than an empty block: a
/// bare `[name]` header with nothing under it reads as a peer who considered
/// the question and had nothing to say.
fn peers(candidates: &[CandidateOutcome], skip: usize) -> Vec<(String, String)> {
    candidates
        .iter()
        .enumerate()
        .filter(|(at, _)| *at != skip)
        .filter_map(|(_, candidate)| candidate.draft())
        .filter(|draft| !draft.output.trim().is_empty())
        .map(|draft| (draft.name.clone(), draft.output.clone()))
        .collect()
}

/// Keep only the refined answer out of a review reply.
fn refine(outcome: CandidateOutcome) -> CandidateOutcome {
    match outcome {
        CandidateOutcome::Drafted(mut draft) => {
            draft.output = extract_refined(&draft.output);
            CandidateOutcome::Drafted(draft)
        }
        failed => failed,
    }
}

/// Drain a [`ChatStream`] to the concatenated answer text, skipping `thinking`
/// (reasoning) deltas.
pub(crate) async fn collect_text(stream: ChatStream) -> Result<String> {
    collect_text_billed(stream, None).await
}

/// [`collect_text`], with the tokens the backend reported billed to `usage`.
///
/// Two entry points over one body rather than a `usage` argument at every call
/// site, because the council's own candidates are already billed by the
/// subagent loop underneath them and passing `None` sixty times would be a
/// worse lie than passing nothing.
///
/// The caller that needs the metered spelling is a Lua plugin's
/// `wizard.model.complete`: that call spends the user's money on the user's
/// key, and spend that reaches no tracker is spend `/cost` cannot show. It
/// bills through `record_delegated` for the same reason a subagent does — the
/// prompt was not this turn's prompt, so it must not drive compaction — and
/// through `record_cache` so a cached plugin prompt is not counted as fresh
/// input.
pub(crate) async fn collect_text_billed(
    mut stream: ChatStream,
    usage: Option<&crate::usage::UsageTracker>,
) -> Result<String> {
    let mut out = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if let Some(tracker) = usage {
            // Counts arrive on the final chunk only, but reading them on every
            // chunk costs two `Option` checks and survives a provider that
            // reports incrementally.
            let prompt = chunk.prompt_eval_count.unwrap_or(0);
            let completion = chunk.eval_count.unwrap_or(0);
            if prompt > 0 || completion > 0 {
                tracker.record_delegated(prompt, completion);
            }
            if chunk.cache.read > 0 || chunk.cache.write > 0 {
                tracker.record_cache(chunk.cache.read, chunk.cache.write);
            }
        }
        if chunk.thinking {
            continue;
        }
        if let Some(message) = chunk.message {
            out.push_str(&message.text());
        }
        if chunk.done {
            break;
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Debate prompts
// ---------------------------------------------------------------------------

/// System prompt for a bare-completion candidate.
///
/// Ported verbatim from the debate engine (which ported it from FUSION's
/// Python). The wording is the behaviour: reword it and the panel answers
/// differently, so the tests below pin it. The upstream template took an
/// optional role description and fell back to the agent's name when it was
/// unset; `/fusion` never set one (a panel member is a registered Wizard
/// provider, not a persona), so the fallback is inlined.
fn member_system_prompt(name: &str) -> String {
    format!(
        "You are {name}, specialized in {name}. \
Follow instructions carefully, avoid fabrications, and provide step-by-step, verifiable reasoning when asked."
    )
}

/// Opening prompt for a bare-completion candidate: answer the query without
/// having seen any peer. Verbatim.
fn initial_prompt(query: &str) -> String {
    format!("Task: Provide the best possible answer to the user's query.\n\nQuery: {query}")
}

/// Critique-round prompt: review the peers' latest answers and emit a refined
/// answer. Verbatim; `others` renders as `[Name]\nanswer` blocks in roster
/// order.
fn review_prompt(query: &str, self_response: &str, others: &[(String, String)]) -> String {
    let others_str = others
        .iter()
        .map(|(name, answer)| format!("[{name}]\n{answer}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    format!(
        "You will review responses from other agents and refine your own. \
Instructions:\n\
1) Identify factual errors, logical gaps, and unclear explanations in others' responses.\n\
2) Suggest concrete improvements and corrections.\n\
3) Produce your refined answer that integrates the best ideas and fixes flaws.\n\n\
Original Query:\n{query}\n\n\
Your Previous Answer:\n{self_response}\n\n\
Other Agents' Answers:\n{others_str}\n\n\
Output format:\n\
- Critique: <your short critique>\n\
- Refined Answer: <your improved answer>\n"
    )
}

/// Pull the refined-answer block out of a review-round reply, so the critique
/// that precedes it is not forwarded to the actor as if it were an answer.
///
/// The **last** header wins, so a real trailing `Refined Answer:` block beats an
/// in-critique mention of the phrase ("my refined answer: was weak"). With no
/// header at all the whole reply is the answer, trimmed: a model that ignored
/// the output format still said something worth synthesizing.
pub(crate) fn extract_refined(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut last_header_end: Option<usize> = None;
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        if let Some(end) = refined_header_end(line) {
            last_header_end = Some(offset + end);
        }
        offset += line.len();
    }
    match last_header_end {
        Some(end) => text[end..].trim().to_string(),
        None => text.trim().to_string(),
    }
}

/// Byte offset just past a `Refined Answer:` header opening `line`, or `None`
/// when the line does not open one.
///
/// Tolerates the shapes models actually emit around the header: a leading
/// bullet, markdown bold on either side of the colon, and any mix of spaces and
/// tabs. Anchored at the start of the line on purpose, so the phrase appearing
/// mid-sentence inside a critique is not mistaken for the header.
///
/// Hand-rolled rather than a regex because it is the only pattern in the
/// binary that would need one: the upstream engine's `regex` + `once_cell` pair
/// is not worth two dependencies and a lazily compiled DFA for a fixed
/// ASCII prefix.
fn refined_header_end(line: &str) -> Option<usize> {
    /// `[ \t]*`
    fn spaces(b: &[u8], mut i: usize) -> usize {
        while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
            i += 1;
        }
        i
    }
    /// `\**` (markdown bold/italic markers)
    fn stars(b: &[u8], mut i: usize) -> usize {
        while i < b.len() && b[i] == b'*' {
            i += 1;
        }
        i
    }

    const LABEL: &[u8] = b"refined answer";
    let b = line.as_bytes();

    let mut i = spaces(b, 0);
    // An optional list bullet, then any run of emphasis markers.
    if i < b.len() && (b[i] == b'-' || b[i] == b'*') {
        i += 1;
    }
    i = stars(b, spaces(b, i));
    i = spaces(b, i);

    let end = i + LABEL.len();
    if end > b.len() || !b[i..end].eq_ignore_ascii_case(LABEL) {
        return None;
    }
    i = stars(b, spaces(b, end));
    i = spaces(b, i);

    if i >= b.len() || b[i] != b':' {
        return None;
    }
    i = stars(b, spaces(b, i + 1));
    // Every byte stepped over above is ASCII, so `i` is a char boundary and the
    // caller can slice the transcript at it.
    Some(spaces(b, i))
}

// ---------------------------------------------------------------------------
// `/ultra`: the front end
// ---------------------------------------------------------------------------

/// A resolved, runnable ultra roster. Holds no client: the agent supplies its
/// own live client, model, registry, hooks, and context at run time, which is
/// what keeps candidates pinned to the *active* model across a mid-session
/// `/model`.
#[derive(Debug, Clone)]
pub struct UltraEngine {
    /// One candidate per lens, in configured order, with ultra's budgets and
    /// tool scope already applied.
    pub lenses: Vec<SubagentConfig>,
    /// The judge definition, cloned per judge when `judges > 1`.
    pub judge: SubagentConfig,
    /// How many judges to run; `0` skips the compare phase.
    pub judges: u8,
    /// Wall-clock cap on one candidate or one judge.
    pub timeout: Duration,
    /// Per-draft character cap inside the guidance.
    pub max_draft_chars: usize,
    /// Providers the lenses are dealt across, round-robin. Empty is the common
    /// case and means the parent's own client and model: nothing about
    /// `[ultra]` names a provider.
    ///
    /// Non-empty is what `/ultra` and `/fusion` being on together *is*: the
    /// surface fills this from the fusion panel (see
    /// [`crate::llm::fusion::panel_seats`]), so three lenses over a two-provider
    /// panel is three runs on two providers. Left empty while the active client
    /// is a fused one, every candidate would re-run the whole panel debate,
    /// which is the cost the two modes used to refuse each other over.
    pub seats: Vec<Seat>,
}

impl UltraEngine {
    /// Resolve `cfg` into a runnable engine. **The only validation gate for
    /// `[ultra]`:** an empty roster, a duplicate or unknown lens name, a count
    /// or budget out of range, and a zero timeout all fail here with the
    /// offending field named, rather than being silently clamped into something
    /// the user did not ask for. Ultra overrides each lens's `max_steps` and
    /// forces `tool_scope: None` — a lens contributes a prompt and a name,
    /// nothing else.
    pub fn build(cfg: &UltraConfig, user_dir: &Path) -> Result<Self> {
        if cfg.lenses.is_empty() {
            bail!("ultra: `lenses` is empty — ultra needs at least one candidate lens");
        }
        if cfg.lenses.len() > MAX_LENSES {
            bail!(
                "ultra: `lenses` has {} entries — at most {MAX_LENSES} are allowed (each is a \
                 full subagent run before the turn starts)",
                cfg.lenses.len()
            );
        }
        let mut seen = HashSet::new();
        for name in &cfg.lenses {
            if !seen.insert(name.as_str()) {
                bail!(
                    "ultra: `lenses` names '{name}' twice — the same prompt twice buys two \
                     near-identical drafts and two panes labeled the same thing"
                );
            }
        }
        if cfg.judges > MAX_JUDGES {
            bail!(
                "ultra: `judges` is {} — at most {MAX_JUDGES} are allowed (verdicts do not vote; \
                 the main agent decides)",
                cfg.judges
            );
        }
        if cfg.candidate_max_steps == 0 {
            bail!("ultra: `candidate_max_steps` is 0 — a candidate needs at least one step");
        }
        if cfg.judge_max_steps == 0 {
            bail!("ultra: `judge_max_steps` is 0 — a judge needs at least one step");
        }
        if cfg.timeout_secs == 0 {
            bail!(
                "ultra: `timeout_secs` is 0 — without a deadline a throttled provider parks a \
                 candidate in the retry ladder and the turn hangs on a spinner"
            );
        }
        if cfg.max_draft_chars < MIN_DRAFT_CHARS {
            bail!(
                "ultra: `max_draft_chars` is {} — below {MIN_DRAFT_CHARS} a draft is not clipped, \
                 it is destroyed",
                cfg.max_draft_chars
            );
        }

        let catalog = lens_catalog(user_dir);
        let mut lenses = Vec::with_capacity(cfg.lenses.len());
        for name in &cfg.lenses {
            let found = catalog
                .iter()
                .find(|config| &config.name == name)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "ultra: `lenses` names unknown lens '{name}'; available: {}",
                        catalog
                            .iter()
                            .map(|config| config.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })?;
            lenses.push(SubagentConfig {
                // A lens contributes a posture, never its own budget or tool
                // scope: ultra owns both, so a user TOML that happens to set
                // `max_steps = 99` cannot quietly make the pre-phase ten times
                // more expensive than the roster says it is.
                max_steps: StepBudget::new(cfg.candidate_max_steps),
                tool_scope: None,
                ..found.clone()
            });
        }

        let judge = SubagentConfig {
            max_steps: StepBudget::new(cfg.judge_max_steps),
            tool_scope: None,
            ..resolve_judge(user_dir)
        };

        Ok(Self {
            lenses,
            judge,
            judges: cfg.judges,
            timeout: Duration::from_secs(cfg.timeout_secs),
            max_draft_chars: cfg.max_draft_chars,
            seats: Vec::new(),
        })
    }

    /// Deal this roster across `seats` instead of across the parent's own
    /// client.
    ///
    /// Separate from [`UltraEngine::build`] because `[ultra]` names no
    /// provider and never will: which providers exist is a question about the
    /// *session*, and the answer changes when `/fusion` is toggled without a
    /// line of `[ultra]` changing.
    pub fn with_seats(mut self, seats: Vec<Seat>) -> Self {
        self.seats = seats;
        self
    }

    /// The council this roster describes, seated by default on the agent's own
    /// `client`/`model`.
    pub fn council(&self, client: &Arc<dyn LlmProvider>, model: &str) -> Council {
        let candidates = self
            .lenses
            .iter()
            .enumerate()
            .map(|(at, lens)| Candidate {
                name: lens.name.clone(),
                kind: CandidateKind::Lens(Box::new(lens.clone())),
                // Round-robin, so a roster longer than the seat list still
                // spreads evenly instead of piling its tail onto the last
                // provider.
                seat: match self.seats.is_empty() {
                    true => Seat::default(),
                    false => self.seats[at % self.seats.len()].clone(),
                },
            })
            .collect();
        Council::new(Arc::clone(client), model.to_string())
            .with_candidates(candidates)
            .adjudicated_by(Adjudicator::Judges {
                config: Box::new(self.judge.clone()),
                count: self.judges,
            })
            .timed_out_after(self.timeout)
    }

    /// Number of candidates — which *is* `lenses.len()`, by construction. The
    /// `ULTRA ×N` badge reads this, so it cannot lie.
    pub fn candidates(&self) -> usize {
        self.lenses.len()
    }

    /// Status/notice label, e.g.
    /// `"ultra ×3 · implementer+skeptic+minimalist · 1 judge"`, with
    /// `· across claude+openrouter` appended when the roster is seated. Shared
    /// by the toggle notice, the `/ultra config` confirmation, and `/status`.
    /// The cost of this mode is the one thing the user must always have been
    /// told, and *where* it is being spent is now part of that.
    pub fn label(&self) -> String {
        let roster = self
            .lenses
            .iter()
            .map(|lens| lens.name.as_str())
            .collect::<Vec<_>>()
            .join("+");
        let judges = match self.judges {
            0 => "no judge".to_string(),
            1 => "1 judge".to_string(),
            n => format!("{n} judges"),
        };
        let seats = match self.seats.is_empty() {
            true => String::new(),
            false => format!(
                " \u{00b7} across {}",
                self.seats
                    .iter()
                    .map(|seat| seat.provider.as_deref().unwrap_or("?"))
                    .collect::<Vec<_>>()
                    .join("+")
            ),
        };
        format!(
            "ultra \u{00d7}{} \u{00b7} {roster} \u{00b7} {judges}{seats}",
            self.candidates()
        )
    }
}

/// What the ultra pre-phase leaves behind for the turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UltraOutcome {
    /// The system message to inject before the main loop runs.
    Guidance(String),
    /// Ultra produced nothing usable — every candidate failed, timed out, or
    /// returned no final text. The turn runs as an ordinary one; the string is
    /// the reason, surfaced as a notice.
    Skipped(String),
    /// The user interrupted during the pre-phase. The turn ends as
    /// [`DoneReason::Stopped`](super::DoneReason::Stopped); every pane this
    /// phase opened has already been closed out.
    Cancelled,
}

/// Run the pre-phase for one turn: sit the council, then render the guidance
/// the main agent executes from.
///
/// `request` is this turn's user message and `context` the conversation as it
/// stood *before* it — a follow-up like "now do the same for the other file" is
/// meaningless without it, and a candidate sees no message history of its own.
///
/// Each candidate streams into its own rail pane. This function opens no panes
/// and closes none: [`Council::confer`] emits `SubagentRunStarted` and
/// [`subagent::spawn`] emits everything after it, terminal event included, on
/// every path. That used to be split across three places, and the seam was
/// where a duplicate `SubagentRunDone` came from (which flips a pane from
/// `Done` to `Failed`).
///
/// `ctx` is the agent's own context and carries no event channel (an `Agent` is
/// built with `events: None`; the dispatcher injects the turn's channel per
/// call). Wiring `events` into it is therefore this function's job, not the
/// caller's — [`subagent::spawn`] streams a run's progress to `ctx.events`, so a
/// context handed down bare would open every pane and never write a line to one
/// or close it.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    engine: &UltraEngine,
    request: &str,
    context: &[ChatMessage],
    client: &Arc<dyn LlmProvider>,
    model: &str,
    registry: &ToolRegistry,
    hooks: &Arc<HookEngine>,
    ctx: &ToolContext,
    cancel: &CancelHandle,
    breaker: &breaker::LlmBreaker,
    events: &mpsc::Sender<AgentEvent>,
) -> UltraOutcome {
    // `build` rejects an empty roster, but the engine's fields are public and a
    // pre-phase with nothing to fan out is an ordinary turn, not an error.
    if engine.lenses.is_empty() {
        return UltraOutcome::Skipped("no candidate lenses configured".to_string());
    }

    let ctx = ctx.with_events(events.clone());
    let bench = Bench {
        registry,
        hooks,
        ctx: &ctx,
        events,
        cancel: Some(cancel),
        breaker: Some(breaker),
    };
    let brief = Brief {
        context: render_context(context),
        request: request.to_string(),
    };

    let council = engine.council(client, model);
    let result = match council.confer(&brief, Some(&bench)).await {
        CouncilOutcome::Cancelled => return UltraOutcome::Cancelled,
        CouncilOutcome::Concluded(result) => result,
    };

    // A dead candidate contributes nothing here, unlike in `/fusion`: the
    // guidance goes in front of the model, and naming an agent that said
    // nothing invites it to wonder what the missing draft would have said.
    let drafts = result.drafts();
    if drafts.is_empty() {
        return UltraOutcome::Skipped(format!(
            "no candidate produced a usable draft — {}; running an ordinary turn",
            result.failures().join("; ")
        ));
    }
    let verdicts: Vec<&Draft> = result.verdicts.iter().collect();

    let budget = guidance_budget(client.context_window(model).await);
    UltraOutcome::Guidance(build_ultra_guidance(
        &drafts,
        &verdicts,
        budget,
        engine.max_draft_chars,
    ))
}

/// The tool set a candidate or judge gets.
///
/// **Safety is not this function's job** — `SpawnOptions { read_only: true }` is
/// what holds the no-write, no-recurse invariant: [`Tool::access`] defaults to
/// [`ToolAccess::Execute`], so [`subagent::read_only_registry`] already strips
/// `spawn_subagent`, `run_command`, `exit_plan`, `execute`, `write_file`,
/// `edit_file`, and every MCP/scripted tool, and [`subagent::spawn`]
/// additionally forces `command_dispatch: CommandDispatch::None` and a fresh
/// todo list into the child context.
///
/// This is **step-budget hygiene**: `interview` and `todo` are classed
/// `ReadOnly` and therefore survive that filter, and both are pure waste in a
/// candidate — `interview` has no surface to ask (it returns "No interactive
/// user is available to answer") and `todo` writes to the throwaway list spawn
/// hands it. Across N candidates, a burnt step is a real cost.
///
/// [`Tool::access`]: crate::tools::Tool::access
pub fn candidate_registry(parent: &ToolRegistry) -> ToolRegistry {
    let wasted = [
        crate::tools::interview::INTERVIEW_TOOL_NAME,
        crate::tools::todo::TODO_TOOL_NAME,
        // Compact mutates the *parent* agent history via the main loop
        // intercept; a candidate's registry only hits CompactTool::execute,
        // which errors. Strip it so candidates don't burn a step.
        crate::tools::compact::COMPACT_TOOL_NAME,
    ];
    let mut registry = ToolRegistry::new();
    for spec in parent.specs() {
        let name = spec.function.name.as_str();
        if wasted.contains(&name) {
            continue;
        }
        if let Some(tool) = parent.get(name)
            && tool.access() == ToolAccess::ReadOnly
        {
            registry.register(Arc::clone(tool));
        }
    }
    registry
}

// ── private ────────────────────────────────────────────────────────────────

/// A draft is usable when the subagent actually said something: non-empty, and
/// not [`subagent::NO_FINAL_TEXT`] (a run that only ever called tools).
fn draft_is_usable(result: &SubagentResult) -> bool {
    let output = result.output.trim();
    !output.is_empty() && output != subagent::NO_FINAL_TEXT
}

/// The self-contained brief a lens candidate gets: the bounded conversation
/// tail plus this turn's request. A subagent sees nothing else: no parent
/// history, no parent system prompt.
fn lens_brief(brief: &Brief) -> String {
    let mut task = String::new();
    if !brief.context.is_empty() {
        task.push_str("The conversation so far, for context:\n\n");
        task.push_str(&brief.context);
        task.push_str("\n\n");
    }
    task.push_str("The user's request for this turn:\n\n");
    task.push_str(&brief.request);
    task.push_str(
        "\n\nInvestigate this repository with your read-only tools and draft your full proposed \
         answer to that request, under your lens.",
    );
    task
}

/// The judge's brief: the request plus every usable draft, verbatim and
/// unclipped — the judge is the one reader that needs the whole thing.
fn judge_brief(brief: &Brief, drafts: &[&Draft]) -> String {
    let mut task = String::new();
    if !brief.context.is_empty() {
        task.push_str("The conversation so far, for context:\n\n");
        task.push_str(&brief.context);
        task.push_str("\n\n");
    }
    task.push_str("The user's request for this turn:\n\n");
    task.push_str(&brief.request);
    task.push_str("\n\nThe drafts to compare:\n\n");
    for draft in drafts {
        task.push_str(&draft_header(draft));
        task.push('\n');
        task.push_str(&draft.output);
        task.push_str("\n\n");
    }
    task.push_str(
        "Rule on them: which is best and why, what each got right and wrong, and the merged best \
         approach.",
    );
    task
}

/// The bounded conversation tail both briefs open with: the last
/// [`CONTEXT_MESSAGES`] messages, with tool results clipped — a candidate needs
/// to know what was already discussed, not to re-read a 50 KB grep through the
/// parent's eyes.
///
/// `context[0]` — and only it — is dropped: it is the parent's system prompt,
/// which describes tools and a personality the candidate does not have. Every
/// *other* `Role::System` message is an injected note (a compaction summary, a
/// background task's result, a subagent's report), and those are conversation.
/// The compaction summary in particular is the session's only record of
/// everything older than the tail, so when it has already fallen outside the
/// window it is pulled back in — a compacted session is exactly the one where a
/// follow-up like "now do the same for the other file" cannot be resolved from
/// the tail alone.
fn render_context(context: &[ChatMessage]) -> String {
    // The system prompt is at index 0 by construction (`refresh_system_prompt`
    // keeps it there); a context that does not start with one is simply short.
    let body = match context.first() {
        Some(first) if first.role == Role::System => &context[1..],
        _ => context,
    };
    let start = body.len().saturating_sub(CONTEXT_MESSAGES);

    let mut parts = Vec::new();
    if let Some(summary) = body[..start]
        .iter()
        .rev()
        .find(|message| is_compaction_summary(message))
    {
        parts.push(render_note(summary));
    }
    for message in &body[start..] {
        match message.role {
            Role::System => parts.push(render_note(message)),
            Role::User => parts.push(format!("User: {}", message.text())),
            Role::Assistant if !message.text().trim().is_empty() => {
                parts.push(format!("Assistant: {}", message.text()))
            }
            Role::Assistant => {}
            Role::Tool => parts.push(format!(
                "[tool {} result] {}",
                message.tool_name().unwrap_or("?"),
                elide_middle(&message.text(), CONTEXT_TOOL_RESULT_CHARS)
            )),
        }
    }
    parts.join("\n\n")
}

/// Whether `message` is the note [`Agent::compact_now`] leaves behind when it
/// summarizes the middle of a long history.
///
/// [`Agent::compact_now`]: super::Agent::compact_now
fn is_compaction_summary(message: &ChatMessage) -> bool {
    message.role == Role::System && message.text().starts_with(super::COMPACT_SUMMARY_HEADING)
}

/// One injected system note, rendered for a candidate. Clipped: a compaction
/// summary or a subagent report can be long, and the tail around it has to
/// survive in the brief too.
fn render_note(message: &ChatMessage) -> String {
    let body = elide_middle(&message.text(), CONTEXT_NOTE_CHARS);
    if is_compaction_summary(message) {
        format!("[earlier in this session, summarized]\n{body}")
    } else {
        format!("[note to the agent]\n{body}")
    }
}

/// Whether `message` is a guidance block this module injected into a turn.
/// The agent uses it to drop the previous turn's guidance (see
/// [`GUIDANCE_HEADING`]); nothing else should match it, since the heading opens
/// a system message that only [`build_ultra_guidance`] writes.
pub fn is_guidance(message: &ChatMessage) -> bool {
    message.role == Role::System && message.text().starts_with(GUIDANCE_HEADING)
}

/// How one draft is introduced, wherever it is rendered. An incomplete draft is
/// kept — its last message is still evidence — but never presented as a finished
/// answer: a plan that ran out of budget half way through is a partial thought,
/// and both the judge and the main agent have to weigh it as one.
///
/// The seat is named only when there is one to name. A roster on the parent's
/// own client is the common case and every header would carry the same
/// provider, which is noise; a roster dealt across providers is exactly where
/// "which model said this" is the reader's next question.
fn draft_header(draft: &Draft) -> String {
    let via = match &draft.seat {
        Some(provider) => format!(" via {provider}"),
        None => String::new(),
    };
    if draft.completed {
        format!(
            "[lens '{}'{via} — {} step(s)]",
            draft.name, draft.steps_used
        )
    } else {
        format!(
            "[lens '{}'{via} — incomplete, hit its {}-step budget]",
            draft.name, draft.steps_used
        )
    }
}

/// Total characters the guidance may occupy, from the model's context window
/// when it reports one. N drafts of unbounded length is the obvious way to blow
/// the window on the very turn ultra was supposed to help.
fn guidance_budget(window: Option<u32>) -> usize {
    let chars = match window {
        Some(window) => {
            (window as usize)
                .saturating_mul(CHARS_PER_TOKEN)
                .saturating_mul(GUIDANCE_WINDOW_FRACTION)
                / 100
        }
        None => GUIDANCE_FALLBACK_CHARS,
    };
    chars.clamp(GUIDANCE_MIN_CHARS, GUIDANCE_MAX_CHARS)
}

/// Keep the head and the tail, elide the middle with an explicit marker, on char
/// boundaries. Local rather than [`crate::tools::truncate_output`]: that marker
/// tells the reader to re-run a narrower command, which is nonsense inside a
/// draft — and a draft ends in its conclusion, so head-only truncation throws
/// away the part worth reading.
///
/// The result never exceeds `max_chars` bytes, which is what lets
/// [`build_ultra_guidance`] hold its own budget by construction.
fn elide_middle(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        return text.to_string();
    }
    if max_chars <= ELISION_MARKER.len() {
        // No room for a head, a tail, and the marker between them; a clipped
        // head is all that fits, and it still beats an empty draft.
        return text[..floor_boundary(text, max_chars)].to_string();
    }
    let keep = max_chars - ELISION_MARKER.len();
    let head = floor_boundary(text, keep / 2);
    let tail = ceil_boundary(text, text.len() - (keep - keep / 2));
    format!("{}{ELISION_MARKER}{}", &text[..head], &text[tail..])
}

/// Largest char boundary at or below `index` (clamped to the string's end).
fn floor_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Smallest char boundary at or above `index`. Moving *forward* is what keeps
/// the tail no longer than it was budgeted for.
fn ceil_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

/// The system message injected into the turn. The council's two front ends
/// render their guidance differently, which is why this is not shared with
/// [`crate::llm::fusion`]'s: the actor there is a model that was told nothing
/// about tools, while the actor here has them, so this one names the *agents*,
/// says they had real tools and could not write, marks the drafts that ran out
/// of budget, and puts the verdict last so it reads as the standing
/// recommendation.
/// The register is load-bearing: these are drafts from agents that could not
/// verify their own claims, so they are framed as evidence to check, never as
/// instructions to follow.
///
/// The rendered message stays within `budget` bytes: the preamble and the
/// section headers are counted first, and what is left is split evenly across
/// the drafts and verdicts (each also capped at `max_draft_chars`), with any
/// oversized item elided in the middle.
fn build_ultra_guidance(
    drafts: &[&Draft],
    verdicts: &[&Draft],
    budget: usize,
    max_draft_chars: usize,
) -> String {
    let preamble = format!(
        "{GUIDANCE_HEADING} {} agent(s) independently investigated this request, each under a \
         different lens. They had read-only tools — they could read this repository \
         but could not write to it, run anything, or verify a claim by executing it. Nothing they \
         describe has been applied. You are the only agent in this session that may act.\n\n\
         Treat every draft below as evidence to check, not as instructions to follow: a draft can \
         be confidently wrong about a line number, a path, or a function that no longer exists. \
         Verify what you rely on, keep what survives, discard the rest, and then carry out the \
         user's request yourself with your own tools.\n\n",
        drafts.len()
    );
    const DRAFTS_HEADING: &str = "Candidate drafts:\n\n";
    const VERDICTS_HEADING: &str = "Judge verdict(s), from agents that read the drafts and could re-read the repository to \
         settle their disagreements:\n\n";

    let draft_headers: Vec<String> = drafts.iter().map(|draft| draft_header(draft)).collect();
    let verdict_headers: Vec<String> = verdicts
        .iter()
        .map(|verdict| format!("[{}]", verdict.name))
        .collect();

    // Everything that is not a body: what is left over is the bodies' to share.
    let overhead = preamble.len()
        + DRAFTS_HEADING.len()
        + if verdicts.is_empty() {
            0
        } else {
            VERDICTS_HEADING.len()
        }
        + draft_headers
            .iter()
            .chain(verdict_headers.iter())
            // Each header is followed by a newline and each body by a blank
            // line: two newlines per section.
            .map(|header| header.len() + 2)
            .sum::<usize>();
    let items = drafts.len() + verdicts.len();
    let per_body = (budget.saturating_sub(overhead) / items.max(1)).min(max_draft_chars);

    let mut out = preamble;
    out.push_str(DRAFTS_HEADING);
    for (draft, header) in drafts.iter().zip(&draft_headers) {
        out.push_str(header);
        out.push('\n');
        out.push_str(&elide_middle(&draft.output, per_body));
        out.push('\n');
    }
    if !verdicts.is_empty() {
        out.push_str(VERDICTS_HEADING);
        for (verdict, header) in verdicts.iter().zip(&verdict_headers) {
            out.push_str(header);
            out.push('\n');
            out.push_str(&elide_middle(&verdict.output, per_body));
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use futures_util::stream;
    use serde_json::{Value, json};

    use super::*;
    use crate::llm::{CacheTokens, ChatChunk, ChatRequest, ChatStream, ToolCall};
    use crate::tools::{Tool, ToolError, ToolOutput};

    /// Temp dir removed on drop (mirrors the subagent tests').
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("wizard-ultra-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Lens names the stub recognizes, marked into each test config's system
    /// prompt by [`lens`]. A lens outside this list drafts as `"unknown"`.
    const LENS_NAMES: &[&str] = &["implementer", "skeptic", "minimalist", JUDGE_NAME];

    /// The draft a lens produces, unique per lens so a guidance or brief
    /// assertion can look for it verbatim.
    fn draft_text(lens: &str) -> String {
        format!("draft from {lens}")
    }

    /// Provider that answers per *lens*, keyed on the system prompt rather than
    /// on a queue of canned responses: ultra fans its candidates out
    /// concurrently, so a queue would hand them out in whatever order the
    /// executor happened to poll and nothing about a test would be
    /// deterministic.
    struct LensProvider {
        /// Every request served, in arrival order.
        seen: Mutex<Vec<ChatRequest>>,
        /// Lenses whose every call fails permanently. A 401 is never retried,
        /// so the run dies at once instead of sleeping through the ladder.
        fail: HashSet<String>,
        /// Lenses that never answer, to be killed by the deadline.
        stall: HashSet<String>,
        /// Lenses that only ever call a tool: the run ends with no final text
        /// and `spawn` returns [`subagent::NO_FINAL_TEXT`].
        empty: HashSet<String>,
        /// Lenses that speak *and* call a tool every step: the run always has
        /// more to do, so it ends on its step budget with a last message that
        /// is still worth reading.
        chatty: HashSet<String>,
        /// Padding added to every draft, in bytes — for the budget tests.
        bulk: usize,
        /// What `context_window` reports.
        window: Option<u32>,
        /// Fired once the named lens's request has been served: lets a test
        /// cancel *after* a candidate has finished rather than before any ran.
        cancel_on: Option<(String, CancelHandle)>,
        /// Lenses that try to *write* on their first step, by calling the
        /// parent's Edit tool. What comes back is the whole point of scoping
        /// a candidate's registry.
        reaching: HashSet<String>,
    }

    impl LensProvider {
        fn new() -> Self {
            Self {
                seen: Mutex::new(Vec::new()),
                fail: HashSet::new(),
                stall: HashSet::new(),
                empty: HashSet::new(),
                chatty: HashSet::new(),
                bulk: 0,
                window: None,
                cancel_on: None,
                reaching: HashSet::new(),
            }
        }

        fn reaching(mut self, lenses: &[&str]) -> Self {
            self.reaching = lenses.iter().map(|l| (*l).to_string()).collect();
            self
        }

        fn failing(mut self, lenses: &[&str]) -> Self {
            self.fail = lenses.iter().map(|l| (*l).to_string()).collect();
            self
        }

        fn stalling(mut self, lenses: &[&str]) -> Self {
            self.stall = lenses.iter().map(|l| (*l).to_string()).collect();
            self
        }

        fn empty(mut self, lenses: &[&str]) -> Self {
            self.empty = lenses.iter().map(|l| (*l).to_string()).collect();
            self
        }

        fn chatty(mut self, lenses: &[&str]) -> Self {
            self.chatty = lenses.iter().map(|l| (*l).to_string()).collect();
            self
        }

        fn bulky(mut self, chars: usize) -> Self {
            self.bulk = chars;
            self
        }

        fn window(mut self, window: Option<u32>) -> Self {
            self.window = window;
            self
        }

        fn cancelling_after(mut self, lens: &str, cancel: &CancelHandle) -> Self {
            self.cancel_on = Some((lens.to_string(), cancel.clone()));
            self
        }

        /// Which lens a request belongs to: the system prompt is the only thing
        /// that tells two otherwise identical concurrent runs apart.
        fn lens_of(&self, request: &ChatRequest) -> String {
            let system = request
                .messages
                .iter()
                .find(|message| matches!(message.role, Role::System))
                .map(|message| message.text())
                .unwrap_or_default();
            LENS_NAMES
                .iter()
                .find(|name| system.contains(&format!("lens-marker:{name}")))
                .map(|name| (*name).to_string())
                .unwrap_or_else(|| "unknown".to_string())
        }

        fn requests_for(&self, lens: &str) -> Vec<ChatRequest> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|request| self.lens_of(request) == lens)
                .cloned()
                .collect()
        }
    }

    #[async_trait]
    impl LlmProvider for LensProvider {
        async fn health(&self) -> Result<()> {
            Ok(())
        }

        async fn supports_native_tools(&self, _model: &str) -> Result<bool> {
            Ok(true)
        }

        async fn list_models(&self) -> Result<Vec<String>> {
            Ok(Vec::new())
        }

        async fn chat_stream(&self, request: ChatRequest) -> Result<ChatStream> {
            let lens = self.lens_of(&request);
            let first = self.requests_for(&lens).is_empty();
            self.seen.lock().unwrap().push(request);

            if self.fail.contains(&lens) {
                return Err(crate::llm::ProviderError::http(401, "scripted failure").into());
            }
            if self.stall.contains(&lens) {
                // Longer than any test's deadline: a stalled run must die on the
                // timeout, never on the provider relenting.
                tokio::time::sleep(Duration::from_secs(3_600)).await;
            }

            let probe = || ToolCall::new("probe".to_string(), json!({}));
            let mut text = String::new();
            let mut tool_calls = Vec::new();
            if first && self.reaching.contains(&lens) {
                tool_calls.push(ToolCall::new("mutate".to_string(), json!({})));
            } else if self.empty.contains(&lens) {
                tool_calls.push(probe());
            } else {
                text = draft_text(&lens);
                if self.bulk > 0 {
                    text.push('\n');
                    text.push_str(&"x".repeat(self.bulk));
                    text.push_str(&format!("\nconclusion of {lens}"));
                }
                if self.chatty.contains(&lens) {
                    tool_calls.push(probe());
                }
            }

            if let Some((on, cancel)) = &self.cancel_on
                && on == &lens
            {
                cancel.cancel();
            }

            let chunk = ChatChunk {
                message: Some(ChatMessage::assistant_turn(text, Vec::new(), tool_calls)),
                images: Vec::new(),
                thinking: false,
                done: true,
                done_reason: Some("stop".to_string()),
                eval_count: None,
                prompt_eval_count: None,
                cache: CacheTokens::NONE,
            };
            Ok(Box::pin(stream::iter(vec![Ok(chunk)])))
        }

        async fn context_window(&self, _model: &str) -> Option<u32> {
            self.window
        }

        fn label(&self) -> String {
            "lens-stub".to_string()
        }
    }

    /// Minimal tool with a configurable access class (mirrors the subagent
    /// tests' `FakeTool`).
    struct FakeTool {
        name: &'static str,
        access: ToolAccess,
    }

    #[async_trait]
    impl Tool for FakeTool {
        fn name(&self) -> &str {
            self.name
        }

        fn description(&self) -> &str {
            "fake tool for ultra tests"
        }

        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }

        fn access(&self) -> ToolAccess {
            self.access
        }

        async fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::ok("ok"))
        }
    }

    /// A parent registry with one tool of every access class, plus the two
    /// ReadOnly-but-useless tools [`candidate_registry`] drops.
    fn parent_registry() -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        for (name, access) in [
            ("probe", ToolAccess::ReadOnly),
            ("mutate", ToolAccess::Edit),
            ("run", ToolAccess::Execute),
            (
                crate::tools::interview::INTERVIEW_TOOL_NAME,
                ToolAccess::ReadOnly,
            ),
            (crate::tools::todo::TODO_TOOL_NAME, ToolAccess::ReadOnly),
        ] {
            registry.register(Arc::new(FakeTool { name, access }));
        }
        registry
    }

    /// A lens the stub recognizes, with a budget of `max_steps`.
    fn lens(name: &str, max_steps: u32) -> SubagentConfig {
        SubagentConfig {
            name: name.to_string(),
            description: format!("{name} lens"),
            system_prompt: format!("lens-marker:{name}"),
            tool_scope: None,
            max_steps: StepBudget::new(max_steps),
        }
    }

    /// An engine over `names`. Built by hand rather than through
    /// [`UltraEngine::build`] so a test can hold a deadline in milliseconds,
    /// which `[ultra]` (whole seconds) has no way to express.
    fn engine(names: &[&str], judges: u8) -> UltraEngine {
        UltraEngine {
            lenses: names.iter().map(|name| lens(name, 2)).collect(),
            judge: lens(JUDGE_NAME, 2),
            judges,
            timeout: Duration::from_secs(30),
            max_draft_chars: 6_000,
            seats: Vec::new(),
        }
    }

    /// Everything [`run`] needs besides the engine, so a test states only what
    /// it varies.
    struct Harness {
        provider: Arc<LensProvider>,
        registry: ToolRegistry,
        hooks: Arc<HookEngine>,
        ctx: ToolContext,
        cancel: CancelHandle,
        /// The breaker the turn's candidates share, so a test can assert one
        /// outage is not rediscovered N times.
        breaker: breaker::LlmBreaker,
        events: mpsc::Sender<AgentEvent>,
        drain: mpsc::Receiver<AgentEvent>,
        _tmp: TempDir,
    }

    impl Harness {
        fn new(provider: LensProvider) -> Self {
            Self::with_cancel(CancelHandle::default(), provider)
        }

        /// A harness whose provider already holds the cancel handle — the only
        /// way to fire cancellation from inside a run.
        fn with_cancel(cancel: CancelHandle, provider: LensProvider) -> Self {
            let tmp = TempDir::new();
            let (events, drain) = mpsc::channel(256);
            Self {
                provider: Arc::new(provider),
                registry: parent_registry(),
                hooks: Arc::new(HookEngine::new(
                    Vec::new(),
                    tmp.0.clone(),
                    "test".to_string(),
                )),
                ctx: ToolContext::new(&tmp.0),
                cancel,
                breaker: breaker::LlmBreaker::new(),
                events,
                drain,
                _tmp: tmp,
            }
        }

        async fn run(&self, engine: &UltraEngine) -> UltraOutcome {
            let client: Arc<dyn LlmProvider> = self.provider.clone();
            run(
                engine,
                "add a flag",
                &[ChatMessage::user("earlier turn")],
                &client,
                "parent-active-model",
                &self.registry,
                &self.hooks,
                &self.ctx,
                &self.cancel,
                &self.breaker,
                &self.events,
            )
            .await
        }

        /// Every event emitted so far. The sender is still alive, so drain by
        /// polling rather than by waiting for the channel to close.
        fn events(&mut self) -> Vec<AgentEvent> {
            let mut drained = Vec::new();
            while let Ok(event) = self.drain.try_recv() {
                drained.push(event);
            }
            drained
        }

        /// The user message of the single request served to `lens` — a
        /// subagent's brief.
        fn brief_for(&self, lens: &str) -> String {
            let requests = self.provider.requests_for(lens);
            assert_eq!(requests.len(), 1, "expected exactly one '{lens}' request");
            requests[0]
                .messages
                .iter()
                .find(|message| matches!(message.role, Role::User))
                .expect("a brief")
                .text()
        }
    }

    /// `(run, name)` of every pane opened.
    fn started(events: &[AgentEvent]) -> Vec<(u64, String)> {
        events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::SubagentRunStarted { run, name, .. } => Some((*run, name.clone())),
                _ => None,
            })
            .collect()
    }

    /// `(run, completed, error)` of every pane closed.
    fn done(events: &[AgentEvent]) -> Vec<(u64, bool, Option<String>)> {
        events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::SubagentRunDone {
                    run,
                    completed,
                    error,
                    ..
                } => Some((*run, *completed, error.clone())),
                _ => None,
            })
            .collect()
    }

    fn guidance(outcome: &UltraOutcome) -> &str {
        match outcome {
            UltraOutcome::Guidance(text) => text,
            other => panic!("expected guidance, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn candidates_run_read_only_on_the_parent_model() {
        let harness = Harness::new(LensProvider::new());
        let outcome = harness.run(&engine(&["implementer", "skeptic"], 0)).await;
        assert!(matches!(outcome, UltraOutcome::Guidance(_)));

        let seen = harness.provider.seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "one request per candidate");
        for request in seen.iter() {
            assert_eq!(
                request.model, "parent-active-model",
                "candidates run on the model the agent passed, never the configured one"
            );
            let tools: Vec<&str> = request
                .tools
                .iter()
                .map(|spec| spec.function.name.as_str())
                .collect();
            assert_eq!(
                tools,
                vec!["probe"],
                "read_only strips every Edit/Execute tool — which is what stops a candidate \
                 writing files or calling spawn_subagent — and candidate_registry drops the \
                 ReadOnly-but-useless interview/todo"
            );
        }
    }

    /// The advertised roster is one half of scoping a candidate; the other is
    /// that a tool the roster left out is not *there*. A candidate that goes
    /// looking for the parent's Edit tool anyway must find nothing to call,
    /// because `read_only: true` and [`candidate_registry`] build the registry
    /// the run dispatches against, not a list of suggestions.
    #[tokio::test]
    async fn a_candidate_that_reaches_for_a_write_tool_finds_it_absent() {
        let harness = Harness::new(LensProvider::new().reaching(&["implementer"]));
        let outcome = harness.run(&engine(&["implementer", "skeptic"], 0)).await;
        assert!(matches!(outcome, UltraOutcome::Guidance(_)));

        let requests = harness.provider.requests_for("implementer");
        assert_eq!(requests.len(), 2, "the write attempt, then the draft");
        let feedback = requests[1]
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::Tool)
            .expect("the attempt was answered");
        assert!(
            feedback.text().contains("unknown tool: mutate"),
            "the parent's Edit tool does not exist inside the run: {}",
            feedback.text()
        );
        assert!(
            !requests[0]
                .tools
                .iter()
                .any(|spec| spec.function.name == "mutate"),
            "and it was never offered in the first place"
        );
    }

    #[tokio::test]
    async fn every_candidate_and_judge_gets_exactly_one_pane() {
        let mut harness = Harness::new(LensProvider::new());
        let outcome = harness
            .run(&engine(&["implementer", "skeptic", "minimalist"], 1))
            .await;
        assert!(matches!(outcome, UltraOutcome::Guidance(_)));

        let events = harness.events();
        let started = started(&events);
        assert_eq!(started.len(), 4, "three candidates and one judge");
        let ids: HashSet<u64> = started.iter().map(|(run, _)| *run).collect();
        assert_eq!(ids.len(), 4, "every run has its own id");

        let done = done(&events);
        assert_eq!(done.len(), 4, "exactly one Done per started run");
        for (run, _) in &started {
            assert_eq!(
                done.iter().filter(|(id, ..)| id == run).count(),
                1,
                "a second Done for run {run} flips its pane from Done to Failed"
            );
        }
    }

    #[tokio::test]
    async fn a_dead_candidate_does_not_lose_the_turn() {
        let mut harness = Harness::new(LensProvider::new().failing(&["skeptic"]));
        let outcome = harness
            .run(&engine(&["implementer", "skeptic", "minimalist"], 0))
            .await;
        let guidance = guidance(&outcome);
        assert!(guidance.contains(&draft_text("implementer")));
        assert!(guidance.contains(&draft_text("minimalist")));
        assert!(
            !guidance.contains(&draft_text("skeptic")),
            "a dead candidate contributes nothing"
        );

        let events = harness.events();
        let skeptic = started(&events)
            .into_iter()
            .find(|(_, name)| name == "skeptic")
            .expect("skeptic's pane opened")
            .0;
        assert_eq!(
            done(&events)
                .iter()
                .filter(|(run, ..)| *run == skeptic)
                .count(),
            1,
            "spawn closed the failed run's pane itself; ultra must not close it a second time"
        );
    }

    #[tokio::test]
    async fn every_candidate_dead_skips_ultra_and_runs_an_ordinary_turn() {
        let harness = Harness::new(LensProvider::new().failing(&["implementer", "skeptic"]));
        let outcome = harness.run(&engine(&["implementer", "skeptic"], 1)).await;
        match outcome {
            UltraOutcome::Skipped(reason) => assert!(reason.contains("no candidate"), "{reason}"),
            other => panic!("expected an ordinary turn, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_empty_draft_is_dropped() {
        let harness = Harness::new(LensProvider::new().empty(&["minimalist"]));
        let outcome = harness
            .run(&engine(&["implementer", "skeptic", "minimalist"], 1))
            .await;
        let guidance = guidance(&outcome);
        assert!(
            !guidance.contains(subagent::NO_FINAL_TEXT),
            "a run that only ever called tools said nothing worth injecting"
        );
        assert!(!guidance.contains("[lens 'minimalist'"));
        assert!(
            !harness
                .brief_for(JUDGE_NAME)
                .contains(subagent::NO_FINAL_TEXT),
            "and the judge is not asked to weigh it either"
        );

        // The same lens on its own leaves ultra with nothing at all.
        let harness = Harness::new(LensProvider::new().empty(&["minimalist"]));
        let outcome = harness.run(&engine(&["minimalist"], 0)).await;
        assert!(matches!(outcome, UltraOutcome::Skipped(_)));
    }

    #[tokio::test]
    async fn the_judge_sees_every_usable_draft() {
        let harness = Harness::new(
            LensProvider::new()
                .failing(&["skeptic"])
                .empty(&["minimalist"]),
        );
        // `edge-cases` is outside LENS_NAMES, so the stub does not recognize it
        // and it drafts as "unknown" — still a perfectly usable draft.
        let outcome = harness
            .run(&engine(
                &["implementer", "skeptic", "minimalist", "edge-cases"],
                1,
            ))
            .await;
        assert!(matches!(outcome, UltraOutcome::Guidance(_)));

        let brief = harness.brief_for(JUDGE_NAME);
        assert!(brief.contains(&draft_text("implementer")));
        assert!(brief.contains(&draft_text("unknown")));
        assert!(!brief.contains(&draft_text("skeptic")), "the dead one");
        assert!(!brief.contains(subagent::NO_FINAL_TEXT), "the empty one");
    }

    #[tokio::test]
    async fn fewer_than_two_usable_drafts_skips_the_judge() {
        let mut harness = Harness::new(LensProvider::new().failing(&["skeptic"]));
        let outcome = harness.run(&engine(&["implementer", "skeptic"], 1)).await;
        assert!(matches!(outcome, UltraOutcome::Guidance(_)));

        assert!(
            harness.provider.requests_for(JUDGE_NAME).is_empty(),
            "one draft is nothing to compare"
        );
        let events = harness.events();
        assert!(
            !started(&events).iter().any(|(_, name)| name == JUDGE_NAME),
            "so no judge pane opened either"
        );
    }

    #[tokio::test]
    async fn judges_zero_skips_the_compare_phase() {
        let mut harness = Harness::new(LensProvider::new());
        let outcome = harness.run(&engine(&["implementer", "skeptic"], 0)).await;
        let guidance = guidance(&outcome);
        assert!(guidance.contains(&draft_text("implementer")));
        assert!(guidance.contains(&draft_text("skeptic")));
        assert!(
            !guidance.contains("Judge verdict"),
            "no judges, no verdict section"
        );

        assert!(harness.provider.requests_for(JUDGE_NAME).is_empty());
        let events = harness.events();
        assert_eq!(started(&events).len(), 2, "candidates only");
    }

    #[tokio::test]
    async fn an_incomplete_draft_is_kept_but_marked() {
        // `skeptic` speaks *and* calls a tool every step, so with a one-step
        // budget it ends unfinished — with a last message still worth reading.
        let engine = UltraEngine {
            lenses: vec![lens("implementer", 2), lens("skeptic", 1)],
            judge: lens(JUDGE_NAME, 2),
            judges: 0,
            timeout: Duration::from_secs(30),
            max_draft_chars: 6_000,
            seats: Vec::new(),
        };
        let harness = Harness::new(LensProvider::new().chatty(&["skeptic"]));
        let outcome = harness.run(&engine).await;

        let guidance = guidance(&outcome);
        assert!(
            guidance.contains("[lens 'skeptic' — incomplete, hit its 1-step budget]"),
            "a partial thought is kept, but weighed as one: {guidance}"
        );
        assert!(guidance.contains(&draft_text("skeptic")));
        assert!(guidance.contains("[lens 'implementer' — 1 step(s)]"));
    }

    #[tokio::test]
    async fn cancellation_closes_only_the_open_panes_and_stops_the_turn() {
        // Cancelled before anything ran: every pane opens and is closed out.
        let cancel = CancelHandle::default();
        cancel.cancel();
        let mut harness = Harness::with_cancel(cancel, LensProvider::new());
        let outcome = harness
            .run(&engine(&["implementer", "skeptic", "minimalist"], 1))
            .await;
        assert_eq!(outcome, UltraOutcome::Cancelled);

        let events = harness.events();
        let panes = started(&events);
        let closed_panes = done(&events);
        assert_eq!(panes.len(), 3, "the judge never got to run");
        assert_eq!(
            closed_panes.len(),
            panes.len(),
            "and no pane was left running"
        );
        for (run, _) in &panes {
            let closed: Vec<_> = closed_panes.iter().filter(|(id, ..)| id == run).collect();
            assert_eq!(closed.len(), 1);
            assert!(!closed[0].1);
            assert_eq!(closed[0].2.as_deref(), Some("cancelled"));
        }

        // Cancelled mid-flight, once the first candidate is already done: its
        // pane must stay Done, never be re-marked Failed by a second event.
        let cancel = CancelHandle::default();
        let mut harness = Harness::with_cancel(
            cancel.clone(),
            LensProvider::new()
                .cancelling_after("implementer", &cancel)
                .stalling(&["skeptic"]),
        );
        let outcome = harness.run(&engine(&["implementer", "skeptic"], 1)).await;
        assert_eq!(outcome, UltraOutcome::Cancelled);

        let events = harness.events();
        let implementer = started(&events)
            .into_iter()
            .find(|(_, name)| name == "implementer")
            .expect("implementer started")
            .0;
        let closed: Vec<_> = done(&events)
            .into_iter()
            .filter(|(run, ..)| *run == implementer)
            .collect();
        assert_eq!(closed.len(), 1, "the finished run is closed exactly once");
        assert!(closed[0].1, "and stays Done, not Failed");
    }

    #[tokio::test]
    async fn a_stalled_candidate_is_killed_by_the_timeout() {
        let mut engine = engine(&["implementer", "skeptic"], 0);
        engine.timeout = Duration::from_millis(50);
        let mut harness = Harness::new(LensProvider::new().stalling(&["skeptic"]));

        let started_at = std::time::Instant::now();
        let outcome = harness.run(&engine).await;
        assert!(
            started_at.elapsed() < Duration::from_secs(5),
            "the deadline ends a stalled candidate, not spawn's 315s retry ladder"
        );

        let guidance = guidance(&outcome);
        assert!(guidance.contains(&draft_text("implementer")));
        assert!(!guidance.contains(&draft_text("skeptic")));

        let events = harness.events();
        let skeptic = started(&events)
            .into_iter()
            .find(|(_, name)| name == "skeptic")
            .expect("skeptic's pane opened")
            .0;
        let closed: Vec<_> = done(&events)
            .into_iter()
            .filter(|(run, ..)| *run == skeptic)
            .collect();
        assert_eq!(closed.len(), 1);
        assert!(
            closed[0]
                .2
                .as_deref()
                .is_some_and(|why| why.contains("timed out")),
            "spawn's future was dropped, so nobody but ultra could have closed this pane"
        );
    }

    #[tokio::test]
    async fn guidance_is_bounded_by_the_context_window() {
        let bulk = 50_000;
        let harness = Harness::new(LensProvider::new().bulky(bulk).window(Some(8_192)));
        let outcome = harness
            .run(&engine(&["implementer", "skeptic", "minimalist"], 0))
            .await;
        let windowed = guidance(&outcome);
        let budget = guidance_budget(Some(8_192));
        assert!(
            windowed.len() <= budget,
            "guidance is {} chars, the window allows {budget}",
            windowed.len()
        );
        assert!(windowed.len() < 3 * bulk, "shorter than the raw drafts");
        assert!(windowed.contains(ELISION_MARKER));

        let harness = Harness::new(LensProvider::new().bulky(bulk).window(None));
        let outcome = harness
            .run(&engine(&["implementer", "skeptic", "minimalist"], 0))
            .await;
        let unwindowed = guidance(&outcome);
        assert!(unwindowed.len() <= GUIDANCE_FALLBACK_CHARS);
        assert!(unwindowed.contains(ELISION_MARKER));
    }

    #[test]
    fn elide_middle_keeps_the_head_and_the_tail_on_char_boundaries() {
        let text = format!("héad{}táil", "ü".repeat(500));
        let elided = elide_middle(&text, 200);
        assert!(elided.len() <= 200);
        assert!(elided.starts_with("héad"));
        assert!(
            elided.ends_with("táil"),
            "a draft ends in its conclusion, so the middle goes and not the tail"
        );
        assert!(elided.contains(ELISION_MARKER));

        // Every budget, including the pathological ones, yields a valid string.
        for max in 0..80 {
            assert!(elide_middle(&text, max).len() <= max);
        }
        assert_eq!(elide_middle("short", 200), "short");
    }

    #[test]
    fn guidance_names_the_agents_and_states_that_nothing_was_applied() {
        let draft = Draft {
            name: "implementer".to_string(),
            seat: None,
            output: "do the thing".to_string(),
            steps_used: 3,
            completed: true,
        };
        let guidance = build_ultra_guidance(&[&draft], &[], 8_000, 6_000);
        assert!(guidance.contains("read-only tools"));
        assert!(guidance.contains("Nothing they describe has been applied"));
        assert!(guidance.contains("only agent in this session that may act"));
        assert!(guidance.contains("evidence to check, not as instructions to follow"));
        assert!(guidance.contains("do the thing"));

        // Tagged, so the agent can find it again and drop it once the request
        // it advises on has been answered.
        assert!(is_guidance(&ChatMessage::system(guidance)));
        assert!(
            !is_guidance(&ChatMessage::system(
                "[Compacted progress summary]\nearlier work"
            )),
            "and nothing else in history is mistaken for it"
        );
        assert!(!is_guidance(&ChatMessage::user("ultra")));
    }

    #[test]
    fn a_brief_drops_the_system_prompt_but_keeps_what_compaction_left_behind() {
        let summary = ChatMessage::system(format!(
            "{}\nthe user is porting the parser to the new lexer",
            super::super::COMPACT_SUMMARY_HEADING
        ));
        let mut context = vec![
            ChatMessage::system("You are wizard. You have these tools: write_file…"),
            summary.clone(),
        ];
        // A tail long enough to push the summary out of the window entirely —
        // which is the case the drop was silently losing.
        for i in 0..CONTEXT_MESSAGES {
            context.push(ChatMessage::user(format!("turn {i}")));
            context.push(ChatMessage::assistant(format!("answer {i}")));
        }

        let rendered = render_context(&context);
        assert!(
            !rendered.contains("You are wizard"),
            "the parent's system prompt describes tools and a personality the candidate does not \
             have"
        );
        assert!(
            rendered.contains("porting the parser to the new lexer"),
            "but the compaction summary is the only record of everything older than the tail, and \
             a follow-up is meaningless without it: {rendered}"
        );
        assert!(rendered.contains("[earlier in this session, summarized]"));
        assert!(rendered.contains("answer 7"), "and the tail is still there");

        // An ordinary injected note inside the window is conversation too.
        let context = vec![
            ChatMessage::system("You are wizard."),
            ChatMessage::user("build it"),
            ChatMessage::system("[background task #1 finished] cargo build: 0 errors"),
        ];
        let rendered = render_context(&context);
        assert!(rendered.contains("cargo build: 0 errors"));
        assert!(!rendered.contains("You are wizard"));
    }

    #[test]
    fn guidance_budget_clamps_to_the_window() {
        assert_eq!(guidance_budget(None), GUIDANCE_FALLBACK_CHARS);
        assert_eq!(guidance_budget(Some(1_024)), GUIDANCE_MIN_CHARS);
        assert_eq!(guidance_budget(Some(8_192)), 8_192 * 4 * 15 / 100);
        assert_eq!(guidance_budget(Some(1_000_000)), GUIDANCE_MAX_CHARS);
    }

    #[test]
    fn build_is_the_single_validation_gate() {
        let tmp = TempDir::new();
        let base = UltraConfig::default();
        UltraEngine::build(&base, &tmp.0).expect("the defaults build clean");

        let cases: Vec<(UltraConfig, &str)> = vec![
            (
                UltraConfig {
                    lenses: Vec::new(),
                    ..base.clone()
                },
                "lenses",
            ),
            (
                UltraConfig {
                    lenses: vec!["skeptic".to_string(), "skeptic".to_string()],
                    ..base.clone()
                },
                "lenses",
            ),
            (
                UltraConfig {
                    lenses: vec!["nope".to_string()],
                    ..base.clone()
                },
                "lenses",
            ),
            (
                UltraConfig {
                    lenses: (0..=MAX_LENSES).map(|i| format!("l{i}")).collect(),
                    ..base.clone()
                },
                "lenses",
            ),
            (
                UltraConfig {
                    judges: MAX_JUDGES + 1,
                    ..base.clone()
                },
                "judges",
            ),
            (
                UltraConfig {
                    candidate_max_steps: 0,
                    ..base.clone()
                },
                "candidate_max_steps",
            ),
            (
                UltraConfig {
                    judge_max_steps: 0,
                    ..base.clone()
                },
                "judge_max_steps",
            ),
            (
                UltraConfig {
                    timeout_secs: 0,
                    ..base.clone()
                },
                "timeout_secs",
            ),
            (
                UltraConfig {
                    max_draft_chars: MIN_DRAFT_CHARS - 1,
                    ..base.clone()
                },
                "max_draft_chars",
            ),
        ];
        for (cfg, field) in cases {
            let err = UltraEngine::build(&cfg, &tmp.0).expect_err("an invalid config is rejected");
            let message = format!("{err:#}");
            assert!(
                message.contains(field),
                "the error must name the offending field '{field}': {message}"
            );
        }

        // An unknown lens lists what is actually on offer.
        let err = UltraEngine::build(
            &UltraConfig {
                lenses: vec!["nope".to_string()],
                ..base
            },
            &tmp.0,
        )
        .expect_err("an unknown lens is rejected");
        assert!(format!("{err:#}").contains("implementer"), "{err:#}");
    }

    #[test]
    fn a_lens_can_be_replaced_by_a_toml_file() {
        let tmp = TempDir::new();
        std::fs::write(
            tmp.0.join("skeptic.toml"),
            "name = \"skeptic\"\ndescription = \"mine\"\nsystem_prompt = \"be mean\"\n",
        )
        .unwrap();

        let catalog = lens_catalog(&tmp.0);
        let skeptics: Vec<_> = catalog
            .iter()
            .filter(|lens| lens.name == "skeptic")
            .collect();
        assert_eq!(skeptics.len(), 1, "shadowed by name, not duplicated");
        assert_eq!(skeptics[0].system_prompt, "be mean");
        assert!(
            !catalog.iter().any(|lens| lens.name == JUDGE_NAME),
            "the judge has its own row in /ultra config, never a lens row"
        );
    }

    #[test]
    fn ultra_overrides_a_lens_budget_and_tool_scope() {
        let tmp = TempDir::new();
        std::fs::write(
            tmp.0.join("skeptic.toml"),
            "name = \"skeptic\"\ndescription = \"mine\"\nsystem_prompt = \"be mean\"\n\
             max_steps = 99\ntool_scope = [\"write_file\"]\n",
        )
        .unwrap();

        let cfg = UltraConfig {
            lenses: vec!["skeptic".to_string()],
            candidate_max_steps: 4,
            ..UltraConfig::default()
        };
        let engine = UltraEngine::build(&cfg, &tmp.0).expect("builds");
        assert_eq!(
            engine.lenses[0].max_steps,
            StepBudget::new(4),
            "ultra owns the budget"
        );
        assert!(
            engine.lenses[0].tool_scope.is_none(),
            "a lens contributes a prompt, never a scope"
        );
        assert_eq!(engine.judge.max_steps, StepBudget::new(cfg.judge_max_steps));
        assert_eq!(engine.candidates(), 1);
    }

    #[test]
    fn label_states_the_roster_and_the_judge_count() {
        let tmp = TempDir::new();
        let mut engine = UltraEngine::build(&UltraConfig::default(), &tmp.0).expect("builds");
        assert_eq!(
            engine.label(),
            "ultra \u{00d7}3 \u{00b7} implementer+skeptic+minimalist \u{00b7} 1 judge"
        );

        // Seated, the label says where the spend is going too. A user who
        // toggled both modes has to be able to read what the next turn costs.
        engine = engine.with_seats(vec![seat("alice", &Arc::new(LensProvider::new()))]);
        assert!(
            engine.label().ends_with("\u{00b7} across alice"),
            "{}",
            engine.label()
        );
    }

    #[tokio::test]
    async fn an_empty_roster_skips_and_runs_an_ordinary_turn() {
        let harness = Harness::new(LensProvider::new());
        let outcome = harness.run(&engine(&[], 1)).await;
        assert!(matches!(outcome, UltraOutcome::Skipped(_)));
        assert!(
            harness.provider.seen.lock().unwrap().is_empty(),
            "nothing to fan out is an ordinary turn, not an error"
        );
    }

    /// A seat pointing at `provider`, named and modelled after it.
    fn seat(name: &str, provider: &Arc<LensProvider>) -> Seat {
        let client: Arc<dyn LlmProvider> = provider.clone();
        Seat {
            provider: Some(name.to_string()),
            client: Some(client),
            model: Some(format!("m-{name}")),
        }
    }

    /// The configuration the old code had no way to express: several lenses,
    /// dealt across several providers.
    ///
    /// `/ultra` and `/fusion` each refused to turn on over the other, because
    /// each owned its own fan-out and stacking them meant every candidate
    /// re-running the whole panel. Seated, a candidate talks to one provider
    /// directly and the two modes compose: three lenses over two providers is
    /// three runs, not six debates.
    #[tokio::test]
    async fn lenses_are_dealt_across_their_seats() {
        let alice = Arc::new(LensProvider::new());
        let bob = Arc::new(LensProvider::new());
        let mut engine = engine(&["implementer", "skeptic", "minimalist"], 1);
        engine.seats = vec![seat("alice", &alice), seat("bob", &bob)];

        let mut harness = Harness::new(LensProvider::new());
        let outcome = harness.run(&engine).await;
        let guidance = guidance(&outcome);

        // Round-robin over two seats: lenses 0 and 2 to alice, lens 1 to bob.
        assert_eq!(
            alice.requests_for("implementer").len(),
            1,
            "the first lens is seated on the first provider"
        );
        assert_eq!(
            alice.requests_for("minimalist").len(),
            1,
            "and so is the third"
        );
        assert_eq!(alice.requests_for("skeptic").len(), 0);
        assert_eq!(
            bob.requests_for("skeptic").len(),
            1,
            "the second lens is bob's"
        );

        for request in alice.seen.lock().unwrap().iter() {
            assert_eq!(
                request.model, "m-alice",
                "a seat carries its own model, not the parent's active one"
            );
        }
        for request in bob.seen.lock().unwrap().iter() {
            assert_eq!(request.model, "m-bob");
        }

        // The judge stays on the council's own seat: it reads drafts that
        // already came from everywhere, and dealing it across providers would
        // make which model ruled depend on how many lenses there were.
        let own = harness.provider.seen.lock().unwrap();
        assert_eq!(own.len(), 1, "one call on the parent's client: the judge");
        assert_eq!(own[0].model, "parent-active-model");
        drop(own);

        // And the guidance says which provider each draft came from, because
        // with a mixed roster that is the reader's next question.
        assert!(
            guidance.contains("[lens 'implementer' via alice"),
            "{guidance}"
        );
        assert!(guidance.contains("[lens 'skeptic' via bob"), "{guidance}");

        let events = harness.events();
        assert_eq!(
            started(&events).len(),
            4,
            "three seated candidates and one judge, each with its own pane"
        );
    }

    /// An interrupt ends the sitting promptly, even when every candidate is
    /// parked inside a provider that will never answer.
    ///
    /// The interrupt now lives inside [`subagent::spawn`] rather than in a
    /// `select!` this module wrapped around it. That moved the honouring of a
    /// Ctrl-C from "whichever callers remembered to wrap" to "every run", and
    /// this pins that it did not cost the promptness the wrapper had.
    #[tokio::test]
    async fn a_cancelled_council_stops_without_waiting_for_its_candidates() {
        let cancel = CancelHandle::default();
        let mut harness = Harness::with_cancel(
            cancel.clone(),
            LensProvider::new().stalling(&["implementer", "skeptic", "minimalist"]),
        );

        let raised = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            raised.cancel();
        });

        let started_at = std::time::Instant::now();
        let outcome = harness
            .run(&engine(&["implementer", "skeptic", "minimalist"], 1))
            .await;
        assert_eq!(outcome, UltraOutcome::Cancelled);
        assert!(
            started_at.elapsed() < Duration::from_secs(5),
            "the sitting ends on the interrupt, not on the 30s candidate deadline"
        );

        let events = harness.events();
        let panes = started(&events);
        let closed = done(&events);
        assert_eq!(panes.len(), 3, "the judge never got to run");
        assert_eq!(
            closed.len(),
            panes.len(),
            "and every pane the sitting opened was closed out exactly once"
        );
        for (run, _) in &panes {
            let closed: Vec<_> = closed.iter().filter(|(id, ..)| id == run).collect();
            assert_eq!(closed.len(), 1);
            assert_eq!(closed[0].2.as_deref(), Some("cancelled"));
        }
    }

    #[test]
    fn extract_refined_takes_the_last_header_and_tolerates_markdown() {
        assert_eq!(
            extract_refined("- Critique: fine\n- Refined Answer: 42"),
            "42"
        );
        assert_eq!(
            extract_refined("Critique: ok\n**Refined Answer:** bold"),
            "bold"
        );
        assert_eq!(
            extract_refined("Critique: ok\n**Refined Answer**: bold"),
            "bold"
        );
        // The real trailing header beats an in-critique mention of the phrase.
        assert_eq!(
            extract_refined(
                "Critique: my refined answer: was weak.\nRefined Answer: the strong one"
            ),
            "the strong one"
        );
        // The header may sit alone on its line, with the answer under it.
        assert_eq!(
            extract_refined("Critique: ok\nRefined Answer:\n  the body\n"),
            "the body"
        );
        // Case, tabs, and a bullet in one.
        assert_eq!(extract_refined("\t*\tREFINED ANSWER\t:\tshouty"), "shouty");
    }

    #[test]
    fn extract_refined_falls_back_to_the_whole_reply() {
        assert_eq!(extract_refined("  no header at all  "), "no header at all");
        assert_eq!(extract_refined(""), "");
        // Mid-sentence the phrase is prose, not a header, so nothing is cut.
        let prose = "I would call this my refined answer: it is done.";
        assert_eq!(extract_refined(prose), prose);
    }

    /// A peer with nothing to say is not quoted as one. A bare `[name]` header
    /// with an empty body under it reads as a peer who considered the question
    /// and declined, which is a different thing from a peer whose provider was
    /// down.
    #[test]
    fn a_failed_candidate_is_not_a_peer_to_critique() {
        let candidates = vec![
            CandidateOutcome::Drafted(Draft {
                name: "alice".to_string(),
                seat: None,
                output: "alice's answer".to_string(),
                steps_used: 1,
                completed: true,
            }),
            CandidateOutcome::Failed {
                name: "down".to_string(),
                seat: None,
                why: "unreachable".to_string(),
            },
        ];
        assert_eq!(peers(&candidates, 0), Vec::new(), "and never itself");
        assert_eq!(
            peers(&candidates, 1),
            vec![("alice".to_string(), "alice's answer".to_string())]
        );
    }
}
