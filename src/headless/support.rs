//! Pieces of the headless runner that Claude Code's flags add: which session
//! a run opens, falling back to another model, and checking the final answer
//! against `--json-schema`.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::agent::session::Session;
use crate::agent::{
    Agent, AgentEvent, DoneReason, build_headless_agent, build_headless_agent_for_session, critic,
};
use crate::claude_stream::schema;
use crate::cli::Cli;
use crate::config::Config;

/// The prompt a fallback model starts from: the conversation so far is its
/// context, and the first model never answered.
const FALLBACK_CONTINUE: &str = "The previous model could not finish this turn. Continue the \
     task from where you left off. If it is already complete, summarize what was done.";

/// How many times a final answer that fails `--json-schema` is sent back.
const SCHEMA_RETRIES: usize = 2;

async fn build_for(config: &Config, root: &Path, cli: &Cli) -> Result<Agent> {
    match cli.claude.resume_id.as_deref() {
        Some(id) => {
            let dir = Config::sessions_dir()?;
            let session = Session::open_by_id(&dir, id)?
                .with_context(|| format!("no session with the id '{id}'"))?;
            build_headless_agent_for_session(config, root, session, None).await
        }
        None => build_headless_agent(config, root, cli.resume).await,
    }
}

/// Build the run's agent on the session the flags chose. If the model does
/// not come up and `--fallback-model` names others, the next one is tried.
/// The models still unused come back for the turn loop.
pub(super) async fn open_agent(
    config: &mut Config,
    root: &Path,
    cli: &Cli,
) -> Result<(Agent, VecDeque<String>)> {
    let mut fallbacks: VecDeque<String> = cli.claude.fallback_models().into();
    loop {
        match build_for(config, root, cli).await {
            Ok(agent) => return Ok((agent, fallbacks)),
            Err(err) => match fallbacks.pop_front() {
                Some(model) => {
                    eprintln!(
                        "wizard: {} did not start ({err:#}); trying {model}",
                        config.active().model
                    );
                    crate::cli_claude::set_model(config, &model);
                }
                None => return Err(err),
            },
        }
    }
}

/// How many "LLM unavailable" reports in one turn make a model count as down.
/// The agent retries with a growing wait; with a fallback to go to, waiting out
/// the whole ladder is the slow way to find out.
const DOWN_AFTER: u32 = 2;

/// Counts the agent's "LLM unavailable" reports so a turn can be cut short
/// and handed to a fallback model.
#[derive(Clone, Default)]
pub(super) struct Trip {
    count: Arc<AtomicU32>,
    tripped: Arc<AtomicBool>,
    cancel: Option<crate::agent::CancelHandle>,
}

impl Trip {
    pub(super) fn new(cancel: crate::agent::CancelHandle) -> Self {
        Self {
            cancel: Some(cancel),
            ..Self::default()
        }
    }

    pub(super) fn observe(&self, event: &AgentEvent) {
        let AgentEvent::Error(message) = event else {
            return;
        };
        if !message.starts_with("LLM unavailable") {
            return;
        }
        if self.count.fetch_add(1, Ordering::SeqCst) + 1 >= DOWN_AFTER
            && !self.tripped.swap(true, Ordering::SeqCst)
            && let Some(cancel) = &self.cancel
        {
            cancel.cancel();
        }
    }

    fn take(&self) -> bool {
        self.count.store(0, Ordering::SeqCst);
        self.tripped.swap(false, Ordering::SeqCst)
    }
}

/// Passes events through, counting them for a [`Trip`].
pub(super) struct TripSink {
    inner: Box<dyn crate::output::EventSink>,
    trip: Trip,
}

impl TripSink {
    pub(super) fn new(inner: Box<dyn crate::output::EventSink>, trip: Trip) -> Self {
        Self { inner, trip }
    }
}

impl crate::output::EventSink for TripSink {
    fn start(&mut self) {
        self.inner.start();
    }

    fn event(&mut self, event: AgentEvent) {
        self.trip.observe(&event);
        self.inner.event(event);
    }

    fn finish(&mut self, reason: DoneReason) {
        self.inner.finish(reason);
    }

    fn annotate(&mut self, key: &str, value: Value) {
        self.inner.annotate(key, value);
    }

    fn budget_exceeded(&self) -> bool {
        self.inner.budget_exceeded()
    }
}

/// One turn, and if the model is down, fails outright or trips the circuit
/// breaker, the same task again on each fallback model in turn.
pub(super) async fn run_with_fallback(
    agent: &mut Agent,
    input: &str,
    images: Vec<std::path::PathBuf>,
    tx: &mpsc::Sender<AgentEvent>,
    fallbacks: &mut VecDeque<String>,
    trip: &Trip,
) -> Result<DoneReason> {
    let mut turn = agent.run_turn_with_images(input, images, tx.clone()).await;
    while (trip.take() || matches!(turn, Err(_) | Ok(DoneReason::CircuitBreaker)))
        && let Some(model) = fallbacks.pop_front()
    {
        let native =
            crate::llm::provider::probe_native_tools(agent.client().as_ref(), &model).await;
        agent.set_model(model.clone(), native);
        let _ = tx
            .send(AgentEvent::Notice(format!(
                "the model kept failing; switching to the fallback model {model}"
            )))
            .await;
        turn = agent
            .run_turn_with_images(FALLBACK_CONTINUE, Vec::new(), tx.clone())
            .await;
    }
    turn
}

/// What came of checking the final answer against `--json-schema`.
pub(super) enum SchemaOutcome {
    /// The answer is JSON and matches.
    Valid(Value),
    /// It still did not match after the retries; the problems found.
    Invalid(Vec<String>),
}

/// Check the run's last reply against the schema, and send it back for a fix
/// up to [`SCHEMA_RETRIES`] times when it does not match.
pub(super) async fn enforce_schema(
    agent: &mut Agent,
    schema_text: &str,
    tx: &mpsc::Sender<AgentEvent>,
) -> Result<SchemaOutcome> {
    let schema: Value = serde_json::from_str(schema_text)
        .context("--json-schema is not valid JSON, so the answer cannot be checked")?;
    let mut attempt = 0;
    loop {
        let reply = critic::last_reply(agent.history()).unwrap_or_default();
        let problems = match schema::extract_json(&reply) {
            None => vec!["the final message holds no JSON value".to_string()],
            Some(value) => match schema::validate(&schema, &value) {
                Ok(()) => return Ok(SchemaOutcome::Valid(value)),
                Err(problems) => problems,
            },
        };
        if attempt >= SCHEMA_RETRIES {
            return Ok(SchemaOutcome::Invalid(problems));
        }
        attempt += 1;
        let list: String = problems
            .iter()
            .take(8)
            .map(|problem| format!("- {problem}\n"))
            .collect();
        let prompt = format!(
            "Your final message does not match the required JSON Schema:\n{list}\n\
             Reply again with only the corrected JSON value, no other text."
        );
        agent
            .run_turn_with_images(&prompt, Vec::new(), tx.clone())
            .await?;
    }
}

/// `--no-session-persistence`: drop the session's files once the run is over.
pub(super) fn discard_session(agent: &Agent) {
    if std::env::var_os(crate::cli_claude::env::EPHEMERAL_SESSION).is_none() {
        return;
    }
    let path = agent.session().path();
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(crate::session_flags::name_path(path));
}
