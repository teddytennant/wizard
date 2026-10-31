//! Starting a run: resolve the script, validate it, record it, and run it in
//! the background as a task of the session.
//!
//! A run is a task in the session's [`TaskRegistry`](crate::tools::tasks::TaskRegistry),
//! so the TUI's task list shows it, `TaskStop` stops it, `task_output` reads its
//! progress, and its completion reaches the model the way any background
//! task's does: a note with the tail of the task's output, whose last lines are
//! the result.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::board;
use super::engine::{AgentRunner, Engine, Limits, Progress};
use super::live::{self, Cancel};
use super::script::{self, Parsed};
use super::store::{self, RunRecord, Status};
use super::vm::{self, Resolver};
use crate::agent::AgentEvent;
use crate::tools::ToolContext;
use crate::tools::tasks::{TaskRegistry, TaskStatus};

/// What the `Workflow` tool was asked.
#[derive(Debug, Clone, Default)]
pub struct Input {
    pub script: Option<String>,
    pub name: Option<String>,
    pub script_path: Option<String>,
    pub args: Option<Value>,
    pub resume_from_run_id: Option<String>,
}

/// A script found and read, not yet validated.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub text: String,
    pub parsed: Parsed,
    /// `inline`, `name:<name>` or `path:<file>`.
    pub source: String,
    /// The file the script was read from, for `scriptPath` and named workflows.
    pub file: Option<PathBuf>,
}

fn home() -> Option<PathBuf> {
    dirs::home_dir()
}

fn read_script_file(path: &str, cwd: &Path) -> Result<(PathBuf, String), String> {
    let full = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        cwd.join(path)
    };
    match std::fs::read_to_string(&full) {
        Ok(text) => Ok((full, text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Err(format!("Workflow script file not found: {path}"))
        }
        Err(err) => Err(format!("Failed to read workflow script file {path}: {err}")),
    }
}

/// Find the script the input names. `scriptPath` wins over `name`, which wins
/// over an inline `script`, the order Claude's tool takes them in.
pub fn resolve(input: &Input, cwd: &Path, home: Option<&Path>) -> Result<Resolved, String> {
    if let Some(path) = &input.script_path {
        let (file, text) = read_script_file(path, cwd)?;
        let parsed = script::parse(&text).map_err(invalid_script)?;
        return Ok(Resolved {
            text,
            parsed,
            source: format!("path:{}", file.display()),
            file: Some(file),
        });
    }
    if let Some(name) = &input.name {
        let Some(named) = store::find_named(cwd, home, name) else {
            let (found, _) = store::discover(cwd, home);
            let names: Vec<&str> = found.iter().map(|n| n.name.as_str()).collect();
            return Err(format!(
                "Workflow \"{name}\" not found. Available: {}",
                if names.is_empty() {
                    "(none)".to_string()
                } else {
                    names.join(", ")
                }
            ));
        };
        let text = named
            .read()
            .map_err(|e| format!("Failed to load workflow from {}: {e}", named.path.display()))?;
        let parsed = script::parse(&text).map_err(invalid_script)?;
        return Ok(Resolved {
            text,
            parsed,
            source: format!("name:{}", named.name),
            file: named.bundled.is_none().then_some(named.path),
        });
    }
    if let Some(text) = &input.script {
        let parsed = script::parse(text).map_err(invalid_script)?;
        return Ok(Resolved {
            text: text.clone(),
            parsed,
            source: "inline".to_string(),
            file: None,
        });
    }
    Err("Must provide script, name, or scriptPath".to_string())
}

/// What follows every refusal of a script: Claude's words, naming the skill
/// that carries the reference.
const RETRY_HINT: &str = "\nLoad the `workflow-authoring` skill for the script reference if you have not, fix the script, and retry.";

fn invalid_script(why: String) -> String {
    format!("Invalid workflow script: {why}{RETRY_HINT}")
}

/// Refuse what cannot run: forbidden calls, and a body that does not compile.
pub async fn validate(resolved: &Resolved) -> Result<(), String> {
    if let Some(why) = script::forbidden_in_body(&resolved.parsed.body) {
        return Err(format!("{why}{RETRY_HINT}"));
    }
    vm::check_syntax(&resolved.parsed.body)
        .await
        .map_err(invalid_script)
}

/// `workflow(ref)` for a running script: a name or a `{scriptPath}`.
pub fn child_resolver(cwd: PathBuf) -> Resolver {
    Arc::new(move |reference| {
        let input = Input {
            name: reference
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string),
            script_path: reference
                .get("scriptPath")
                .and_then(Value::as_str)
                .map(str::to_string),
            ..Input::default()
        };
        let resolved = resolve(&input, &cwd, home().as_deref())?;
        if let Some(why) = script::forbidden_in_body(&resolved.parsed.body) {
            return Err(why);
        }
        Ok((resolved.parsed.meta.name.clone(), resolved.parsed.body))
    })
}

/// How a run ended.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub status: Status,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub summary: String,
}

/// A run that has started.
pub struct Launched {
    pub run_id: String,
    pub task_id: u32,
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
    /// The file to edit and pass back as `scriptPath`.
    pub script_path: PathBuf,
    pub done: tokio::task::JoinHandle<Outcome>,
    pub cancel: Arc<Cancel>,
}

/// Writes a run's progress to its task's output, one line each.
struct TaskProgress {
    tasks: Arc<TaskRegistry>,
    task_id: u32,
    started: Instant,
}

impl Progress for TaskProgress {
    fn line(&self, text: &str) {
        let line = format!("[{}] {text}\n", clock(self.started.elapsed()));
        self.tasks.append_output(self.task_id, line.as_bytes());
    }
}

fn clock(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// How much of a result the task's final note carries.
const RESULT_NOTE_BYTES: usize = 1200;

fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

/// Everything a launch needs from the session.
pub struct Deps {
    pub runner: Arc<dyn AgentRunner>,
    pub ctx: ToolContext,
    pub limits: Limits,
}

/// Why a resume is refused, or the journal to resume from.
fn resume_state(id: &str) -> Result<Vec<store::JournalEntry>, String> {
    if !store::valid_run_id(id) {
        return Err(format!("{id:?} is not a workflow run id"));
    }
    if live::is_live(id) {
        let task = store::load(id)
            .ok()
            .and_then(|r| r.task_id)
            .map_or(String::new(), |t| format!(" (task {t})"));
        return Err(format!(
            "Workflow {id} is still running{task}. Stop it first with TaskStop before resuming."
        ));
    }
    let path = store::journal_path(id).map_err(|e| e.to_string())?;
    if !path.exists() {
        return Err(format!(
            "No journal found for workflow run {id}, so there is nothing to resume. To run the workflow from the start, call Workflow again without resumeFromRunId."
        ));
    }
    Ok(store::read_journal(id))
}

/// Start the run in the background.
pub async fn launch(deps: Deps, input: &Input, resolved: Resolved) -> Result<Launched, String> {
    let (run_id, resume) = match &input.resume_from_run_id {
        Some(id) => (id.clone(), Some(resume_state(id)?)),
        None => (store::new_run_id(), None),
    };
    let cwd = deps.ctx.cwd.clone();
    let meta = resolved.parsed.meta.clone();
    let args = input.args.clone().unwrap_or(Value::Null);
    let mut record = RunRecord {
        run_id: run_id.clone(),
        name: meta.name.clone(),
        description: meta.description.clone(),
        status: Status::Running,
        started_ms: store::now_ms(),
        ended_ms: None,
        cwd: cwd.display().to_string(),
        source: resolved.source.clone(),
        args: args.clone(),
        agents: 0,
        cached: 0,
        phases: meta.phases.iter().map(|p| p.title.clone()).collect(),
        result: None,
        error: None,
        failures: Vec::new(),
        pid: std::process::id(),
        task_id: None,
    };
    let dir = store::run_dir(&run_id).map_err(|e| format!("{e:#}"))?;
    let (task_id, kill_rx) = deps
        .ctx
        .tasks
        .add_virtual(format!("workflow {} ({run_id})", meta.name));
    record.task_id = Some(task_id);
    store::create(&record, &resolved.text).map_err(|e| format!("{e:#}"))?;
    if resume.is_some() {
        let _ = std::fs::remove_file(store::journal_path(&run_id).map_err(|e| format!("{e:#}"))?);
    }
    let script_path = match (&resolved.file, &input.script_path) {
        (Some(file), Some(_)) => file.clone(),
        _ => store::script_path(&run_id).map_err(|e| format!("{e:#}"))?,
    };

    if let Some(events) = &deps.ctx.events {
        crate::agent::emit(
            events,
            AgentEvent::TaskStarted {
                id: task_id,
                command: format!("workflow {}", meta.name),
            },
        )
        .await;
    }

    let cancel = Cancel::new();
    live::register(&run_id, Arc::clone(&cancel));
    let started = Instant::now();
    let progress: Arc<dyn Progress> = Arc::new(TaskProgress {
        tasks: Arc::clone(&deps.ctx.tasks),
        task_id,
        started,
    });
    let usage = deps.ctx.usage.clone();
    let baseline = usage.as_ref().map_or(0, |u| u.session_totals().1);
    let mut engine = Engine::new(
        run_id.clone(),
        deps.runner,
        deps.limits,
        progress,
        Arc::clone(&cancel),
    )
    .persist(true)
    .with_spent(Arc::new(move || {
        usage
            .as_ref()
            .map_or(0, |u| u.session_totals().1.saturating_sub(baseline))
    }));
    if let Some(entries) = resume {
        engine = engine.resume_from(entries);
    }
    let engine = Arc::new(engine);

    let tasks = Arc::clone(&deps.ctx.tasks);
    let body = resolved.parsed.body.clone();
    let name = meta.name.clone();
    let description = meta.description.clone();
    let driver_cancel = Arc::clone(&cancel);
    let done = tokio::spawn(async move {
        let killer = {
            let cancel = Arc::clone(&driver_cancel);
            tokio::spawn(async move {
                if kill_rx.await.is_ok() {
                    cancel.cancel();
                }
            })
        };
        let ticker = {
            let engine = Arc::clone(&engine);
            let record = record.clone();
            tokio::spawn(async move {
                let mut record = record;
                let mut ticks = 0u32;
                loop {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    // A stop another process asked for, as a file in the run's directory.
                    if board::stop_requested(&engine.run_id) {
                        engine.cancel.cancel();
                    }
                    let _ = board::write(&engine.run_id, &engine.board());
                    ticks += 1;
                    if ticks.is_multiple_of(2) {
                        snapshot(&mut record, &engine);
                        let _ = store::write_record(&record);
                    }
                }
            })
        };
        let resolver = child_resolver(cwd);
        let result = vm::run(Arc::clone(&engine), resolver, &body, &args).await;
        killer.abort();
        ticker.abort();

        let stopped = driver_cancel.is_cancelled();
        if stopped {
            engine.stop_rows();
        }
        let _ = board::write(&engine.run_id, &engine.board());
        let outcome = finish(&engine, &name, started.elapsed(), result, stopped);
        let mut record = store::load(&engine.run_id).unwrap_or(record);
        snapshot(&mut record, &engine);
        record.status = outcome.status;
        record.ended_ms = Some(store::now_ms());
        record.result = outcome.result.clone();
        record.error = outcome.error.clone();
        let _ = store::write_record(&record);
        if let Some(result) = &outcome.result {
            let _ = std::fs::write(
                store::run_dir(&engine.run_id)
                    .map(|d| d.join("result.json"))
                    .unwrap_or_default(),
                serde_json::to_vec_pretty(result).unwrap_or_default(),
            );
        }
        live::unregister(&engine.run_id);
        tasks.append_output(task_id, outcome.summary.as_bytes());
        tasks.finish_virtual(
            task_id,
            match outcome.status {
                Status::Completed => TaskStatus::Done(0),
                Status::Stopped => TaskStatus::Killed,
                _ => TaskStatus::Done(1),
            },
        );
        outcome
    });

    Ok(Launched {
        run_id,
        task_id,
        name: meta.name,
        description,
        dir,
        script_path,
        done,
        cancel,
    })
}

fn snapshot(record: &mut RunRecord, engine: &Engine) {
    record.agents = engine.calls();
    record.cached = engine.cached();
    record.failures = engine.failures();
    for phase in engine.phases() {
        if !record.phases.contains(&phase) {
            record.phases.push(phase);
        }
    }
}

fn finish(
    engine: &Engine,
    name: &str,
    elapsed: Duration,
    result: Result<Value, String>,
    stopped: bool,
) -> Outcome {
    let agents = engine.calls();
    let cached = engine.cached();
    let counts = format!(
        "{agents} agent{}{}",
        if agents == 1 { "" } else { "s" },
        if cached > 0 {
            format!(" ({cached} cached)")
        } else {
            String::new()
        }
    );
    let failures = engine.failures();
    let took = clock(elapsed);
    if stopped {
        return Outcome {
            status: Status::Stopped,
            result: None,
            error: None,
            summary: format!("\nWorkflow \"{name}\" was stopped after {took}, {counts}.\n"),
        };
    }
    match result {
        Ok(value) => {
            let shown = match &value {
                Value::String(text) => text.clone(),
                other => serde_json::to_string_pretty(other).unwrap_or_default(),
            };
            let full = if shown.len() > RESULT_NOTE_BYTES {
                let path = store::run_dir(&engine.run_id)
                    .map(|d| d.join("result.json"))
                    .unwrap_or_default();
                format!("\n[the full result is in {}]", path.display())
            } else {
                String::new()
            };
            let failed = if failures.is_empty() {
                String::new()
            } else {
                format!(", {} failure(s) (see task output)", failures.len())
            };
            Outcome {
                status: Status::Completed,
                summary: format!(
                    "\nWorkflow \"{name}\" completed in {took}: {counts}{failed}.\nResult:\n{}{full}\n",
                    cut(&shown, RESULT_NOTE_BYTES)
                ),
                result: Some(value),
                error: None,
            }
        }
        Err(error) => Outcome {
            status: Status::Failed,
            result: None,
            summary: format!(
                "\nWorkflow \"{name}\" failed after {took} ({counts}): {}\n",
                cut(&error, RESULT_NOTE_BYTES)
            ),
            error: Some(error),
        },
    }
}

#[cfg(test)]
#[path = "launch_tests.rs"]
mod tests;
