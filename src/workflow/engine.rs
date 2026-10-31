//! The bookkeeping behind `agent()`: the call counter, the cap, the concurrency
//! gate, the resume cache and the journal. Nothing here knows about JavaScript
//! or about models. A script calls [`Engine::begin`] when it calls `agent()`
//! and awaits the [`Pending`] it gets back; an [`AgentRunner`] does the work.
//!
//! Calls are numbered in the order the script makes them, and that numbering is
//! what makes resume work: a script is deterministic, so call `n` of a rerun is
//! the same question as call `n` of the run before.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Semaphore;

use super::board::{self, AgentRow, AgentState, Board};
use super::live::Cancel;
use super::store::{self, JournalEntry};

/// Agents one run may start unless `WIZARD_WORKFLOW_MAX_AGENTS` says otherwise.
/// Claude Code's figure; it is a backstop against a loop that never ends, not a
/// budget.
pub const DEFAULT_MAX_AGENTS: u32 = 1000;

/// Items one `parallel()` or `pipeline()` call takes.
pub const MAX_ITEMS: usize = 4096;

/// How many agents run at once by default: `min(16, max(2, cpus - 2))`.
pub fn default_concurrency() -> usize {
    let cpus = std::thread::available_parallelism().map_or(4, usize::from);
    cpus.saturating_sub(2).clamp(2, 16)
}

/// Limits for one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_agents: u32,
    pub concurrency: usize,
}

impl Limits {
    /// The defaults with the environment applied. `WIZARD_WORKFLOW_MAX_AGENTS`
    /// (1 to 100000) moves the cap. `WIZARD_WORKFLOW_MAX_CONCURRENT_AGENTS` or
    /// Claude's `CLAUDE_CODE_WORKFLOW_MAX_CONCURRENT_AGENTS` (1 to 256) moves the
    /// gate.
    pub fn from_env(get: &dyn Fn(&str) -> Option<String>) -> Self {
        let number = |names: &[&str], min: u64, max: u64| {
            names
                .iter()
                .find_map(|n| get(n))
                .and_then(|v| v.trim().parse::<u64>().ok())
                .filter(|v| (min..=max).contains(v))
        };
        Limits {
            max_agents: number(&["WIZARD_WORKFLOW_MAX_AGENTS"], 1, 100_000)
                .map_or(DEFAULT_MAX_AGENTS, |v| v as u32),
            concurrency: number(
                &[
                    "WIZARD_WORKFLOW_MAX_CONCURRENT_AGENTS",
                    "CLAUDE_CODE_WORKFLOW_MAX_CONCURRENT_AGENTS",
                ],
                1,
                256,
            )
            .map_or_else(default_concurrency, |v| v as usize),
        }
    }
}

/// What a script asked one agent to do.
#[derive(Debug, Clone)]
pub struct AgentRequest {
    /// 1-based position among the run's `agent()` calls.
    pub index: u32,
    pub label: String,
    pub phase: Option<String>,
    pub prompt: String,
    pub schema: Option<Value>,
    pub model: Option<String>,
    pub agent_type: Option<String>,
    pub worktree: bool,
    pub disallowed_tools: Vec<String>,
}

/// How a run of one agent ended, when it ended at all.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentOutcome {
    /// The agent's text, or the schema-shaped object.
    Done(Value),
    /// The agent died; `agent()` gives the script `null`.
    Failed(String),
}

/// Whatever turns an [`AgentRequest`] into an agent run.
#[async_trait]
pub trait AgentRunner: Send + Sync {
    /// `Err` is a mistake in the call itself and throws into the script.
    async fn run(&self, request: AgentRequest) -> Result<AgentOutcome, String>;
}

/// Where a run reports what it is doing.
pub trait Progress: Send + Sync {
    fn line(&self, text: &str);
}

/// A progress sink that drops everything.
pub struct Quiet;

impl Progress for Quiet {
    fn line(&self, _: &str) {}
}

/// What `agent()` resolves to once the call has been numbered.
pub enum Pending {
    Cached(Value),
    Live(Box<LiveCall>),
    Refused(String),
}

pub struct LiveCall {
    request: AgentRequest,
    key: String,
}

#[derive(Default)]
struct Cache {
    entries: HashMap<u32, JournalEntry>,
    broken: bool,
}

pub struct Engine {
    pub run_id: String,
    runner: Arc<dyn AgentRunner>,
    pub limits: Limits,
    gate: Arc<Semaphore>,
    calls: AtomicU32,
    cached: AtomicU32,
    cache: Mutex<Cache>,
    pub cancel: Arc<Cancel>,
    progress: Arc<dyn Progress>,
    phases: Mutex<Vec<String>>,
    failures: Mutex<Vec<String>>,
    rows: Mutex<Vec<AgentRow>>,
    persist: AtomicBool,
    spent: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl Engine {
    pub fn new(
        run_id: impl Into<String>,
        runner: Arc<dyn AgentRunner>,
        limits: Limits,
        progress: Arc<dyn Progress>,
        cancel: Arc<Cancel>,
    ) -> Self {
        Engine {
            run_id: run_id.into(),
            runner,
            gate: Arc::new(Semaphore::new(limits.concurrency)),
            limits,
            calls: AtomicU32::new(0),
            cached: AtomicU32::new(0),
            cache: Mutex::new(Cache::default()),
            cancel,
            progress,
            phases: Mutex::new(Vec::new()),
            failures: Mutex::new(Vec::new()),
            rows: Mutex::new(Vec::new()),
            persist: AtomicBool::new(false),
            spent: Arc::new(|| 0),
        }
    }

    /// Write each finished call to the run's journal.
    pub fn persist(self, on: bool) -> Self {
        self.persist.store(on, Ordering::Relaxed);
        self
    }

    pub fn with_spent(mut self, spent: Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
        self.spent = spent;
        self
    }

    /// Calls from an earlier run to answer from. A call is answered from the
    /// journal only while every call before it was; the first one that is not
    /// (changed prompt, missing, failed) and everything after it runs live.
    pub fn resume_from(self, journal: Vec<JournalEntry>) -> Self {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        for entry in journal {
            // A later line for the same call (a retry) replaces the earlier.
            cache.entries.insert(entry.i, entry);
        }
        drop(cache);
        self
    }

    pub fn spent(&self) -> u64 {
        (self.spent)()
    }

    pub fn calls(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }

    pub fn cached(&self) -> u32 {
        self.cached.load(Ordering::SeqCst)
    }

    pub fn failures(&self) -> Vec<String> {
        self.failures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn phases(&self) -> Vec<String> {
        self.phases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Every agent called so far and its state, for the panel.
    pub fn board(&self) -> Board {
        Board {
            phases: self.phases(),
            agents: self.rows.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        }
    }

    fn row(&self, index: u32, change: impl FnOnce(&mut AgentRow)) {
        let mut rows = self.rows.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(row) = rows.iter_mut().find(|row| row.i == index) {
            change(row);
        }
    }

    /// The run was stopped: whatever had not finished did not.
    pub fn stop_rows(&self) {
        let now = store::now_ms();
        for row in self
            .rows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter_mut()
        {
            if !row.state.finished() {
                row.state = AgentState::Stopped;
                row.ended_ms = Some(now);
            }
        }
    }

    pub fn log(&self, text: &str) {
        self.progress.line(text);
    }

    pub fn phase(&self, title: &str) {
        let mut phases = self.phases.lock().unwrap_or_else(|e| e.into_inner());
        if !phases.iter().any(|p| p == title) {
            phases.push(title.to_string());
        }
        drop(phases);
        self.progress.line(&format!("phase: {title}"));
    }

    pub fn fail(&self, message: &str) {
        self.failures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(message.to_string());
        self.progress.line(&format!("failed: {message}"));
    }

    /// Number a call, check the cap and the cache. Synchronous on purpose: it
    /// runs when the script calls `agent()`, so the numbering is the script's
    /// call order and not the order the calls happen to get scheduled in.
    pub fn begin(&self, mut request: AgentRequest) -> Pending {
        let index = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if index > self.limits.max_agents {
            return Pending::Refused(format!(
                "Workflow agent() call cap reached ({}). This usually means a loop using budget.remaining() never terminates because the budget is unset. Add a hard iteration cap to the loop. WIZARD_WORKFLOW_MAX_AGENTS raises the cap.",
                self.limits.max_agents
            ));
        }
        request.index = index;
        self.rows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(AgentRow {
                i: index,
                label: request.label.clone(),
                phase: request.phase.clone(),
                state: AgentState::Queued,
                agent: request
                    .agent_type
                    .clone()
                    .unwrap_or_else(|| "worker".into()),
                started_ms: None,
                ended_ms: None,
                prompt: board::kept(&request.prompt),
                outcome: None,
                error: None,
            });
        let key = store::call_key(
            &request.prompt,
            &json!({
                "schema": request.schema,
                "model": request.model,
                "agentType": request.agent_type,
                "worktree": request.worktree,
                "disallowedTools": request.disallowed_tools,
            }),
        );
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if !cache.broken && !cache.entries.is_empty() {
            match cache.entries.get(&index) {
                Some(entry) if entry.ok && entry.key == key => {
                    self.cached.fetch_add(1, Ordering::SeqCst);
                    self.progress
                        .line(&format!("agent {index} [{}] cached", request.label));
                    let replay = entry.clone();
                    drop(cache);
                    let outcome = board::kept(&outcome_text(&replay.result));
                    self.row(index, |row| {
                        row.state = AgentState::Cached;
                        row.outcome = Some(outcome);
                    });
                    // Kept in the new journal so a second resume still has it.
                    self.record(&replay);
                    return Pending::Cached(replay.result);
                }
                _ => cache.broken = true,
            }
        }
        drop(cache);
        Pending::Live(Box::new(LiveCall { request, key }))
    }

    /// Wait for the call to finish.
    pub async fn finish(&self, pending: Pending) -> Result<Value, String> {
        let call = match pending {
            Pending::Cached(value) => return Ok(value),
            Pending::Refused(why) => return Err(why),
            Pending::Live(call) => *call,
        };
        let LiveCall { request, key } = call;
        let aborted = || "Workflow aborted".to_string();
        let permit = tokio::select! {
            biased;
            () = self.cancel.cancelled() => return Err(aborted()),
            permit = Arc::clone(&self.gate).acquire_owned() => permit.map_err(|_| aborted())?,
        };
        self.row(request.index, |row| {
            row.state = AgentState::Running;
            row.started_ms = Some(store::now_ms());
        });
        self.progress.line(&format!(
            "agent {} [{}] started{}",
            request.index,
            request.label,
            request
                .phase
                .as_deref()
                .map_or(String::new(), |p| format!(" in {p}"))
        ));
        let label = request.label.clone();
        let phase = request.phase.clone();
        let index = request.index;
        let outcome = tokio::select! {
            biased;
            () = self.cancel.cancelled() => return Err(aborted()),
            outcome = self.runner.run(request) => outcome,
        };
        drop(permit);
        let entry = |ok: bool, result: Value, error: Option<String>| JournalEntry {
            i: index,
            key: key.clone(),
            label: label.clone(),
            phase: phase.clone(),
            ok,
            result,
            error,
        };
        match outcome? {
            AgentOutcome::Done(value) => {
                let text = board::kept(&outcome_text(&value));
                self.row(index, |row| {
                    row.state = AgentState::Done;
                    row.ended_ms = Some(store::now_ms());
                    row.outcome = Some(text);
                });
                self.progress.line(&format!("agent {index} [{label}] done"));
                self.record(&entry(true, value.clone(), None));
                Ok(value)
            }
            AgentOutcome::Failed(why) => {
                self.row(index, |row| {
                    row.state = AgentState::Failed;
                    row.ended_ms = Some(store::now_ms());
                    row.error = Some(board::kept(&why));
                });
                self.fail(&format!("agent {index} [{label}]: {why}"));
                self.record(&entry(false, Value::Null, Some(why)));
                Ok(Value::Null)
            }
        }
    }

    fn record(&self, entry: &JournalEntry) {
        if !self.persist.load(Ordering::Relaxed) {
            return;
        }
        if let Err(err) = store::append_journal(&self.run_id, entry) {
            tracing::warn!("workflow journal write failed: {err:#}");
        }
    }
}

/// An agent's result as the panel shows it: text as it is, anything else as JSON.
fn outcome_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    use super::*;

    /// Answers every agent with `answer:<prompt>` and counts how many ran at once.
    struct Echo {
        running: AtomicUsize,
        peak: AtomicUsize,
        delay: Duration,
        seen: Mutex<Vec<u32>>,
    }

    impl Echo {
        fn new(delay: Duration) -> Arc<Self> {
            Arc::new(Echo {
                running: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                delay,
                seen: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl AgentRunner for Echo {
        async fn run(&self, request: AgentRequest) -> Result<AgentOutcome, String> {
            let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            self.seen.lock().unwrap().push(request.index);
            tokio::time::sleep(self.delay).await;
            self.running.fetch_sub(1, Ordering::SeqCst);
            if request.prompt.contains("die") {
                return Ok(AgentOutcome::Failed("api error".into()));
            }
            Ok(AgentOutcome::Done(json!(format!(
                "answer:{}",
                request.prompt
            ))))
        }
    }

    fn request(prompt: &str) -> AgentRequest {
        AgentRequest {
            index: 0,
            label: prompt.to_string(),
            phase: None,
            prompt: prompt.to_string(),
            schema: None,
            model: None,
            agent_type: None,
            worktree: false,
            disallowed_tools: vec![],
        }
    }

    fn engine(runner: Arc<dyn AgentRunner>, limits: Limits) -> Engine {
        Engine::new(
            "wf_test0000-000",
            runner,
            limits,
            Arc::new(Quiet),
            Cancel::new(),
        )
    }

    fn limits(max_agents: u32, concurrency: usize) -> Limits {
        Limits {
            max_agents,
            concurrency,
        }
    }

    #[tokio::test]
    async fn calls_are_numbered_in_the_order_they_begin() {
        let echo = Echo::new(Duration::from_millis(5));
        let engine = engine(echo.clone(), limits(100, 8));
        let a = engine.begin(request("a"));
        let b = engine.begin(request("b"));
        let (b, a) = (engine.finish(b).await, engine.finish(a).await);
        assert_eq!(a.unwrap(), json!("answer:a"));
        assert_eq!(b.unwrap(), json!("answer:b"));
        assert_eq!(engine.calls(), 2);
    }

    #[tokio::test]
    async fn the_gate_holds_concurrency_to_the_limit() {
        let echo = Echo::new(Duration::from_millis(20));
        let engine = Arc::new(engine(echo.clone(), limits(100, 3)));
        let mut tasks = Vec::new();
        for n in 0..10 {
            let pending = engine.begin(request(&format!("p{n}")));
            let engine = Arc::clone(&engine);
            tasks.push(tokio::spawn(async move { engine.finish(pending).await }));
        }
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        assert_eq!(echo.peak.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn the_cap_refuses_the_call_past_it() {
        let engine = engine(Echo::new(Duration::ZERO), limits(2, 4));
        for _ in 0..2 {
            let p = engine.begin(request("x"));
            engine.finish(p).await.unwrap();
        }
        let third = engine.begin(request("x"));
        let err = engine.finish(third).await.unwrap_err();
        assert!(err.contains("agent() call cap reached (2)"), "{err}");
        assert!(err.contains("WIZARD_WORKFLOW_MAX_AGENTS"));
    }

    #[tokio::test]
    async fn a_failed_agent_gives_null_and_is_recorded() {
        let engine = engine(Echo::new(Duration::ZERO), limits(10, 2));
        let p = engine.begin(request("please die"));
        assert_eq!(engine.finish(p).await.unwrap(), Value::Null);
        assert!(engine.failures()[0].contains("api error"));
    }

    fn entry(i: u32, prompt: &str, ok: bool) -> JournalEntry {
        let key = store::call_key(
            prompt,
            &json!({"schema": null, "model": null, "agentType": null, "worktree": false, "disallowedTools": []}),
        );
        JournalEntry {
            i,
            key,
            label: prompt.into(),
            phase: None,
            ok,
            result: json!(format!("cached:{prompt}")),
            error: None,
        }
    }

    #[tokio::test]
    async fn resume_answers_the_unchanged_prefix_and_reruns_from_the_first_change() {
        let echo = Echo::new(Duration::ZERO);
        let journal = vec![
            entry(1, "a", true),
            entry(2, "b", true),
            entry(3, "c", true),
        ];
        let engine = engine(echo.clone(), limits(10, 2)).resume_from(journal);
        // 1 matches, 2 was edited, 3 is unchanged but follows a change.
        for (prompt, want) in [
            ("a", "cached:a"),
            ("b-edited", "answer:b-edited"),
            ("c", "answer:c"),
        ] {
            let p = engine.begin(request(prompt));
            assert_eq!(engine.finish(p).await.unwrap(), json!(want), "{prompt}");
        }
        assert_eq!(engine.cached(), 1);
        assert_eq!(*echo.seen.lock().unwrap(), vec![2, 3]);
    }

    #[tokio::test]
    async fn a_failed_journal_entry_reruns_with_everything_after_it() {
        let echo = Echo::new(Duration::ZERO);
        let journal = vec![
            entry(1, "a", true),
            entry(2, "b", false),
            entry(3, "c", true),
        ];
        let engine = engine(echo.clone(), limits(10, 2)).resume_from(journal);
        for prompt in ["a", "b", "c"] {
            let p = engine.begin(request(prompt));
            engine.finish(p).await.unwrap();
        }
        assert_eq!(engine.cached(), 1);
        assert_eq!(*echo.seen.lock().unwrap(), vec![2, 3]);
    }

    #[tokio::test]
    async fn an_agent_missing_from_the_journal_reruns_with_everything_after_it() {
        let echo = Echo::new(Duration::ZERO);
        let journal = vec![entry(1, "a", true), entry(3, "c", true)];
        let engine = engine(echo.clone(), limits(10, 2)).resume_from(journal);
        for prompt in ["a", "b", "c"] {
            let p = engine.begin(request(prompt));
            engine.finish(p).await.unwrap();
        }
        assert_eq!(*echo.seen.lock().unwrap(), vec![2, 3]);
    }

    #[tokio::test]
    async fn a_stop_ends_a_waiting_call() {
        let echo = Echo::new(Duration::from_secs(60));
        let engine = Arc::new(engine(echo, limits(10, 1)));
        let p = engine.begin(request("slow"));
        let waiter = {
            let engine = Arc::clone(&engine);
            tokio::spawn(async move { engine.finish(p).await })
        };
        tokio::time::sleep(Duration::from_millis(30)).await;
        engine.cancel.cancel();
        let err = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("woke")
            .unwrap()
            .unwrap_err();
        assert_eq!(err, "Workflow aborted");
    }

    #[test]
    fn limits_read_the_environment_and_ignore_nonsense() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.to_string())
            }
        };
        let defaults = Limits::from_env(&env(&[]));
        assert_eq!(defaults.max_agents, 1000);
        assert!((2..=16).contains(&defaults.concurrency));
        let set = Limits::from_env(&env(&[
            ("WIZARD_WORKFLOW_MAX_AGENTS", "40"),
            ("CLAUDE_CODE_WORKFLOW_MAX_CONCURRENT_AGENTS", "5"),
        ]));
        assert_eq!((set.max_agents, set.concurrency), (40, 5));
        let bad = Limits::from_env(&env(&[
            ("WIZARD_WORKFLOW_MAX_AGENTS", "0"),
            ("WIZARD_WORKFLOW_MAX_CONCURRENT_AGENTS", "9999"),
        ]));
        assert_eq!(bad.max_agents, 1000);
        assert_eq!(bad.concurrency, defaults.concurrency);
    }
}
