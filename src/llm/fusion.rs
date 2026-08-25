//! `FusionProvider`: a council of providers, as an [`LlmProvider`].
//!
//! Wraps a *panel* of Wizard providers and a *synthesizer*. On each turn the
//! panel members independently answer and critique each other over N rounds,
//! then the synthesizer produces the final answer with the panel's drafts
//! injected as guidance. Because this is just another [`LlmProvider`], the agent
//! loop, tools, and TUI are unchanged: `/fusion` simply swaps the active
//! provider to one of these and back.
//!
//! **The debate is not implemented here.** It is one configuration of the
//! council in [`crate::agent::ultra`]: candidates whose [`CandidateKind`] is
//! [`CandidateKind::Panel`], each on a [`Seat`] naming its own provider, with
//! [`Adjudicator::Debate`] for the critique rounds. `/ultra` is the same
//! primitive with lens subagents and judges instead. Before they were merged
//! this file carried its own fan-out, which is why the two modes could not be
//! stacked: each was the only thing that knew how to run a fan-out, so there was
//! no way to describe one that mixed them.
//!
//! What stays here is what is specific to being a *provider*: which model
//! synthesizes, how the conversation is flattened for members that see no
//! structured history, the guidance the synthesizer gets, and the run log.
//!
//! **Tool semantics:** panel members are advisors (text only, no tools); the
//! synthesizer is the sole actor — it receives the request's `tools` and is the
//! only model that may emit `tool_calls`. So fusion works on agentic turns, not
//! just Q&A, with no conflicting tool calls.
//!
//! **Why the debate lives in-tree at all.** It used to come from an out-of-tree
//! `fusion-core` crate, declared in `Cargo.toml` as a bare `{ git = "..." }`
//! dependency. Cargo refuses to publish any crate that has one, so that single
//! line was the whole reason `cargo install wizard` (and every packaging route
//! downstream of crates.io) could not exist. The engine was one adapter away
//! from the shape Wizard already needed, and it was reachable from exactly one
//! file (this one), so it was brought in-tree rather than pinned to a rev. Four
//! of its layers did not come across, because nothing on this path could reach
//! them and carrying them in would have been dead scaffolding:
//!
//! * **Retry, backoff, and fallback models.** The old adapter reported every
//!   panel failure as a non-retryable HTTP 400 and configured no fallback
//!   models, deliberately, so that a dead member degraded fast instead of
//!   stalling a turn behind three exponential sleeps. The retry loop therefore
//!   never made a second attempt. Retries belong to the Wizard providers
//!   underneath, which own the transport and its error taxonomy.
//! * **`max_tokens`, `temperature`, `seed`.** The adapter built its Wizard
//!   [`ChatRequest`] with `options: None`, so none of the three ever reached a
//!   wire. Panel members answer under the same sampling settings as any other
//!   Wizard turn.
//! * **Paper mode.** `/fusion` always passed `paper_mode: false`; only the
//!   standalone FUSION CLI ever set it.
//! * **Progress events.** `/fusion` passed a no-op event sink. The panel is
//!   invisible mid-turn by design: the user sees the synthesizer's stream.
//!
//! [`CandidateKind`]: crate::agent::ultra::CandidateKind
//! [`CandidateKind::Panel`]: crate::agent::ultra::CandidateKind::Panel
//! [`Seat`]: crate::agent::ultra::Seat
//! [`Adjudicator::Debate`]: crate::agent::ultra::Adjudicator::Debate

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent::ultra::{
    Adjudicator, Brief, Candidate, CandidateKind, CandidateOutcome, Council, CouncilJournal,
    CouncilOutcome, Seat,
};
use crate::config::{Config, FusionConfig, ProviderConfig};
use crate::llm::provider::LlmProvider;
use crate::llm::{ChatMessage, ChatRequest, ChatStream, Role};

/// Wall-clock cap on one panel member's call.
///
/// The panel had none, which is the failure `[ultra] timeout_secs` exists to
/// prevent, one layer down: a member behind a throttled endpoint parked the
/// synthesis behind it indefinitely, and the user saw a spinner and never
/// learned that one advisor was the reason. Generous, because a panel member is
/// a full cloud completion on a long conversation and not a health check.
const PANEL_TIMEOUT: Duration = Duration::from_secs(300);

/// One member of the debate panel: a Wizard provider bound to its model, plus a
/// unique routing key (its provider name).
pub struct PanelMember {
    /// Unique routing key — the provider's configured name.
    pub name: String,
    /// The built Wizard provider for this member.
    pub provider: Arc<dyn LlmProvider>,
    /// The model tag to request against `provider`.
    pub model: String,
}

/// The council seats a `[fusion]` panel offers, for a roster that is *not* the
/// panel.
///
/// This is what makes `/ultra` and `/fusion` composable rather than mutually
/// exclusive. With fusion on, the agent's active client is a [`FusionProvider`],
/// so an ultra candidate left unseated would run the whole panel debate for its
/// own single draft: candidates × panel × rounds before the first token, which
/// is exactly the bill the two modes used to refuse each other over. Seated on
/// these, each candidate talks to one panel provider directly, and "three lenses
/// across two providers" costs three runs.
///
/// The providers are built fresh rather than borrowed off a live
/// [`FusionProvider`], because the surface that needs the seats holds the client
/// as an `Arc<dyn LlmProvider>` and cannot get the concrete type back out of it.
/// Building is what [`Config::build_fusion_from`] does too, and a provider is a
/// thin HTTP client.
pub fn panel_seats(fusion: &FusionConfig, providers: &[ProviderConfig]) -> Result<Vec<Seat>> {
    let mut seats = Vec::with_capacity(fusion.panel.len());
    for name in &fusion.panel {
        let configured = providers
            .iter()
            .find(|provider| &provider.name == name)
            .with_context(|| format!("fusion references unknown provider '{name}'"))?;
        seats.push(Seat {
            provider: Some(configured.name.clone()),
            client: Some(configured.build()?),
            model: Some(configured.model.clone()),
        });
    }
    Ok(seats)
}

/// Best-effort JSONL log of every panel call, appended to
/// `~/.wizard/fusion-runs.jsonl`: one row per member per phase, carrying
/// `{phase, agent, model, request: {prompt}, response}`.
///
/// This is the only record a fused turn leaves of its debate. The user sees the
/// synthesizer's answer and nothing else, so without this there is no way to
/// tell a panel that disagreed productively from one whose members all timed
/// out and contributed nothing.
///
/// The schema is the out-of-tree engine's, unchanged, so old and new rows sit in
/// the same file and parse the same way. Two of the fields did start saying
/// something, because under the old adapter they said nothing. `model` used to
/// repeat `agent`: routing worked by putting the member's *name* in the engine's
/// model slot, so every row named the routing key twice and never the model that
/// actually answered. `response` used to be `null` on success, because the
/// adapter had no provider-native body to hand back and filled that slot with
/// `Value::Null`, so a successful row proved a call happened and nothing more.
/// Both now hold what the field name claims: the member's configured model tag,
/// and the text it returned. Failure rows are unchanged
/// (`{error, content: ""}`), which is why they were the only readable half of
/// the old log.
///
/// Writes are best-effort: a logging failure must never lose a turn, so errors
/// are traced and swallowed. A `~/.wizard` that will not resolve disables the
/// log outright rather than dropping a stray `runs.jsonl` into whatever
/// directory Wizard happens to have been started in.
struct RunLog {
    path: Option<PathBuf>,
}

impl RunLog {
    fn open() -> Self {
        Self {
            path: Config::wizard_dir()
                .ok()
                .map(|dir| dir.join("fusion-runs.jsonl")),
        }
    }

    /// Append the row for one member call.
    fn step(&self, phase: &str, agent: &str, model: &str, prompt: &str, response: &Value) {
        let Some(path) = &self.path else {
            return;
        };
        let row = json!({
            "phase": phase,
            "agent": agent,
            "model": model,
            "request": { "prompt": prompt },
            "response": response,
        });
        if let Err(e) = append_row(path, &row) {
            tracing::error!("failed to write the fusion run log {}: {e}", path.display());
        }
    }
}

impl CouncilJournal for RunLog {
    /// The council does not know what a JSONL row is and this file does not
    /// know when a call happened; the trait is the whole of the seam between
    /// them.
    fn record(
        &self,
        phase: &str,
        candidate: &str,
        model: &str,
        prompt: &str,
        answer: Result<&str, &str>,
    ) {
        let response = match answer {
            Ok(text) => json!({ "content": text }),
            // Unchanged from the out-of-tree engine's failure shape, which is
            // what makes old and new rows parse the same way.
            Err(err) => json!({ "error": err, "content": "" }),
        };
        self.step(phase, candidate, model, prompt, &response);
    }
}

/// Append one row and its newline in a *single* write to an append-only handle,
/// so that two concurrent sessions logging the same file cannot interleave half
/// a row with half of another and corrupt both.
fn append_row(path: &Path, row: &Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let mut line = serde_json::to_string(row).unwrap_or_else(|_| "{}".to_string());
    line.push('\n');
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(line.as_bytes())
}

/// Model fusion exposed as a single [`LlmProvider`].
pub struct FusionProvider {
    /// The advisors. Empty degrades to the synthesizer alone, which is the real
    /// passthrough case and not an error.
    panel: Vec<PanelMember>,
    /// Critique rounds after the initial answers. Each round is one more call
    /// per member, which is why `/fusion`'s cost is `panel × (1 + rounds) + 1`.
    rounds: u32,
    /// Where every panel call is recorded. Shared with the council, which is
    /// what makes the calls it makes on this provider's behalf show up in this
    /// provider's log.
    journal: Arc<RunLog>,
    /// The provider that synthesizes the final, tool-capable, streamed answer.
    synthesizer: Arc<dyn LlmProvider>,
    /// Model tag to request against `synthesizer`.
    synth_model: String,
    /// Status-bar label, e.g. `"fusion: claude+openrouter ×1"`.
    label: String,
}

impl FusionProvider {
    /// Build a fusion provider from a resolved panel and synthesizer.
    ///
    /// `rounds` is the number of critique rounds (typically 1). `label` is shown
    /// in the status bar. An empty `panel` degrades to the synthesizer alone.
    ///
    /// Member names are the keys the debate is bookkept by: each round looks up
    /// a member's own previous answer and shows it everyone else's. So
    /// duplicates are rejected rather than silently collapsed, because
    /// `panel = ["claude", "claude"]` in `[fusion]` is a config mistake, not a
    /// two-model panel, and nothing upstream of here rejects it.
    pub fn new(
        panel: Vec<PanelMember>,
        synthesizer: Arc<dyn LlmProvider>,
        synth_model: String,
        rounds: u32,
        label: String,
    ) -> Result<Self> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut dups: Vec<String> = Vec::new();
        for member in &panel {
            if !seen.insert(member.name.clone()) {
                dups.push(member.name.clone());
            }
        }
        if !dups.is_empty() {
            bail!(
                "fusion panel lists the same provider twice: {}",
                dups.join(", ")
            );
        }

        Ok(Self {
            panel,
            rounds,
            journal: Arc::new(RunLog::open()),
            synthesizer,
            synth_model,
            label,
        })
    }

    /// The panel as a council: one bare-completion candidate per member, each
    /// seated on its own provider, adjudicated by having them critique each
    /// other.
    ///
    /// Rebuilt per turn rather than held, because a council owns the roster it
    /// is about to run and nothing here changes between turns except which
    /// conversation it is about.
    fn council(&self) -> Council {
        let candidates = self
            .panel
            .iter()
            .map(|member| Candidate {
                name: member.name.clone(),
                // No tools and no repository access: an advisor advises, and
                // the synthesizer downstream is the only thing that may act.
                kind: CandidateKind::Panel,
                seat: Seat {
                    provider: Some(member.name.clone()),
                    client: Some(Arc::clone(&member.provider)),
                    model: Some(member.model.clone()),
                },
            })
            .collect();
        // `.clone()` rather than `Arc::clone(&..)`: the latter pins the type
        // parameter to `RunLog`, and the unsize coercion to the trait object
        // happens at the binding.
        let journal: Arc<dyn CouncilJournal> = self.journal.clone();
        // The synthesizer is the default seat, which no panel candidate uses
        // (each names its own). It is what a candidate added later without one
        // would fall back to, and the synthesizer is the honest answer to
        // "whose model is this council's own".
        Council::new(Arc::clone(&self.synthesizer), self.synth_model.clone())
            .with_candidates(candidates)
            .adjudicated_by(Adjudicator::Debate {
                rounds: self.rounds,
            })
            .timed_out_after(PANEL_TIMEOUT)
            .journalled_by(journal)
    }

    /// Clone `req` with its model retargeted to the synthesizer.
    fn synth_request(&self, mut req: ChatRequest) -> ChatRequest {
        req.model = self.synth_model.clone();
        req
    }
}

/// Render the conversation into a single query string for the panel members
/// (who do not see the structured message history or tools).
///
/// The panel debates in text: the fusion engine's messages carry no images, so
/// an image in the history is named here rather than dropped in silence. The
/// synthesizer — the model that actually answers, and the only one that runs
/// tools — receives the real [`ChatRequest`] with its images intact.
fn render_query(messages: &[ChatMessage]) -> String {
    let mut parts = Vec::new();
    for m in messages {
        let attached = m.images();
        let images = if attached.is_empty() {
            String::new()
        } else {
            format!(" [{} image(s) not shown to the panel]", attached.len())
        };
        let text = m.text();
        match m.role {
            Role::System => {}
            Role::User => parts.push(format!("User: {text}{images}")),
            Role::Assistant if !text.is_empty() || !attached.is_empty() => {
                parts.push(format!("Assistant: {text}{images}"))
            }
            Role::Assistant => {}
            Role::Tool => parts.push(format!(
                "[tool {} result] {text}{images}",
                m.tool_name().unwrap_or(""),
            )),
        }
    }
    parts.join("\n\n")
}

/// Build the synthesizer guidance message that carries the panel's drafts.
///
/// Every member is named, including one that degraded to nothing, which is why
/// this takes the council's [`CandidateOutcome`]s rather than its drafts.
/// Dropping the empty ones would mean a wholly dead panel produced no drafts at
/// all, this message would not be injected, and `/fusion` would silently become
/// a plain single-model turn with nothing on the wire saying so. Hiding an empty
/// answer is the *critique* round's job, and the council does it there.
fn build_synth_guidance(candidates: &[CandidateOutcome]) -> String {
    let mut s = String::from(
        "Several expert models independently drafted answers to the user's latest request. \
Synthesize the single best response: resolve disagreements with reasoning, keep what is \
correct, and discard what is wrong. Use tools as normal if the task requires action. \
Drafts:\n\n",
    );
    for candidate in candidates {
        let answer = candidate.draft().map_or("", |draft| draft.output.as_str());
        s.push_str(&format!("[{}]\n{answer}\n\n", candidate.name()));
    }
    s
}

#[async_trait]
impl LlmProvider for FusionProvider {
    async fn health(&self) -> Result<()> {
        // The synthesizer is the critical path; panel failures degrade
        // gracefully (an unreachable member just contributes nothing).
        self.synthesizer.health().await
    }

    async fn supports_native_tools(&self, _model: &str) -> Result<bool> {
        self.synthesizer
            .supports_native_tools(&self.synth_model)
            .await
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        self.synthesizer.list_models().await
    }

    async fn chat_stream(&self, req: ChatRequest) -> Result<ChatStream> {
        if self.panel.is_empty() {
            return self.synthesizer.chat_stream(self.synth_request(req)).await;
        }

        // The whole conversation, flattened, and no separate request: a panel
        // member sees one user message, and the latest request is already the
        // last thing in it.
        let brief = Brief {
            context: render_query(&req.messages),
            request: String::new(),
        };
        // No bench: this council is not inside an agent. There is no rail to
        // open panes on, no registry to read the repository with, and no
        // interrupt, and the agent loop above does not know a council ran.
        let council = self.council();
        let sat = council.confer(&brief, None).await;

        let mut synth_req = self.synth_request(req);
        if let CouncilOutcome::Concluded(result) = sat
            && !result.candidates.is_empty()
        {
            synth_req
                .messages
                .push(ChatMessage::system(build_synth_guidance(
                    &result.candidates,
                )));
        }
        self.synthesizer.chat_stream(synth_req).await
    }

    async fn context_window(&self, _model: &str) -> Option<u32> {
        self.synthesizer.context_window(&self.synth_model).await
    }

    fn label(&self) -> String {
        self.label.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The council's, not this file's: draining a stream to text is what every
    // candidate does, whatever it is a candidate of.
    use crate::agent::ultra::collect_text;
    use crate::llm::{CacheTokens, ChatChunk};
    use futures_util::stream;
    use std::sync::Mutex;

    /// A stub provider that records the requests it sees and replies with a
    /// fixed, single-chunk answer derived from its tag.
    struct StubProvider {
        tag: String,
        seen: Arc<Mutex<Vec<ChatRequest>>>,
    }

    impl StubProvider {
        fn new(tag: &str) -> (Arc<Self>, Arc<Mutex<Vec<ChatRequest>>>) {
            let seen = Arc::new(Mutex::new(Vec::new()));
            (
                Arc::new(Self {
                    tag: tag.to_string(),
                    seen: seen.clone(),
                }),
                seen,
            )
        }
    }

    #[async_trait]
    impl LlmProvider for StubProvider {
        async fn health(&self) -> Result<()> {
            Ok(())
        }
        async fn supports_native_tools(&self, _model: &str) -> Result<bool> {
            Ok(true)
        }
        async fn list_models(&self) -> Result<Vec<String>> {
            Ok(vec![self.tag.clone()])
        }
        async fn chat_stream(&self, req: ChatRequest) -> Result<ChatStream> {
            self.seen.lock().unwrap().push(req.clone());
            let chunk = ChatChunk {
                message: Some(ChatMessage::assistant(format!("answer from {}", self.tag))),
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
        fn label(&self) -> String {
            self.tag.clone()
        }
    }

    /// A panel member with one scripted reply per call, so a review round can
    /// be made to answer in the `Critique:` / `Refined Answer:` shape the review
    /// prompt asks for.
    struct ScriptedProvider {
        replies: Mutex<std::collections::VecDeque<String>>,
    }

    impl ScriptedProvider {
        fn new(replies: &[&str]) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(replies.iter().map(|s| s.to_string()).collect()),
            })
        }
    }

    #[async_trait]
    impl LlmProvider for ScriptedProvider {
        async fn health(&self) -> Result<()> {
            Ok(())
        }
        async fn supports_native_tools(&self, _model: &str) -> Result<bool> {
            Ok(true)
        }
        async fn list_models(&self) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
        async fn chat_stream(&self, _req: ChatRequest) -> Result<ChatStream> {
            let text = self.replies.lock().unwrap().pop_front().unwrap_or_default();
            Ok(Box::pin(stream::iter(vec![Ok(chunk(&text, false, true))])))
        }
        fn label(&self) -> String {
            "scripted".to_string()
        }
    }

    /// A panel member whose backend is down: every chat attempt errors.
    struct FailingProvider;

    #[async_trait]
    impl LlmProvider for FailingProvider {
        async fn health(&self) -> Result<()> {
            Ok(())
        }
        async fn supports_native_tools(&self, _model: &str) -> Result<bool> {
            Ok(true)
        }
        async fn list_models(&self) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
        async fn chat_stream(&self, _req: ChatRequest) -> Result<ChatStream> {
            anyhow::bail!("panel member down")
        }
        fn label(&self) -> String {
            "failing".to_string()
        }
    }

    fn chunk(text: &str, thinking: bool, done: bool) -> ChatChunk {
        ChatChunk {
            message: Some(ChatMessage::assistant(text)),
            images: Vec::new(),
            thinking,
            done,
            done_reason: None,
            eval_count: None,
            prompt_eval_count: None,
            cache: CacheTokens::NONE,
        }
    }

    fn user_req(text: &str, with_tool: bool) -> ChatRequest {
        let tools = if with_tool {
            vec![crate::llm::ToolSpec::function(
                "noop",
                "does nothing",
                serde_json::json!({"type": "object"}),
            )]
        } else {
            Vec::new()
        };
        ChatRequest {
            model: "ignored".to_string(),
            messages: vec![ChatMessage::user(text)],
            tools,
            stream: true,
            options: None,
        }
    }

    #[tokio::test]
    async fn panel_advises_and_only_synthesizer_streams_and_sees_tools() {
        let (a, a_seen) = StubProvider::new("alice");
        let (b, b_seen) = StubProvider::new("bob");
        let (synth, synth_seen) = StubProvider::new("synth");

        let panel = vec![
            PanelMember {
                name: "alice".to_string(),
                provider: a,
                model: "m-alice".to_string(),
            },
            PanelMember {
                name: "bob".to_string(),
                provider: b,
                model: "m-bob".to_string(),
            },
        ];
        let fusion = FusionProvider::new(
            panel,
            synth,
            "m-synth".to_string(),
            1,
            "fusion: test".to_string(),
        )
        .unwrap();

        let out = collect_text(fusion.chat_stream(user_req("Q", true)).await.unwrap())
            .await
            .unwrap();
        // The final stream is the synthesizer's answer.
        assert_eq!(out, "answer from synth");

        // Panel members were consulted (1 initial + 1 review each), never
        // received tools, and were each routed to their own model.
        assert_eq!(a_seen.lock().unwrap().len(), 2);
        assert_eq!(b_seen.lock().unwrap().len(), 2);
        for req in a_seen.lock().unwrap().iter() {
            assert!(req.tools.is_empty(), "panel members must not get tools");
            assert_eq!(req.model, "m-alice", "alice routed to her model");
        }
        for req in b_seen.lock().unwrap().iter() {
            assert!(req.tools.is_empty(), "panel members must not get tools");
            assert_eq!(req.model, "m-bob", "bob routed to his model");
        }

        // The synthesizer ran once, kept the tools, and got the drafts injected.
        let synth_calls = synth_seen.lock().unwrap();
        assert_eq!(synth_calls.len(), 1);
        let sreq = &synth_calls[0];
        assert_eq!(sreq.model, "m-synth");
        assert_eq!(sreq.tools.len(), 1, "synthesizer is the sole tool-caller");
        let injected = sreq
            .messages
            .iter()
            .any(|m| matches!(m.role, Role::System) && m.text().contains("answer from alice"));
        assert!(injected, "panel drafts injected into the synthesis request");
    }

    #[tokio::test]
    async fn a_failing_panel_member_does_not_break_the_turn() {
        let (synth, synth_seen) = StubProvider::new("synth");
        let panel = vec![PanelMember {
            name: "down".to_string(),
            provider: Arc::new(FailingProvider),
            model: "m".to_string(),
        }];
        let fusion =
            FusionProvider::new(panel, synth, "m-synth".to_string(), 1, "fusion".to_string())
                .unwrap();
        let out = collect_text(fusion.chat_stream(user_req("Q", false)).await.unwrap())
            .await
            .unwrap();
        assert_eq!(out, "answer from synth");
        assert_eq!(synth_seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn render_query_flattens_history_for_the_panel() {
        let messages = vec![
            ChatMessage::system("hidden instructions"),
            ChatMessage::user_with_images(
                "what is this?",
                vec![crate::llm::Image::new("QUJD", "image/png")],
            ),
            ChatMessage::assistant(""),
            ChatMessage::assistant("an owl"),
            ChatMessage::tool_result("call_execute", "execute", "ok"),
        ];
        let query = render_query(&messages);
        assert!(!query.contains("hidden instructions"), "{query}");
        assert!(
            query.contains("User: what is this? [1 image(s) not shown to the panel]"),
            "{query}"
        );
        assert!(query.contains("Assistant: an owl"), "{query}");
        assert!(query.contains("[tool execute result] ok"), "{query}");
        assert_eq!(
            query.matches("Assistant:").count(),
            1,
            "empty assistant turns are dropped: {query}"
        );
    }

    #[tokio::test]
    async fn collect_text_skips_thinking_and_stops_at_done() {
        let stream: ChatStream = Box::pin(stream::iter(vec![
            Ok(chunk("pondering", true, false)),
            Ok(chunk("an", false, false)),
            Ok(chunk("swer", false, true)),
            Ok(chunk("never read", false, false)),
        ]));
        assert_eq!(collect_text(stream).await.unwrap(), "answer");
    }

    #[tokio::test]
    async fn empty_panel_degrades_to_synthesizer_alone() {
        let (synth, synth_seen) = StubProvider::new("synth");
        let fusion = FusionProvider::new(
            Vec::new(),
            synth,
            "m-synth".to_string(),
            1,
            "fusion".to_string(),
        )
        .unwrap();
        let out = collect_text(fusion.chat_stream(user_req("Q", false)).await.unwrap())
            .await
            .unwrap();
        assert_eq!(out, "answer from synth");
        assert_eq!(synth_seen.lock().unwrap().len(), 1);
        assert_eq!(synth_seen.lock().unwrap()[0].model, "m-synth");
    }

    #[tokio::test]
    async fn a_review_round_shows_a_member_its_peers_and_its_own_previous_answer() {
        let (a, a_seen) = StubProvider::new("alice");
        let (b, _) = StubProvider::new("bob");
        let (synth, _) = StubProvider::new("synth");
        let fusion = FusionProvider::new(
            vec![
                PanelMember {
                    name: "alice".to_string(),
                    provider: a,
                    model: "m-alice".to_string(),
                },
                PanelMember {
                    name: "bob".to_string(),
                    provider: b,
                    model: "m-bob".to_string(),
                },
            ],
            synth,
            "m-synth".to_string(),
            1,
            "fusion".to_string(),
        )
        .unwrap();
        collect_text(fusion.chat_stream(user_req("Q", false)).await.unwrap())
            .await
            .unwrap();

        let calls = a_seen.lock().unwrap();
        assert_eq!(calls.len(), 2, "one initial answer plus one review round");

        // Phase 1: the member's persona, then the bare query. No peer exists yet.
        assert_eq!(calls[0].messages.len(), 2);
        assert!(
            calls[0].messages[0]
                .text()
                .starts_with("You are alice, specialized in alice."),
            "{}",
            calls[0].messages[0].text()
        );
        let initial = calls[0].messages[1].text();
        assert!(initial.contains("Query: User: Q"), "{initial}");
        assert!(!initial.contains("Other Agents' Answers"), "{initial}");

        // Phase 2: bob's draft is quoted as a peer, alice's own comes back as
        // hers, and she is never shown to herself as somebody to critique.
        let review = calls[1].messages[1].text();
        assert!(review.contains("[bob]\nanswer from bob"), "{review}");
        assert!(
            review.contains("Your Previous Answer:\nanswer from alice"),
            "{review}"
        );
        assert!(!review.contains("[alice]"), "a member is not its own peer");
    }

    #[tokio::test]
    async fn a_dead_member_is_not_quoted_as_a_peer() {
        // A member that failed this round holds the empty string. Quoting it
        // would put a bare `[down]` header with nothing under it into everyone
        // else's review prompt, which reads as a peer who considered the
        // question and had nothing to say. It is dropped instead.
        let (alice, alice_seen) = StubProvider::new("alice");
        let (synth, _) = StubProvider::new("synth");
        let fusion = FusionProvider::new(
            vec![
                PanelMember {
                    name: "alice".to_string(),
                    provider: alice,
                    model: "m-alice".to_string(),
                },
                PanelMember {
                    name: "down".to_string(),
                    provider: Arc::new(FailingProvider),
                    model: "m-down".to_string(),
                },
            ],
            synth,
            "m-synth".to_string(),
            1,
            "fusion".to_string(),
        )
        .unwrap();
        collect_text(fusion.chat_stream(user_req("Q", false)).await.unwrap())
            .await
            .unwrap();

        let calls = alice_seen.lock().unwrap();
        let review = calls[1].messages[1].text();
        assert!(
            !review.contains("[down]"),
            "a member with no answer is not a peer to critique: {review}"
        );
    }

    #[tokio::test]
    async fn a_wholly_dead_panel_still_injects_the_guidance_shape() {
        // Every member degraded, so there is no non-empty draft to forward. The
        // debate hands back the empty ones rather than nothing, so the synthesis
        // request keeps its shape: the guidance system message is still there,
        // naming the member and carrying an empty body. Drop that fallback and
        // the synthesizer silently gets a differently structured request on
        // exactly the turns where the panel is broken.
        let (synth, synth_seen) = StubProvider::new("synth");
        let fusion = FusionProvider::new(
            vec![PanelMember {
                name: "down".to_string(),
                provider: Arc::new(FailingProvider),
                model: "m".to_string(),
            }],
            synth,
            "m-synth".to_string(),
            1,
            "fusion".to_string(),
        )
        .unwrap();
        collect_text(fusion.chat_stream(user_req("Q", false)).await.unwrap())
            .await
            .unwrap();

        let calls = synth_seen.lock().unwrap();
        let guidance = calls[0]
            .messages
            .iter()
            .find(|m| matches!(m.role, Role::System))
            .map(|m| m.text())
            .expect("a dead panel still injects its (empty-bodied) drafts");
        assert!(guidance.contains("[down]\n\n"), "{guidance}");
    }

    #[tokio::test]
    async fn only_the_refined_answer_reaches_the_synthesizer() {
        let member = ScriptedProvider::new(&[
            "a thin first pass",
            "Critique: the first pass was thin and missed a case.\n\
             Refined Answer: THE REFINED ONE",
        ]);
        let (synth, synth_seen) = StubProvider::new("synth");
        let fusion = FusionProvider::new(
            vec![PanelMember {
                name: "scripted".to_string(),
                provider: member,
                model: "m".to_string(),
            }],
            synth,
            "m-synth".to_string(),
            1,
            "fusion".to_string(),
        )
        .unwrap();
        collect_text(fusion.chat_stream(user_req("Q", false)).await.unwrap())
            .await
            .unwrap();

        let calls = synth_seen.lock().unwrap();
        let guidance = calls[0]
            .messages
            .iter()
            .find(|m| matches!(m.role, Role::System))
            .map(|m| m.text())
            .expect("the panel's drafts are injected as a system message");
        assert!(guidance.contains("THE REFINED ONE"), "{guidance}");
        assert!(
            !guidance.contains("Critique:"),
            "a critique is not an answer: {guidance}"
        );
        assert!(
            !guidance.contains("first pass"),
            "the pre-review draft is superseded: {guidance}"
        );
    }

    #[cfg_attr(not(feature = "provider-ollama"), allow(dead_code))]
    fn provider_config(name: &str, model: &str) -> ProviderConfig {
        ProviderConfig {
            name: name.to_string(),
            // Ollama builds a client without needing a key or a reachable
            // endpoint, which is all these seats have to prove.
            kind: crate::config::ProviderKind::OLLAMA,
            base_url: "http://127.0.0.1:11434".to_string(),
            model: model.to_string(),
            api_key_env: None,
            gguf_path: None,
            usd_per_mtok_in: None,
            usd_per_mtok_out: None,
        }
    }

    /// The seats a panel offers are what lets an `/ultra` roster be dealt
    /// across it instead of running the whole debate once per candidate.
    /// Gated because a seat carries a *live client*, and the fixture's
    /// providers are `kind = "ollama"` — the one client that needs neither a
    /// key nor a reachable endpoint to construct. Without that plugin there is
    /// no such kind and `build()` correctly refuses.
    #[cfg(feature = "provider-ollama")]
    #[test]
    fn panel_seats_name_a_provider_and_its_model() {
        let providers = vec![
            provider_config("alice", "m-alice"),
            provider_config("bob", "m-bob"),
            provider_config("unused", "m-unused"),
        ];
        let fusion = FusionConfig {
            panel: vec!["alice".to_string(), "bob".to_string()],
            synthesizer: "alice".to_string(),
            rounds: 1,
        };

        let seats = panel_seats(&fusion, &providers).expect("seats build");
        assert_eq!(seats.len(), 2, "one seat per panel member, panel order");
        assert_eq!(seats[0].provider.as_deref(), Some("alice"));
        assert_eq!(seats[0].model.as_deref(), Some("m-alice"));
        assert!(seats[0].client.is_some(), "a seat carries a live client");
        assert_eq!(seats[1].provider.as_deref(), Some("bob"));

        let unknown = FusionConfig {
            panel: vec!["nobody".to_string()],
            synthesizer: "alice".to_string(),
            rounds: 1,
        };
        let err = panel_seats(&unknown, &providers)
            .map(|_| ())
            .expect_err("an unknown provider is named, not skipped");
        assert!(format!("{err:#}").contains("nobody"), "{err:#}");
    }

    #[test]
    fn a_provider_listed_twice_in_the_panel_is_rejected() {
        let (a, _) = StubProvider::new("alice");
        let (b, _) = StubProvider::new("alice");
        let (synth, _) = StubProvider::new("synth");
        let err = FusionProvider::new(
            vec![
                PanelMember {
                    name: "alice".to_string(),
                    provider: a,
                    model: "m".to_string(),
                },
                PanelMember {
                    name: "alice".to_string(),
                    provider: b,
                    model: "m".to_string(),
                },
            ],
            synth,
            "m-synth".to_string(),
            1,
            "fusion".to_string(),
        )
        // `FusionProvider` is not `Debug` (it holds trait objects), so the Ok
        // side is discarded before asking for the error.
        .map(|_| ())
        .unwrap_err();
        assert!(err.to_string().contains("alice"), "{err}");
    }

    #[tokio::test]
    async fn every_panel_call_appends_a_run_log_row() {
        // The run log is process-wide (one `~/.wizard` per test binary) and the
        // other tests here append to it concurrently, so this member takes a
        // unique name and only its own rows are read back.
        let name = format!(
            "probe-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let (member, _) = StubProvider::new("probe");
        let (synth, _) = StubProvider::new("synth");
        let fusion = FusionProvider::new(
            vec![PanelMember {
                name: name.clone(),
                provider: member,
                model: "m-probe".to_string(),
            }],
            synth,
            "m-synth".to_string(),
            1,
            "fusion".to_string(),
        )
        .unwrap();
        collect_text(fusion.chat_stream(user_req("Q", false)).await.unwrap())
            .await
            .unwrap();

        let path = Config::wizard_dir().unwrap().join("fusion-runs.jsonl");
        let contents = std::fs::read_to_string(&path).unwrap();
        let rows: Vec<Value> = contents
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|row| row["agent"] == name.as_str())
            .collect();

        assert_eq!(
            rows.len(),
            2,
            "one row per panel call: initial plus review_1"
        );
        assert_eq!(rows[0]["phase"], "initial");
        assert_eq!(rows[1]["phase"], "review_1");
        for row in &rows {
            assert_eq!(
                row["model"], "m-probe",
                "the row names the model actually asked"
            );
            assert!(
                row["request"]["prompt"]
                    .as_str()
                    .unwrap()
                    .contains("User: Q"),
                "{row}"
            );
            assert_eq!(row["response"]["content"], "answer from probe");
        }
    }
}
