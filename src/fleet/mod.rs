//! `wizard fleet` — parallel sovereign workers over git worktrees.
//!
//! `fleet run -n <N> -p "<mission>"` runs one in-process planning turn that
//! decomposes the mission into independent tasks, then spawns up to N
//! headless `wizard --mode sovereign` children, each in its own git worktree
//! on its own `fleet/<i>-<slug>` branch. The coordinator claims tasks for
//! workers (atomic rename from `queue/` into `claimed/`), supervises the
//! children (watchdog, heartbeat, stop sentinel, ctrl-c), records one result
//! JSON per task, and finally runs an in-process synthesis turn that merges
//! the fleet branches back into the current branch.
//!
//! Project-local layout (rooted at the current directory):
//! - `.wizard/fleet/fleet.toml` — mission, worker count, status, child pids
//! - `.wizard/fleet/queue/<id>.json` — tasks not yet claimed
//! - `.wizard/fleet/claimed/<id>.json` — tasks claimed by a worker slot
//! - `.wizard/fleet/results/<id>.json` — one result per finished task
//! - `.wizard/fleet/worktrees/<i>` — per-slot git worktree (removed at end)
//! - `.wizard/fleet/logs/<id>.stdout|stderr` — raw child output
//! - `.wizard/fleet/heartbeat` — unix timestamp, touched every tick
//! - `.wizard/fleet/stop` — sentinel written by `fleet stop`

use std::collections::HashSet;
use std::fmt::Write as _;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use indicatif::{MultiProgress, ProgressBar};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::agent::{Agent, AgentEvent, PlanVerdict, build_headless_agent};
use crate::cli::FleetCmd;
use crate::config::Config;
use crate::git_util as git;

/// Supervision tick: reap children, enforce the watchdog, claim tasks,
/// check the stop sentinel, touch the heartbeat.
const TICK: Duration = Duration::from_secs(1);

/// Max length of the mission slug used in branch names.
const SLUG_MAX: usize = 24;

/// Max length of a sanitized task id (it becomes a file and result name).
const ID_MAX: usize = 40;

// ---------------------------------------------------------------------------
// On-disk layout
// ---------------------------------------------------------------------------

/// Paths of one project's fleet state, rooted at `<project>/.wizard/fleet`.
#[derive(Debug, Clone)]
pub struct FleetDirs {
    root: PathBuf,
}

impl FleetDirs {
    pub fn new(project_root: &Path) -> Self {
        Self {
            root: project_root.join(".wizard").join("fleet"),
        }
    }

    pub fn queue(&self) -> PathBuf {
        self.root.join("queue")
    }

    pub fn claimed(&self) -> PathBuf {
        self.root.join("claimed")
    }

    pub fn results(&self) -> PathBuf {
        self.root.join("results")
    }

    pub fn worktrees(&self) -> PathBuf {
        self.root.join("worktrees")
    }

    pub fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }

    pub fn state_path(&self) -> PathBuf {
        self.root.join("fleet.toml")
    }

    pub fn stop_path(&self) -> PathBuf {
        self.root.join("stop")
    }

    pub fn heartbeat_path(&self) -> PathBuf {
        self.root.join("heartbeat")
    }

    /// Create the whole directory tree (idempotent).
    pub fn ensure(&self) -> Result<()> {
        for dir in [
            self.root.clone(),
            self.queue(),
            self.claimed(),
            self.results(),
            self.worktrees(),
            self.logs(),
        ] {
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        Ok(())
    }

    /// True when `fleet stop` has requested a wind-down.
    pub fn stop_requested(&self) -> bool {
        self.stop_path().exists()
    }
}

/// One unit of work produced by the planning turn (`queue/<id>.json`).
/// Deserialization is liberal: only `prompt` is required; missing ids and
/// titles are filled in by [`finalize_tasks`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetTask {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    pub prompt: String,
    #[serde(default)]
    pub files_hint: Vec<String>,
}

/// Contents of `fleet.toml` — the coordinator's persisted state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetState {
    /// The mission text handed to `fleet run -p`.
    pub mission: String,
    /// Requested worker count (`-n`).
    pub workers: usize,
    /// RFC 3339 start time.
    pub started: String,
    /// planning | running | synthesizing | done | stopped
    pub status: String,
    /// Pids of currently running children (empty once the fleet ends).
    #[serde(default)]
    pub pids: Vec<u32>,
}

/// One finished task (`results/<id>.json`): exit code, branch, and the
/// child's parsed `--output-format json` summary when stdout parsed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskResult {
    pub task_id: String,
    pub title: String,
    pub branch: String,
    /// Child exit code; `None` on signal death or a watchdog/stop kill.
    pub exit: Option<i32>,
    /// True when the watchdog killed the child past `[fleet] max_minutes`.
    #[serde(default)]
    pub timed_out: bool,
    /// The child's final JSON summary object, when stdout parsed as JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<Value>,
}

/// Save `fleet.toml`, creating parents as needed.
pub fn save_state(path: &Path, state: &FleetState) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(state).context("serializing fleet state")?;
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

/// Load `fleet.toml`; `None` when no fleet has ever run here.
pub fn load_state(path: &Path) -> Result<Option<FleetState>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).context(format!("reading {}", path.display())),
    };
    toml::from_str(&text)
        .map(Some)
        .with_context(|| format!("parsing {}", path.display()))
}

// ---------------------------------------------------------------------------
// Pure helpers: slugs, ids, task-list parsing
// ---------------------------------------------------------------------------

/// Lowercase kebab-case of `text`, capped at `max_len`: runs of
/// non-alphanumerics collapse into single dashes.
fn kebab(text: &str, max_len: usize) -> String {
    let mut out = String::new();
    for word in text
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        let sep = usize::from(!out.is_empty());
        if out.len() + sep + word.len() > max_len {
            // A long first word still yields something usable.
            if out.is_empty() {
                out.push_str(&word[..max_len]);
            }
            break;
        }
        if sep == 1 {
            out.push('-');
        }
        out.push_str(word);
    }
    out
}

/// Short branch-name slug derived from the mission text (`fleet` when the
/// mission has no usable characters).
pub fn slug(mission: &str) -> String {
    let s = kebab(mission, SLUG_MAX);
    if s.is_empty() { "fleet".to_string() } else { s }
}

/// Path-safe task id (may come back empty; [`finalize_tasks`] falls back).
fn sanitize_id(raw: &str) -> String {
    kebab(raw, ID_MAX)
}

/// Length (in bytes) of the balanced JSON value starting at the first byte
/// of `s` (which must be `[` or `{`), honoring strings and escapes. `None`
/// when the brackets never balance.
fn balanced_end(s: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escape = false;
    for (i, c) in s.char_indices() {
        if in_str {
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '[' | '{' => depth += 1,
            ']' | '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(i + c.len_utf8());
                }
            }
            _ => {}
        }
    }
    None
}

/// Interpret one parsed JSON value as a task list: an array of task
/// objects, an object with a `tasks` array, or a single task object.
fn tasks_from_value(value: Value) -> Result<Vec<FleetTask>> {
    let items = match value {
        Value::Array(items) => items,
        Value::Object(mut map) => match map.remove("tasks") {
            Some(Value::Array(items)) => items,
            Some(_) => bail!("\"tasks\" is not an array"),
            None => vec![Value::Object(map)],
        },
        _ => bail!("expected a JSON array of task objects"),
    };
    let tasks: Vec<FleetTask> = items
        .into_iter()
        .map(serde_json::from_value)
        .collect::<Result<_, _>>()
        .context("every task object needs at least a \"prompt\" string")?;
    if tasks.is_empty() {
        bail!("the task list is empty");
    }
    Ok(tasks)
}

/// Liberal task-list extraction from a model reply: scan for every balanced
/// JSON array/object (so fenced or prose-wrapped JSON works), return the
/// first one that yields a non-empty task list.
pub fn parse_tasks(text: &str) -> Result<Vec<FleetTask>> {
    let mut last_err: Option<anyhow::Error> = None;
    for (i, c) in text.char_indices() {
        if c != '[' && c != '{' {
            continue;
        }
        let Some(len) = balanced_end(&text[i..]) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text[i..i + len]) else {
            continue;
        };
        match tasks_from_value(value) {
            Ok(tasks) => return Ok(tasks),
            Err(err) => last_err = Some(err),
        }
    }
    Err(last_err
        .unwrap_or_else(|| anyhow::anyhow!("no JSON task list found in the planning response")))
}

/// Normalize a parsed task list: drop empty prompts, sanitize and
/// de-duplicate ids (deriving them from titles when absent), backfill
/// titles, and cap at `max_tasks`.
pub fn finalize_tasks(tasks: Vec<FleetTask>, max_tasks: usize) -> Result<Vec<FleetTask>> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for (i, mut task) in tasks.into_iter().enumerate() {
        if task.prompt.trim().is_empty() {
            continue;
        }
        let mut id = sanitize_id(&task.id);
        if id.is_empty() {
            id = sanitize_id(&task.title);
        }
        if id.is_empty() {
            id = format!("task-{}", i + 1);
        }
        let mut unique = id.clone();
        let mut suffix = 2;
        while !seen.insert(unique.clone()) {
            unique = format!("{id}-{suffix}");
            suffix += 1;
        }
        task.id = unique;
        if task.title.trim().is_empty() {
            task.title = task.id.clone();
        }
        out.push(task);
        if out.len() == max_tasks {
            break;
        }
    }
    if out.is_empty() {
        bail!("mission decomposition produced no usable tasks");
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Prompts
// ---------------------------------------------------------------------------

/// The planning turn's prompt: decompose the mission into independent tasks
/// and answer with strict JSON.
pub fn plan_prompt(mission: &str, max_tasks: usize) -> String {
    format!(
        "You are the coordinator of a fleet of autonomous agents that will work in \
         parallel, each in its own isolated git worktree of this repository.\n\n\
         Mission:\n\n{mission}\n\n\
         Decompose the mission into at most {max_tasks} INDEPENDENT tasks. The tasks run \
         concurrently with no shared state, so no task may depend on another task's \
         output, and two tasks should not edit the same files. Prefer fewer, \
         well-scoped tasks over many overlapping ones.\n\n\
         Respond with ONLY a JSON array — no prose, no code fence — one object per task:\n\
         [{{\"id\": \"short-kebab-case-id\", \"title\": \"one line\", \
         \"prompt\": \"full self-contained instructions for the worker\", \
         \"files_hint\": [\"paths/likely/touched\"]}}]"
    )
}

/// Follow-up prompt when the first planning reply did not parse.
pub fn retry_prompt(error: &str) -> String {
    format!(
        "Your previous reply could not be parsed as a task list ({error}). Reply again \
         with ONLY the JSON array of task objects — no prose, no code fence, no keys \
         other than id, title, prompt, files_hint."
    )
}

/// Wrap a task prompt with the worker's standing instructions.
pub fn worker_prompt(task: &FleetTask) -> String {
    let mut p = format!(
        "You are one worker in a fleet of agents working in parallel toward a larger \
         mission. Your task:\n\n{}\n",
        task.prompt
    );
    if !task.files_hint.is_empty() {
        let _ = writeln!(p, "\nLikely relevant files: {}", task.files_hint.join(", "));
    }
    p.push_str(
        "\nWhen the task is done, commit your changes with a descriptive message \
         (git add the files you touched, then git commit). Do not push. Never commit \
         anything under .wizard/.",
    );
    p
}

/// The synthesis turn's prompt: results + branch list + merge instructions.
/// Runs in the MAIN repository checkout, never a worktree.
pub fn synthesis_prompt(mission: &str, results: &[TaskResult]) -> String {
    let mut p = format!(
        "You are the fleet coordinator. The mission was:\n\n{mission}\n\n\
         {} worker task(s) ran in isolated git worktrees; each committed its work to \
         its own branch. Results:\n\n",
        results.len()
    );
    for result in results {
        let outcome = describe_exit(result);
        let summary = result
            .summary
            .as_ref()
            .and_then(|value| value["result"].as_str())
            .map(|text| truncate_chars(text, 600))
            .unwrap_or_else(|| "(no summary)".to_string());
        let _ = writeln!(
            p,
            "- task '{}' ({}) — branch {}, {outcome}\n  {summary}",
            result.task_id, result.title, result.branch
        );
    }
    p.push_str(
        "\nMerge each branch listed above into the CURRENT branch, one at a time \
         (`git merge <branch>` — you are in the main checkout of the repository, not a \
         worktree). If a merge conflicts and the resolution is trivial, resolve it and \
         complete the merge; otherwise run `git merge --abort`, leave that branch \
         unmerged, and move on to the next one. Never force anything, never rewrite \
         history, never delete branches. Finish with a short report: which branches \
         merged cleanly and which were left unmerged and why.",
    );
    p
}

/// "exit 0" / "killed (timeout)" / "killed" for one task result.
fn describe_exit(result: &TaskResult) -> String {
    match result.exit {
        Some(code) => format!("exit {code}"),
        None if result.timed_out => "killed (timeout)".to_string(),
        None => "killed".to_string(),
    }
}

/// First `max` characters of `text` (with an ellipsis when truncated).
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let mut s: String = text.chars().take(max).collect();
        s.push('…');
        s
    }
}

// ---------------------------------------------------------------------------
// Workers: argv, claiming, spawning
// ---------------------------------------------------------------------------

/// Argv (after the binary itself) of one worker child: a headless sovereign
/// run in the slot's worktree emitting one final JSON summary on stdout.
pub fn worker_args(prompt: &str, worktree: &Path) -> Vec<String> {
    vec![
        "--mode".to_string(),
        "sovereign".to_string(),
        "-p".to_string(),
        prompt.to_string(),
        "--cwd".to_string(),
        worktree.display().to_string(),
        "--output-format".to_string(),
        "json".to_string(),
    ]
}

/// Atomically claim the next queued task: rename `queue/<id>.json` into
/// `claimed/` (rename is atomic within a filesystem, so when two claimants
/// race, exactly one wins; the loser sees `NotFound` and moves on). `None`
/// when the queue is empty.
pub fn claim_next(queue: &Path, claimed: &Path) -> Result<Option<FleetTask>> {
    let mut names: Vec<_> = match std::fs::read_dir(queue) {
        Ok(entries) => entries
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.file_name())
            .filter(|name| name.to_string_lossy().ends_with(".json"))
            .collect(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).context(format!("reading {}", queue.display())),
    };
    names.sort();
    for name in names {
        let from = queue.join(&name);
        let to = claimed.join(&name);
        match std::fs::rename(&from, &to) {
            Ok(()) => {
                let raw = std::fs::read_to_string(&to)
                    .with_context(|| format!("reading {}", to.display()))?;
                let task: FleetTask = serde_json::from_str(&raw)
                    .with_context(|| format!("parsing {}", to.display()))?;
                return Ok(Some(task));
            }
            // Another claimant renamed it first — try the next one.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err).context(format!("claiming {}", from.display()));
            }
        }
    }
    Ok(None)
}

/// True when no `.json` task files remain in `queue` (a missing dir counts
/// as empty).
pub fn queue_is_empty(queue: &Path) -> Result<bool> {
    match std::fs::read_dir(queue) {
        Ok(mut entries) => Ok(!entries
            .any(|entry| entry.is_ok_and(|e| e.file_name().to_string_lossy().ends_with(".json")))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(err) => Err(err).context(format!("reading {}", queue.display())),
    }
}

// ---------------------------------------------------------------------------
// Supervisor tick (pure) + the real loop around it
// ---------------------------------------------------------------------------

/// Observed state of one worker slot at the top of a supervision tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotStatus {
    /// No child running.
    Idle,
    /// Child running and within its time budget.
    Running,
    /// Child has exited and awaits reaping.
    Exited,
    /// Child running past `[fleet] max_minutes` — kill it.
    Overdue,
}

/// What the supervision loop should do this tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickAction {
    /// Stop sentinel present: kill every child and mark the fleet stopped.
    StopAll,
    /// Record this slot's exited child as a result and free the slot.
    Reap(usize),
    /// Kill this slot's overdue child (recorded as timed out).
    Kill(usize),
    /// Claim the next queued task and spawn a child in this slot.
    Spawn(usize),
    /// Queue empty and every slot idle: the fleet is finished.
    AllDone,
}

/// Pure tick logic: given the slot states, queue emptiness, and the stop
/// sentinel, decide the actions. The real loop feeds in observations and
/// executes whatever comes back; tests drive this directly.
pub fn tick(slots: &[SlotStatus], queue_empty: bool, stop_requested: bool) -> Vec<TickAction> {
    if stop_requested {
        return vec![TickAction::StopAll];
    }
    let mut actions = Vec::new();
    for (i, status) in slots.iter().enumerate() {
        match status {
            SlotStatus::Exited => actions.push(TickAction::Reap(i)),
            SlotStatus::Overdue => actions.push(TickAction::Kill(i)),
            SlotStatus::Idle if !queue_empty => actions.push(TickAction::Spawn(i)),
            SlotStatus::Idle | SlotStatus::Running => {}
        }
    }
    if actions.is_empty() && queue_empty && slots.iter().all(|s| *s == SlotStatus::Idle) {
        actions.push(TickAction::AllDone);
    }
    actions
}

/// One worker slot: a reusable worktree + branch, optionally running a child.
struct Slot {
    worktree: PathBuf,
    branch: String,
    running: Option<Worker>,
}

/// A spawned worker child and what it is working on.
struct Worker {
    task: FleetTask,
    child: tokio::process::Child,
    started: Instant,
    stdout_path: PathBuf,
    /// Exit code observed by `try_wait` (set when status is `Exited`).
    exit: Option<i32>,
}

/// Spawn one worker child in `worktree`: direct argv (no shell), cwd set
/// both ways, `WIZARD_FLEET=1`, stdout/stderr captured to log files,
/// `kill_on_drop` as the teardown backstop.
fn spawn_worker(task: FleetTask, worktree: &Path, dirs: &FleetDirs) -> Result<Worker> {
    let exe = std::env::current_exe().context("locating the wizard binary for the worker")?;
    let stdout_path = dirs.logs().join(format!("{}.stdout", task.id));
    let stderr_path = dirs.logs().join(format!("{}.stderr", task.id));
    let stdout = std::fs::File::create(&stdout_path)
        .with_context(|| format!("creating {}", stdout_path.display()))?;
    let stderr = std::fs::File::create(&stderr_path)
        .with_context(|| format!("creating {}", stderr_path.display()))?;
    let prompt = worker_prompt(&task);
    let child = tokio::process::Command::new(exe)
        .args(worker_args(&prompt, worktree))
        .current_dir(worktree)
        .env("WIZARD_FLEET", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawning worker for task '{}'", task.id))?;
    Ok(Worker {
        task,
        child,
        started: Instant::now(),
        stdout_path,
        exit: None,
    })
}

/// Write `results/<task-id>.json` for one finished (or killed) worker:
/// exit code, branch, and the child's parsed JSON summary when stdout
/// parsed.
fn write_result(
    dirs: &FleetDirs,
    task: &FleetTask,
    branch: &str,
    exit: Option<i32>,
    timed_out: bool,
    stdout_path: &Path,
) -> Result<TaskResult> {
    let summary = std::fs::read_to_string(stdout_path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(raw.trim()).ok());
    let result = TaskResult {
        task_id: task.id.clone(),
        title: task.title.clone(),
        branch: branch.to_string(),
        exit,
        timed_out,
        summary,
    };
    let path = dirs.results().join(format!("{}.json", task.id));
    let text = serde_json::to_string_pretty(&result).context("serializing task result")?;
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(result)
}

/// Touch the heartbeat file with the current unix timestamp (best-effort).
fn touch_heartbeat(dirs: &FleetDirs) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let _ = std::fs::write(dirs.heartbeat_path(), format!("{ts}\n"));
}

/// A supervising coordinator touches the heartbeat every [`TICK`]; past
/// this many seconds without one it is presumed dead (SIGKILL, OOM, reboot).
const STALE_HEARTBEAT_SECS: u64 = 30;

/// Seconds since the coordinator last touched the heartbeat file; `None`
/// when the file is missing or unreadable.
pub fn heartbeat_age_secs(dirs: &FleetDirs) -> Option<u64> {
    let raw = std::fs::read_to_string(dirs.heartbeat_path()).ok()?;
    let ts: u64 = raw.trim().parse().ok()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(now.saturating_sub(ts))
}

/// True for `fleet.toml` statuses that mean a coordinator process should
/// still be alive.
fn fleet_is_live(status: &str) -> bool {
    matches!(status, "planning" | "running" | "synthesizing")
}

/// Human-readable heartbeat line for `fleet status`. The heartbeat is only
/// touched while the coordinator supervises workers, so staleness is only
/// judged in the "running" state (planning and synthesis are single long
/// agent turns that do not tick).
fn heartbeat_note(status: &str, age: Option<u64>) -> Option<String> {
    if status != "running" {
        return None;
    }
    Some(match age {
        Some(age) if age > STALE_HEARTBEAT_SECS => {
            format!("stale ({age}s old — coordinator likely dead)")
        }
        Some(age) => format!("{age}s ago"),
        None => "none recorded".to_string(),
    })
}

/// Live supervision display: one spinner per worker slot when stderr is a
/// terminal, plain `println!` lines otherwise (logs, pipes). The periodic
/// status lines route above the bars so they never tear.
struct FleetReporter {
    bars: Option<(MultiProgress, Vec<ProgressBar>)>,
}

impl FleetReporter {
    fn new(slot_count: usize) -> Self {
        let bars = std::io::stderr()
            .is_terminal()
            .then(|| crate::progress::fleet_bars(slot_count));
        Self { bars }
    }

    /// Print a status line above the bars (or plainly off-terminal).
    fn println(&self, line: impl AsRef<str>) {
        match &self.bars {
            Some((multi, _)) => {
                let _ = multi.println(line.as_ref());
            }
            None => println!("{}", line.as_ref()),
        }
    }

    /// Refresh each slot's spinner from its current worker — claimed task id
    /// and elapsed seconds, or "idle".
    fn sync(&self, slots: &[Slot]) {
        let Some((_, bars)) = &self.bars else {
            return;
        };
        for (bar, slot) in bars.iter().zip(slots) {
            let message = match &slot.running {
                Some(worker) => {
                    format!(
                        "{} · {}s",
                        worker.task.id,
                        worker.started.elapsed().as_secs()
                    )
                }
                None => "idle".to_string(),
            };
            bar.set_message(message);
        }
    }

    /// Flag slot `i`'s spinner with a ✓ for the task that just completed.
    fn mark_done(&self, i: usize, task_id: &str) {
        if let Some((_, bars)) = &self.bars
            && let Some(bar) = bars.get(i)
        {
            bar.set_message(format!("{task_id} ✓"));
        }
    }

    /// Clear the bars at the end of supervision.
    fn finish(&self) {
        if let Some((multi, bars)) = &self.bars {
            for bar in bars {
                bar.finish_and_clear();
            }
            let _ = multi.clear();
        }
    }
}

/// Kill every running child and record killed-task results. Used by the
/// stop sentinel and ctrl-c paths.
async fn stop_all(slots: &mut [Slot], dirs: &FleetDirs, reporter: &FleetReporter) {
    for slot in slots.iter_mut() {
        if let Some(mut worker) = slot.running.take() {
            worker.child.kill().await.ok();
            reporter.println(format!("✗ killed task '{}' on shutdown", worker.task.id));
            if let Err(err) = write_result(
                dirs,
                &worker.task,
                &slot.branch,
                None,
                false,
                &worker.stdout_path,
            ) {
                tracing::warn!("could not record killed task result: {err:#}");
            }
        }
    }
}

/// The real supervision loop around [`tick`]. Returns `true` when the queue
/// drained and every child finished, `false` on a stop (sentinel or ctrl-c).
/// Wraps the loop in a [`FleetReporter`] so the per-slot bars are always
/// cleared, whichever way the loop exits.
async fn supervise(
    dirs: &FleetDirs,
    slots: &mut [Slot],
    state: &mut FleetState,
    max_minutes: u64,
) -> Result<bool> {
    let reporter = FleetReporter::new(slots.len());
    let result = supervise_loop(dirs, slots, state, max_minutes, &reporter).await;
    reporter.finish();
    result
}

async fn supervise_loop(
    dirs: &FleetDirs,
    slots: &mut [Slot],
    state: &mut FleetState,
    max_minutes: u64,
    reporter: &FleetReporter,
) -> Result<bool> {
    let max_age = Duration::from_secs(max_minutes.saturating_mul(60));
    loop {
        touch_heartbeat(dirs);

        // Observe: child exits, watchdog deadlines.
        let mut statuses = Vec::with_capacity(slots.len());
        for slot in slots.iter_mut() {
            let status = match slot.running.as_mut() {
                None => SlotStatus::Idle,
                Some(worker) => match worker.child.try_wait() {
                    Ok(Some(exit)) => {
                        // `code()` is None on signal death — recorded as a
                        // failure, exactly like the headless exit-code map.
                        worker.exit = exit.code();
                        SlotStatus::Exited
                    }
                    Ok(None) if worker.started.elapsed() >= max_age => SlotStatus::Overdue,
                    Ok(None) => SlotStatus::Running,
                    Err(err) => {
                        tracing::warn!("try_wait failed for '{}': {err}", worker.task.id);
                        worker.exit = None;
                        SlotStatus::Exited
                    }
                },
            };
            statuses.push(status);
        }

        // Decide + act.
        let actions = tick(
            &statuses,
            queue_is_empty(&dirs.queue())?,
            dirs.stop_requested(),
        );
        for action in actions {
            match action {
                TickAction::StopAll => {
                    reporter.println("fleet: stop requested — winding down");
                    stop_all(slots, dirs, reporter).await;
                    return Ok(false);
                }
                TickAction::Reap(i) => {
                    let slot = &mut slots[i];
                    if let Some(worker) = slot.running.take() {
                        let result = write_result(
                            dirs,
                            &worker.task,
                            &slot.branch,
                            worker.exit,
                            false,
                            &worker.stdout_path,
                        )?;
                        reporter.mark_done(i, &worker.task.id);
                        reporter.println(format!(
                            "← task '{}' finished ({}) on {}",
                            worker.task.id,
                            describe_exit(&result),
                            slot.branch
                        ));
                    }
                }
                TickAction::Kill(i) => {
                    let slot = &mut slots[i];
                    if let Some(mut worker) = slot.running.take() {
                        worker.child.kill().await.ok();
                        write_result(
                            dirs,
                            &worker.task,
                            &slot.branch,
                            None,
                            true,
                            &worker.stdout_path,
                        )?;
                        reporter.println(format!(
                            "⏱ task '{}' exceeded {max_minutes} min — killed",
                            worker.task.id
                        ));
                    }
                }
                TickAction::Spawn(i) => {
                    if let Some(task) = claim_next(&dirs.queue(), &dirs.claimed())? {
                        let slot = &mut slots[i];
                        let worker = spawn_worker(task, &slot.worktree, dirs)?;
                        reporter.println(format!(
                            "→ task '{}' ({}) started on {}",
                            worker.task.id, worker.task.title, slot.branch
                        ));
                        slot.running = Some(worker);
                    }
                }
                TickAction::AllDone => return Ok(true),
            }
        }

        // Persist the live pid set when it changed.
        let pids: Vec<u32> = slots
            .iter()
            .filter_map(|slot| slot.running.as_ref().and_then(|w| w.child.id()))
            .collect();
        if pids != state.pids {
            state.pids = pids;
            save_state(&dirs.state_path(), state)?;
        }

        // Refresh the per-slot bars (elapsed advances every tick).
        reporter.sync(slots);

        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result.context("listening for ctrl-c")?;
                reporter.println("fleet: interrupt — winding down");
                stop_all(slots, dirs, reporter).await;
                return Ok(false);
            }
            () = tokio::time::sleep(TICK) => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Planning / synthesis turns (in-process headless agent)
// ---------------------------------------------------------------------------

/// Run one agent turn and return the assistant's collected text. Deltas and
/// tool one-liners stream to stdout so `fleet run` reads like a normal
/// headless run; plans (if any) are auto-approved.
async fn run_collect_text(agent: &mut Agent, prompt: &str) -> Result<String> {
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
    let collector = tokio::spawn(async move {
        let mut collected = TurnText::default();
        while let Some(event) = rx.recv().await {
            collected.handle(event);
        }
        collected.text
    });
    let turn = agent.run_turn(prompt, tx).await;
    let text = collector.await.context("output collector panicked")?;
    println!();
    turn?;
    Ok(text)
}

/// Folds one planning or synthesis turn into the text `fleet` parses, echoing
/// it to stdout as it arrives so the run reads like a normal headless one.
///
/// A struct rather than a match inside the collector loop because what it
/// collects is not decoration: [`decompose`] parses this string into the task
/// list the whole fleet then runs, so a duplicated half-sentence is a wrong
/// plan, not a cosmetic glitch.
#[derive(Default)]
struct TurnText {
    /// The assistant text of the turn, in arrival order.
    text: String,
    /// Length of `text` at the last completed step: what a retry may not undo.
    committed: usize,
}

impl TurnText {
    fn handle(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::TextDelta(delta) => {
                print!("{delta}");
                let _ = std::io::Write::flush(&mut std::io::stdout());
                self.text.push_str(&delta);
            }
            AgentEvent::ToolStarted { name, .. } => println!("\n→ {name}"),
            AgentEvent::ToolFinished { name, output } => {
                let status = if output.is_error { "error" } else { "ok" };
                println!("← {name} [{status}]");
            }
            AgentEvent::Error(message) => eprintln!("\nwizard error: {message}"),
            AgentEvent::Notice(message) => eprintln!("\nwizard: {message}"),
            AgentEvent::StepCompleted { .. } => self.committed = self.text.len(),
            AgentEvent::StreamRetrying => {
                // The dead attempt is re-generated from scratch, so its partial
                // text goes: keeping it would hand the parser the same task
                // list twice, the first copy cut off mid-line. What was already
                // printed cannot be unprinted, so the cut is marked instead.
                self.text.truncate(self.committed);
                println!("\n… stream interrupted; the response restarts below …");
            }
            AgentEvent::PlanReady { gate, .. } => {
                // Nobody watches a fleet planning turn; approve so it proceeds.
                gate.answer(PlanVerdict::approve());
            }
            AgentEvent::Interview { gate, .. } => {
                // No interactive user either: decline rather than park the turn
                // inside the tool until the process is killed.
                gate.decline();
            }
            // The planning turn's product is its text; everything else is
            // bookkeeping for surfaces the fleet does not have. Spelled out so
            // a new event has to be decided about here instead of disappearing
            // into a wildcard.
            AgentEvent::ThinkingDelta(_)
            | AgentEvent::Images { .. }
            | AgentEvent::HookFired { .. }
            | AgentEvent::OmakaseProceeding { .. }
            | AgentEvent::Usage { .. }
            | AgentEvent::ContextSize { .. }
            | AgentEvent::UltraGuidance { .. }
            | AgentEvent::TodoUpdated(_)
            | AgentEvent::TaskStarted { .. }
            | AgentEvent::TaskFinished { .. }
            | AgentEvent::SubagentStarted { .. }
            | AgentEvent::SubagentFinished { .. }
            | AgentEvent::SubagentRunStarted { .. }
            | AgentEvent::SubagentRunText { .. }
            | AgentEvent::SubagentRunToolStarted { .. }
            | AgentEvent::SubagentRunToolFinished { .. }
            | AgentEvent::SubagentRunImages { .. }
            | AgentEvent::SubagentRunStep { .. }
            | AgentEvent::SubagentRunDone { .. }
            | AgentEvent::CommandRequested(_)
            | AgentEvent::Done { .. } => {}
            // A shell command's console. A fleet run has no human at the
            // keyboard, so its tool context leaves `ConsoleAccess` at `None`
            // and no command opens one; there is nothing to collect.
            AgentEvent::ConsoleOpened { .. }
            | AgentEvent::ConsoleWaiting { .. }
            | AgentEvent::ConsoleOutput { .. }
            | AgentEvent::ConsoleClosed { .. } => {}
        }
    }
}

/// The planning turn: ask the model to decompose `mission` into at most
/// `max_tasks` independent tasks, parse liberally, retry once on a parse
/// failure (the retry prompt quotes the parse error back at the model).
pub async fn decompose(
    agent: &mut Agent,
    mission: &str,
    max_tasks: usize,
) -> Result<Vec<FleetTask>> {
    let text = run_collect_text(agent, &plan_prompt(mission, max_tasks)).await?;
    let tasks = match parse_tasks(&text) {
        Ok(tasks) => tasks,
        Err(err) => {
            let text = run_collect_text(agent, &retry_prompt(&format!("{err:#}"))).await?;
            parse_tasks(&text)
                .context("the planning turn produced no parsable task list, even after a retry")?
        }
    };
    finalize_tasks(tasks, max_tasks)
}

// ---------------------------------------------------------------------------
// Worktrees
// ---------------------------------------------------------------------------

/// `git switch -c <branch>` inside `worktree`.
async fn create_branch(worktree: &Path, branch: &str) -> Result<()> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["switch", "-c", branch])
        .output()
        .await
        .context("running git switch -c")?;
    if !output.status.success() {
        bail!(
            "git switch -c {branch} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Create worker slot `index`: a worktree at `.wizard/fleet/worktrees/<i>`
/// on branch `fleet/<i>-<slug>` (with a timestamp suffix when a previous
/// run already took that name — old branches are never touched).
async fn setup_slot(
    root: &Path,
    dirs: &FleetDirs,
    index: usize,
    mission_slug: &str,
) -> Result<Slot> {
    let dest = dirs.worktrees().join(index.to_string());
    if dest.exists() {
        git::worktree_remove(root, &dest).await;
        let _ = std::fs::remove_dir_all(&dest);
    }
    // CoW-clone the working tree when the filesystem supports reflink; falls
    // back to a plain checkout otherwise. Same clean-HEAD result either way.
    git::worktree_add_cow(root, &dest, "HEAD")
        .await
        .with_context(|| format!("creating worktree for worker {index}"))?;
    let base = format!("fleet/{index}-{mission_slug}");
    let branch = match create_branch(&dest, &base).await {
        Ok(()) => base,
        Err(_taken) => {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs());
            let fallback = format!("{base}-{ts}");
            create_branch(&dest, &fallback).await?;
            fallback
        }
    };
    Ok(Slot {
        worktree: dest,
        branch,
        running: None,
    })
}

// ---------------------------------------------------------------------------
// Status table
// ---------------------------------------------------------------------------

/// One line of `fleet status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusRow {
    pub id: String,
    pub title: String,
    /// queued | running | done
    pub state: String,
    /// "-" while not finished; exit code / "timeout" / "killed" after.
    pub exit: String,
    /// "-" until the task lands on a branch.
    pub branch: String,
}

/// Read every parsable task file in `dir` (missing dir = none).
fn read_tasks_dir(dir: &Path) -> Result<Vec<FleetTask>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).context(format!("reading {}", dir.display())),
    };
    let mut tasks = Vec::new();
    for entry in entries.filter_map(std::result::Result::ok) {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        match std::fs::read_to_string(&path)
            .map_err(anyhow::Error::from)
            .and_then(|raw| serde_json::from_str::<FleetTask>(&raw).map_err(Into::into))
        {
            Ok(task) => tasks.push(task),
            Err(err) => tracing::warn!("skipping unreadable task {}: {err:#}", path.display()),
        }
    }
    Ok(tasks)
}

/// Read every parsable result file, sorted by task id.
pub fn load_results(dirs: &FleetDirs) -> Result<Vec<TaskResult>> {
    let entries = match std::fs::read_dir(dirs.results()) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).context(format!("reading {}", dirs.results().display())),
    };
    let mut results = Vec::new();
    for entry in entries.filter_map(std::result::Result::ok) {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        match std::fs::read_to_string(&path)
            .map_err(anyhow::Error::from)
            .and_then(|raw| serde_json::from_str::<TaskResult>(&raw).map_err(Into::into))
        {
            Ok(result) => results.push(result),
            Err(err) => tracing::warn!("skipping unreadable result {}: {err:#}", path.display()),
        }
    }
    results.sort_by(|a, b| a.task_id.cmp(&b.task_id));
    Ok(results)
}

/// Assemble the status table from the queue/claimed/results directories:
/// queued tasks, claimed-but-unfinished tasks (running), and finished ones.
pub fn status_rows(dirs: &FleetDirs) -> Result<Vec<StatusRow>> {
    let mut rows = Vec::new();
    for task in read_tasks_dir(&dirs.queue())? {
        rows.push(StatusRow {
            id: task.id,
            title: task.title,
            state: "queued".to_string(),
            exit: "-".to_string(),
            branch: "-".to_string(),
        });
    }
    let results = load_results(dirs)?;
    let finished: HashSet<&str> = results.iter().map(|r| r.task_id.as_str()).collect();
    for task in read_tasks_dir(&dirs.claimed())? {
        if finished.contains(task.id.as_str()) {
            continue;
        }
        rows.push(StatusRow {
            id: task.id,
            title: task.title,
            state: "running".to_string(),
            exit: "-".to_string(),
            branch: "-".to_string(),
        });
    }
    for result in results {
        let exit = match result.exit {
            Some(code) => code.to_string(),
            None if result.timed_out => "timeout".to_string(),
            None => "killed".to_string(),
        };
        rows.push(StatusRow {
            id: result.task_id,
            title: result.title,
            state: "done".to_string(),
            exit,
            branch: result.branch,
        });
    }
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(rows)
}

/// Render the status table (column widths fit the data).
pub fn render_table(rows: &[StatusRow]) -> String {
    let id_w = rows.iter().map(|r| r.id.len()).max().unwrap_or(4).max(4);
    let state_w = rows.iter().map(|r| r.state.len()).max().unwrap_or(5).max(5);
    let exit_w = rows.iter().map(|r| r.exit.len()).max().unwrap_or(4).max(4);
    let branch_w = rows
        .iter()
        .map(|r| r.branch.len())
        .max()
        .unwrap_or(6)
        .max(6);
    let mut out = format!(
        "{:<id_w$}  {:<state_w$}  {:<exit_w$}  {:<branch_w$}  title\n",
        "task", "state", "exit", "branch"
    );
    for row in rows {
        let _ = writeln!(
            out,
            "{:<id_w$}  {:<state_w$}  {:<exit_w$}  {:<branch_w$}  {}",
            row.id, row.state, row.exit, row.branch, row.title
        );
    }
    out
}

// ---------------------------------------------------------------------------
// CLI entry points
// ---------------------------------------------------------------------------

/// Dispatch a `wizard fleet` subcommand. `run` loads config (it drives a
/// real agent); `status` and `stop` only touch `.wizard/fleet/`.
pub async fn run(cmd: FleetCmd) -> Result<i32> {
    match cmd {
        FleetCmd::Run { n, prompt } => run_fleet(n, prompt).await,
        FleetCmd::Status => status_cmd(),
        FleetCmd::Stop => stop_cmd(),
    }
}

/// `wizard fleet status`: fleet.toml + the task table.
fn status_cmd() -> Result<i32> {
    let root = std::env::current_dir().context("determining project root")?;
    let dirs = FleetDirs::new(&root);
    let Some(state) = load_state(&dirs.state_path())? else {
        println!("no fleet has run in this project — start one with `wizard fleet run`");
        return Ok(0);
    };
    println!("mission: {}", state.mission);
    println!(
        "status:  {} — {} worker(s), started {}",
        state.status, state.workers, state.started
    );
    if let Some(note) = heartbeat_note(&state.status, heartbeat_age_secs(&dirs)) {
        println!("heartbeat: {note}");
    }
    if !state.pids.is_empty() {
        println!(
            "pids:    {}",
            state
                .pids
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let rows = status_rows(&dirs)?;
    if rows.is_empty() {
        println!("no tasks recorded");
    } else {
        println!();
        print!("{}", render_table(&rows));
    }
    Ok(0)
}

/// `wizard fleet stop`: write the stop sentinel; the coordinator winds down
/// on its next supervision tick. When no fleet is live (never ran, already
/// done/stopped, or the coordinator's heartbeat is stale), says so and
/// exits 1 instead of leaving a stale sentinel behind.
fn stop_cmd() -> Result<i32> {
    let root = std::env::current_dir().context("determining project root")?;
    let dirs = FleetDirs::new(&root);
    let state = load_state(&dirs.state_path())?;
    let live = state.as_ref().is_some_and(|s| fleet_is_live(&s.status));
    if !live {
        // Clear any sentinel a previous no-op stop left behind.
        let _ = std::fs::remove_file(dirs.stop_path());
        println!("no fleet is running in this project — nothing to stop");
        return Ok(1);
    }
    if let Some(state) = &state
        && state.status == "running"
        && heartbeat_age_secs(&dirs).is_none_or(|age| age > STALE_HEARTBEAT_SECS)
    {
        println!(
            "no fleet is running — fleet.toml says \"running\" but the coordinator's \
             heartbeat is stale (likely killed); nothing to stop"
        );
        return Ok(1);
    }
    std::fs::create_dir_all(dirs.stop_path().parent().expect("stop path has a parent"))
        .context("creating .wizard/fleet")?;
    std::fs::write(dirs.stop_path(), "stop\n").context("writing the stop sentinel")?;
    println!("stop requested — the coordinator winds down on its next tick");
    Ok(0)
}

/// Wipe the per-run directories (queue/claimed/results/logs) and any stale
/// stop sentinel, keeping worktrees/ for [`setup_slot`] to recycle.
fn reset_run_dirs(dirs: &FleetDirs) -> Result<()> {
    for dir in [dirs.queue(), dirs.claimed(), dirs.results(), dirs.logs()] {
        let _ = std::fs::remove_dir_all(&dir);
    }
    dirs.ensure()?;
    let _ = std::fs::remove_file(dirs.stop_path());
    Ok(())
}

/// `wizard fleet run -n N -p "<mission>"`: plan → spawn → supervise →
/// synthesize. See the module docs for the lifecycle.
async fn run_fleet(n: usize, mission: String) -> Result<i32> {
    if n == 0 {
        bail!("-n must be at least 1");
    }
    let root = std::env::current_dir().context("determining project root")?;
    git::rev_parse(&root, "HEAD").await.map_err(|err| {
        anyhow::anyhow!(
            "fleet requires a git repository with at least one commit \
             (workers run in git worktrees): {err}"
        )
    })?;
    let config = Config::load()?;

    let dirs = FleetDirs::new(&root);
    reset_run_dirs(&dirs)?;
    let mut state = FleetState {
        mission: mission.clone(),
        workers: n,
        started: chrono::Local::now().to_rfc3339(),
        status: "planning".to_string(),
        pids: Vec::new(),
    };
    save_state(&dirs.state_path(), &state)?;

    // 1. Planning turn (in-process headless agent).
    println!(
        "fleet: planning — decomposing the mission into up to {} task(s)…",
        n * 2
    );
    let mut agent = build_headless_agent(&config, &root, false).await?;
    let tasks = decompose(&mut agent, &mission, n * 2).await?;
    for task in &tasks {
        let path = dirs.queue().join(format!("{}.json", task.id));
        let text = serde_json::to_string_pretty(task).context("serializing task")?;
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    }
    println!("fleet: {} task(s) queued", tasks.len());

    // 2. Worker slots: one worktree + branch per slot, at most N.
    let mission_slug = slug(&mission);
    let slot_count = n.min(tasks.len()).max(1);
    let mut slots = Vec::with_capacity(slot_count);
    for index in 0..slot_count {
        let slot = setup_slot(&root, &dirs, index, &mission_slug).await?;
        println!(
            "fleet: worker {index} on branch {} ({})",
            slot.branch,
            slot.worktree.display()
        );
        slots.push(slot);
    }
    state.status = "running".to_string();
    save_state(&dirs.state_path(), &state)?;

    // 3. Supervision until the queue drains or a stop arrives.
    let completed = supervise(&dirs, &mut slots, &mut state, config.fleet.max_minutes).await?;
    let results = load_results(&dirs)?;

    // 4. Synthesis (skipped on stop or `[fleet] synthesize = false`).
    if completed && config.fleet.synthesize && !results.is_empty() {
        state.status = "synthesizing".to_string();
        save_state(&dirs.state_path(), &state)?;
        println!("\nfleet: synthesizing — merging fleet branches into the current branch…");
        run_collect_text(&mut agent, &synthesis_prompt(&mission, &results)).await?;
    } else if completed && !config.fleet.synthesize {
        println!("\nfleet: synthesis disabled ([fleet] synthesize = false) — branches kept:");
        for slot in &slots {
            println!("  {}", slot.branch);
        }
    }

    // 5. Teardown: worktrees go, branches stay.
    for slot in &slots {
        git::worktree_remove(&root, &slot.worktree).await;
    }
    let _ = std::fs::remove_file(dirs.stop_path());
    state.status = if completed { "done" } else { "stopped" }.to_string();
    state.pids.clear();
    save_state(&dirs.state_path(), &state)?;

    println!("\nfleet: {}", state.status);
    let rows = status_rows(&dirs)?;
    if !rows.is_empty() {
        print!("{}", render_table(&rows));
    }

    // Exit nonzero when the fleet did not finish cleanly (stopped) or any
    // task failed (nonzero exit, timeout, or kill), so `fleet run` can gate
    // CI.
    let failed = results.iter().filter(|r| r.exit != Some(0)).count();
    if failed > 0 {
        println!("fleet: {failed} task(s) failed");
    }
    Ok(if completed && failed == 0 { 0 } else { 1 })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Barrier, Mutex};

    use futures_util::stream;

    use super::*;
    use crate::agent::session::Session;
    use crate::hooks::HookEngine;
    use crate::llm::provider::LlmProvider;
    use crate::llm::{CacheTokens, ChatChunk, ChatMessage, ChatRequest, ChatStream};
    use crate::tools::registry::ToolRegistry;

    fn task(id: &str, prompt: &str) -> FleetTask {
        FleetTask {
            id: id.to_string(),
            title: format!("title of {id}"),
            prompt: prompt.to_string(),
            files_hint: Vec::new(),
        }
    }

    // --- parse_tasks ---

    #[test]
    fn parse_tasks_plain_array() {
        let tasks = parse_tasks(
            r#"[{"id":"a","title":"A","prompt":"do a","files_hint":["src/a.rs"]},
                {"id":"b","title":"B","prompt":"do b"}]"#,
        )
        .expect("parses");
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].id, "a");
        assert_eq!(tasks[0].files_hint, vec!["src/a.rs"]);
        assert_eq!(tasks[1].files_hint, Vec::<String>::new());
    }

    #[test]
    fn parse_tasks_fenced_json_with_prose() {
        let text = "Sure! Here is the decomposition you asked for:\n\n```json\n\
                    [{\"id\":\"x\",\"title\":\"X\",\"prompt\":\"do x\"}]\n```\n\
                    Let me know if you need anything else.";
        let tasks = parse_tasks(text).expect("parses");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "x");
    }

    #[test]
    fn parse_tasks_object_with_tasks_key() {
        let tasks = parse_tasks(r#"{"tasks":[{"prompt":"do it","title":"T"}]}"#).expect("parses");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].prompt, "do it");
    }

    #[test]
    fn parse_tasks_single_object_is_one_task() {
        let tasks = parse_tasks(r#"{"id":"solo","prompt":"just this"}"#).expect("parses");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "solo");
    }

    #[test]
    fn parse_tasks_skips_earlier_non_task_json() {
        // A balanced-but-wrong object precedes the real array.
        let text = r#"My config is {"mode": "fast"} so the plan is:
                      [{"id":"real","prompt":"the task"}]"#;
        let tasks = parse_tasks(text).expect("parses");
        assert_eq!(tasks[0].id, "real");
    }

    #[test]
    fn parse_tasks_handles_brackets_inside_strings() {
        let text = r#"[{"id":"s","prompt":"watch out for ] and } and \" in strings"}]"#;
        let tasks = parse_tasks(text).expect("parses");
        assert!(tasks[0].prompt.contains(']'));
    }

    #[test]
    fn parse_tasks_rejects_garbage_and_empty() {
        assert!(parse_tasks("no json here at all").is_err());
        assert!(parse_tasks("[]").is_err());
        assert!(parse_tasks(r#"[{"title":"no prompt"}]"#).is_err());
        assert!(parse_tasks("[1, 2, 3]").is_err());
    }

    // --- finalize_tasks ---

    #[test]
    fn finalize_fills_ids_dedupes_and_caps() {
        let tasks = vec![
            FleetTask {
                id: String::new(),
                title: "Fix The Parser!".to_string(),
                prompt: "p1".to_string(),
                files_hint: Vec::new(),
            },
            FleetTask {
                id: "fix-the-parser".to_string(),
                title: String::new(),
                prompt: "p2".to_string(),
                files_hint: Vec::new(),
            },
            FleetTask {
                id: String::new(),
                title: String::new(),
                prompt: "p3".to_string(),
                files_hint: Vec::new(),
            },
            FleetTask {
                id: "dropped".to_string(),
                title: "no prompt".to_string(),
                prompt: "   ".to_string(),
                files_hint: Vec::new(),
            },
            FleetTask {
                id: "over-cap".to_string(),
                title: "t".to_string(),
                prompt: "p5".to_string(),
                files_hint: Vec::new(),
            },
        ];
        let out = finalize_tasks(tasks, 3).expect("finalizes");
        assert_eq!(out.len(), 3, "capped at max_tasks");
        assert_eq!(out[0].id, "fix-the-parser");
        assert_eq!(out[1].id, "fix-the-parser-2", "duplicate id de-duplicated");
        assert_eq!(out[1].title, "fix-the-parser-2", "empty title backfilled");
        assert_eq!(out[2].id, "task-3", "positional fallback id");
    }

    #[test]
    fn finalize_rejects_all_empty() {
        let tasks = vec![task("a", "  ")];
        assert!(finalize_tasks(tasks, 4).is_err());
    }

    // --- slug ---

    #[test]
    fn slug_kebabs_and_caps() {
        assert_eq!(slug("Fix the HTTP parser"), "fix-the-http-parser");
        assert_eq!(slug("  weird///chars!!  "), "weird-chars");
        assert_eq!(slug("!!!"), "fleet");
        let long = slug("a very long mission statement that keeps going and going");
        assert!(long.len() <= SLUG_MAX, "{long:?} too long");
        assert!(!long.ends_with('-'));
    }

    // --- worker argv / prompt ---

    #[test]
    fn worker_args_construction() {
        let args = worker_args("do the thing", Path::new("/tmp/wt/0"));
        assert_eq!(
            args,
            vec![
                "--mode",
                "sovereign",
                "-p",
                "do the thing",
                "--cwd",
                "/tmp/wt/0",
                "--output-format",
                "json",
            ]
        );
    }

    #[test]
    fn worker_prompt_wraps_task_with_commit_instructions() {
        let mut t = task("a", "refactor the parser");
        t.files_hint = vec!["src/parser.rs".to_string()];
        let p = worker_prompt(&t);
        assert!(p.contains("refactor the parser"));
        assert!(p.contains("src/parser.rs"));
        assert!(p.contains("commit your changes"));
        assert!(p.contains("Do not push"));
    }

    // --- claiming ---

    #[test]
    fn claim_moves_task_from_queue_to_claimed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = dir.path().join("queue");
        let claimed = dir.path().join("claimed");
        std::fs::create_dir_all(&queue).unwrap();
        std::fs::create_dir_all(&claimed).unwrap();
        let t = task("a", "do a");
        std::fs::write(queue.join("a.json"), serde_json::to_string(&t).unwrap()).unwrap();

        assert!(!queue_is_empty(&queue).unwrap());
        let got = claim_next(&queue, &claimed)
            .expect("claims")
            .expect("a task");
        assert_eq!(got, t);
        assert!(queue_is_empty(&queue).unwrap());
        assert!(claimed.join("a.json").exists());
        assert!(claim_next(&queue, &claimed).expect("ok").is_none());
    }

    #[test]
    fn claim_race_exactly_one_wins() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = dir.path().join("queue");
        let claimed = dir.path().join("claimed");
        std::fs::create_dir_all(&queue).unwrap();
        std::fs::create_dir_all(&claimed).unwrap();
        std::fs::write(
            queue.join("only.json"),
            serde_json::to_string(&task("only", "the one task")).unwrap(),
        )
        .unwrap();

        let barrier = Arc::new(Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let queue = queue.clone();
                let claimed = claimed.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    claim_next(&queue, &claimed).expect("claim never errors")
                })
            })
            .collect();
        let wins: Vec<_> = handles
            .into_iter()
            .map(|h| h.join().expect("thread joins"))
            .collect();
        assert_eq!(
            wins.iter().filter(|w| w.is_some()).count(),
            1,
            "exactly one claimant wins: {wins:?}"
        );
    }

    #[test]
    fn claim_on_missing_queue_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(
            claim_next(&dir.path().join("absent"), &dir.path().join("claimed"))
                .expect("ok")
                .is_none()
        );
        assert!(queue_is_empty(&dir.path().join("absent")).unwrap());
    }

    // --- fleet.toml ---

    #[test]
    fn fleet_state_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("fleet.toml");
        assert!(load_state(&path).expect("missing is None").is_none());

        let state = FleetState {
            mission: "improve test coverage".to_string(),
            workers: 3,
            started: "2026-06-11T10:00:00-04:00".to_string(),
            status: "running".to_string(),
            pids: vec![123, 456],
        };
        save_state(&path, &state).expect("saves");
        let loaded = load_state(&path).expect("loads").expect("present");
        assert_eq!(loaded, state);
    }

    // --- heartbeat ---

    #[test]
    fn heartbeat_age_reads_the_touched_timestamp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dirs = FleetDirs::new(dir.path());
        dirs.ensure().expect("ensure");

        assert_eq!(heartbeat_age_secs(&dirs), None, "missing file is None");
        std::fs::write(dirs.heartbeat_path(), "not a number\n").unwrap();
        assert_eq!(heartbeat_age_secs(&dirs), None, "garbage is None");

        touch_heartbeat(&dirs);
        let age = heartbeat_age_secs(&dirs).expect("age readable");
        assert!(age <= 2, "freshly touched: {age}s");

        let old = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 120;
        std::fs::write(dirs.heartbeat_path(), format!("{old}\n")).unwrap();
        let age = heartbeat_age_secs(&dirs).expect("age readable");
        assert!((115..=125).contains(&age), "two minutes old: {age}s");
    }

    #[test]
    fn heartbeat_note_flags_staleness_only_while_running() {
        assert_eq!(heartbeat_note("done", Some(9999)), None);
        assert_eq!(heartbeat_note("planning", None), None);
        assert_eq!(
            heartbeat_note("running", Some(2)).as_deref(),
            Some("2s ago")
        );
        let stale = heartbeat_note("running", Some(120)).expect("note");
        assert!(stale.contains("stale"), "{stale}");
        assert!(stale.contains("coordinator likely dead"), "{stale}");
        assert_eq!(
            heartbeat_note("running", None).as_deref(),
            Some("none recorded")
        );
    }

    #[test]
    fn live_statuses() {
        for live in ["planning", "running", "synthesizing"] {
            assert!(fleet_is_live(live), "{live}");
        }
        for dead in ["done", "stopped", ""] {
            assert!(!fleet_is_live(dead), "{dead:?}");
        }
    }

    // --- stop sentinel ---

    #[test]
    fn stop_sentinel_detection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dirs = FleetDirs::new(dir.path());
        dirs.ensure().expect("ensure");
        assert!(!dirs.stop_requested());
        std::fs::write(dirs.stop_path(), "stop\n").unwrap();
        assert!(dirs.stop_requested());
    }

    // --- tick ---

    #[test]
    fn tick_stop_overrides_everything() {
        let slots = [SlotStatus::Running, SlotStatus::Exited, SlotStatus::Idle];
        assert_eq!(tick(&slots, false, true), vec![TickAction::StopAll]);
    }

    #[test]
    fn tick_reaps_kills_and_spawns() {
        let slots = [
            SlotStatus::Exited,
            SlotStatus::Overdue,
            SlotStatus::Idle,
            SlotStatus::Running,
        ];
        assert_eq!(
            tick(&slots, false, false),
            vec![
                TickAction::Reap(0),
                TickAction::Kill(1),
                TickAction::Spawn(2),
            ]
        );
    }

    #[test]
    fn tick_idle_slots_wait_when_queue_is_empty() {
        let slots = [SlotStatus::Idle, SlotStatus::Running];
        assert_eq!(tick(&slots, true, false), Vec::<TickAction>::new());
    }

    #[test]
    fn tick_all_done_when_queue_empty_and_all_idle() {
        let slots = [SlotStatus::Idle, SlotStatus::Idle];
        assert_eq!(tick(&slots, true, false), vec![TickAction::AllDone]);
    }

    #[test]
    fn tick_reaps_before_done_on_empty_queue() {
        let slots = [SlotStatus::Idle, SlotStatus::Exited];
        assert_eq!(tick(&slots, true, false), vec![TickAction::Reap(1)]);
    }

    // --- status table ---

    #[test]
    fn status_rows_from_synthetic_layout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dirs = FleetDirs::new(dir.path());
        dirs.ensure().expect("ensure");

        let write_task = |dir: &Path, t: &FleetTask| {
            std::fs::write(
                dir.join(format!("{}.json", t.id)),
                serde_json::to_string(t).unwrap(),
            )
            .unwrap();
        };
        write_task(&dirs.queue(), &task("c-queued", "later"));
        write_task(&dirs.claimed(), &task("a-running", "now"));
        write_task(&dirs.claimed(), &task("b-done", "earlier"));
        let result = TaskResult {
            task_id: "b-done".to_string(),
            title: "title of b-done".to_string(),
            branch: "fleet/0-mission".to_string(),
            exit: Some(0),
            timed_out: false,
            summary: Some(serde_json::json!({"result": "ok", "reason": "completed"})),
        };
        std::fs::write(
            dirs.results().join("b-done.json"),
            serde_json::to_string(&result).unwrap(),
        )
        .unwrap();

        let rows = status_rows(&dirs).expect("rows");
        assert_eq!(rows.len(), 3);
        assert_eq!(
            (
                rows[0].id.as_str(),
                rows[0].state.as_str(),
                rows[0].exit.as_str()
            ),
            ("a-running", "running", "-")
        );
        assert_eq!(
            (
                rows[1].id.as_str(),
                rows[1].state.as_str(),
                rows[1].exit.as_str()
            ),
            ("b-done", "done", "0")
        );
        assert_eq!(rows[1].branch, "fleet/0-mission");
        assert_eq!(
            (rows[2].id.as_str(), rows[2].state.as_str()),
            ("c-queued", "queued")
        );

        let table = render_table(&rows);
        assert!(table.starts_with("task"));
        assert_eq!(table.lines().count(), 4, "header + three rows");
    }

    #[test]
    fn status_rows_label_killed_and_timeout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dirs = FleetDirs::new(dir.path());
        dirs.ensure().expect("ensure");
        for (id, timed_out) in [("killed", false), ("slow", true)] {
            let result = TaskResult {
                task_id: id.to_string(),
                title: id.to_string(),
                branch: "fleet/0-x".to_string(),
                exit: None,
                timed_out,
                summary: None,
            };
            std::fs::write(
                dirs.results().join(format!("{id}.json")),
                serde_json::to_string(&result).unwrap(),
            )
            .unwrap();
        }
        let rows = status_rows(&dirs).expect("rows");
        assert_eq!(rows[0].exit, "killed");
        assert_eq!(rows[1].exit, "timeout");
    }

    // --- planning pipeline with a scripted provider ---

    /// Minimal scripted provider: replays canned chunk sequences, one per
    /// `chat_stream` call, and counts requests (mirrors the agent-loop test
    /// harness in `crate::agent`).
    #[derive(Debug)]
    struct Scripted {
        responses: Mutex<VecDeque<Vec<ChatChunk>>>,
        requests: Mutex<usize>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for Scripted {
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
            *self.requests.lock().unwrap() += 1;
            let chunks = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted response available");
            Ok(futures_util::StreamExt::boxed(stream::iter(
                chunks.into_iter().map(Ok),
            )))
        }

        async fn context_window(&self, _model: &str) -> Option<u32> {
            None
        }

        fn label(&self) -> String {
            "scripted:fleet-test".to_string()
        }
    }

    fn reply(content: &str) -> Vec<ChatChunk> {
        vec![ChatChunk {
            message: Some(ChatMessage::assistant(content)),
            images: Vec::new(),
            thinking: false,
            done: true,
            done_reason: None,
            eval_count: None,
            prompt_eval_count: None,
            cache: CacheTokens::NONE,
        }]
    }

    /// Build an in-process agent over a scripted provider, rooted in `tmp`
    /// (sessions and the usage log stay inside it).
    fn scripted_agent(tmp: &Path, responses: Vec<Vec<ChatChunk>>) -> (Agent, Arc<Scripted>) {
        let provider = Arc::new(Scripted {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(0),
        });
        let session = Session::create(&tmp.join("sessions")).expect("create session");
        let hooks = Arc::new(HookEngine::new(
            Vec::new(),
            tmp.to_path_buf(),
            session.id.clone(),
        ));
        let mut agent = Agent::new(
            Arc::clone(&provider) as Arc<dyn LlmProvider>,
            ToolRegistry::new(),
            Config::default(),
            Vec::new(),
            tmp.to_path_buf(),
            session,
            true,
            hooks,
        )
        .expect("build agent");
        agent.set_usage_log(Some(tmp.join("usage.jsonl")));
        (agent, provider)
    }

    #[tokio::test]
    async fn decompose_parses_a_canned_task_list() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut agent, provider) = scripted_agent(
            dir.path(),
            vec![reply(
                "Here you go:\n```json\n[\
                 {\"id\":\"add-tests\",\"title\":\"Add tests\",\"prompt\":\"add unit tests\",\
                  \"files_hint\":[\"src/lib.rs\"]},\
                 {\"id\":\"write-docs\",\"title\":\"Write docs\",\"prompt\":\"document the api\"}\
                 ]\n```",
            )],
        );
        let tasks = decompose(&mut agent, "improve the project", 4)
            .await
            .expect("decomposes");
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].id, "add-tests");
        assert_eq!(tasks[0].files_hint, vec!["src/lib.rs"]);
        assert_eq!(tasks[1].id, "write-docs");
        assert_eq!(*provider.requests.lock().unwrap(), 1, "no retry needed");
    }

    #[tokio::test]
    async fn decompose_retries_once_on_unparsable_reply() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut agent, provider) = scripted_agent(
            dir.path(),
            vec![
                reply("I would split this mission into two parts, roughly speaking."),
                reply(r#"[{"id":"only","title":"Only","prompt":"the single task"}]"#),
            ],
        );
        let tasks = decompose(&mut agent, "do the thing", 4)
            .await
            .expect("decomposes on retry");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "only");
        assert_eq!(*provider.requests.lock().unwrap(), 2, "exactly one retry");
    }

    #[tokio::test]
    async fn decompose_fails_after_two_unparsable_replies() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut agent, provider) = scripted_agent(
            dir.path(),
            vec![reply("no json, sorry"), reply("still no json")],
        );
        let err = decompose(&mut agent, "do the thing", 4)
            .await
            .expect_err("gives up after the retry");
        assert!(err.to_string().contains("retry"), "{err}");
        assert_eq!(*provider.requests.lock().unwrap(), 2);
    }

    // --- synthesis prompt ---

    #[test]
    fn synthesis_prompt_lists_results_and_merge_instructions() {
        let results = vec![
            TaskResult {
                task_id: "a".to_string(),
                title: "Task A".to_string(),
                branch: "fleet/0-m".to_string(),
                exit: Some(0),
                timed_out: false,
                summary: Some(serde_json::json!({"result": "did the thing"})),
            },
            TaskResult {
                task_id: "b".to_string(),
                title: "Task B".to_string(),
                branch: "fleet/1-m".to_string(),
                exit: None,
                timed_out: true,
                summary: None,
            },
        ];
        let p = synthesis_prompt("the mission", &results);
        assert!(p.contains("the mission"));
        assert!(p.contains("fleet/0-m"));
        assert!(p.contains("did the thing"));
        assert!(p.contains("killed (timeout)"));
        assert!(p.contains("git merge"));
        assert!(p.contains("--abort"));
        assert!(!p.contains("force-push"), "never instructs force anything");
    }

    /// The planning turn's text becomes the task list, so a completion that
    /// dies mid-stream must not leave its half-written tasks in front of the
    /// re-generated ones: the fleet would run a plan the model never finished
    /// writing.
    #[test]
    fn a_retried_stream_leaves_no_duplicate_in_the_collected_text() {
        let mut collected = TurnText::default();
        collected.handle(AgentEvent::TextDelta("intro. ".to_string()));
        collected.handle(AgentEvent::StepCompleted { step: 1 });
        collected.handle(AgentEvent::TextDelta("1. half a ta".to_string()));
        collected.handle(AgentEvent::StreamRetrying);
        collected.handle(AgentEvent::TextDelta("1. the whole task".to_string()));

        assert_eq!(collected.text, "intro. 1. the whole task");
    }

    /// A fleet planning turn has no reviewer, so both gates answer themselves
    /// rather than parking the turn inside the tool forever.
    #[test]
    fn plan_and_interview_gates_are_answered_without_a_human() {
        let mut collected = TurnText::default();
        let (gate, mut verdict) = crate::agent::PlanGate::open();
        collected.handle(AgentEvent::PlanReady {
            plan: "1. do it".to_string(),
            gate,
        });
        assert!(verdict.try_recv().expect("verdict sent").approved);

        let (gate, mut answers) = crate::agent::InterviewGate::open();
        collected.handle(AgentEvent::Interview {
            questions: Vec::new(),
            gate,
        });
        assert_eq!(answers.try_recv().expect("interview answered"), None);
    }
}
