//! Cron scheduler: `wizard schedule` (entry CRUD + foreground run) and the
//! `wizard scheduler` daemon.
//!
//! Entries live in `~/.wizard/schedule.toml` as `[[entries]]` blocks. The
//! daemon reloads the file every pass, so edits — by hand or via `wizard
//! schedule add/remove` — are picked up without a restart. Missed runs are
//! never backfilled: each entry's clock starts at daemon startup (or its
//! last fire within this daemon's lifetime), so occurrences that passed
//! while the daemon was down collapse into nothing, and several occurrences
//! missed during one long sleep collapse into a single fire.
//!
//! Everything here is config-independent: no `config.toml` load, no LLM in
//! this process. Jobs are spawned `wizard` child processes (via
//! `current_exe()`) that load their own config, exactly like a user-invoked
//! headless run.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, TimeZone};
use croner::Cron;
use croner::parser::{CronParser, Seconds, Year};
use serde::{Deserialize, Serialize};

use crate::cli::ScheduleCmd;
use crate::config::Config;

/// Daemon sleep cap so schedule reloads, job reaping, and timeout
/// enforcement all happen at least this often.
const MAX_SLEEP: Duration = Duration::from_secs(60);

/// Grace beyond an entry's `max_hours` before the daemon hard-kills the
/// child. The child also receives `--max-hours`, so the normal path is a
/// graceful self-stop (exit code 4); the kill is the backstop.
const KILL_GRACE: Duration = Duration::from_secs(120);

/// Rotate `scheduler.log` past this size (rename to `.log.old`).
const LOG_ROTATE_BYTES: u64 = 5 * 1024 * 1024;

/// Largest accepted `--max-hours` / `max_hours`: one year.
///
/// A cap rather than "any positive number" because the value ends up in
/// `Instant + Duration`, which panics on overflow just as
/// `Duration::from_secs_f64` panics on a negative. A time limit longer than a
/// year is not a limit anybody means, so refusing it costs nothing and makes
/// every downstream arithmetic total.
pub const MAX_HOURS_CAP: f64 = 24.0 * 365.0;

/// The wall-clock budget a `max_hours` value stands for, or an error saying
/// why the number is unusable.
///
/// Every conversion goes through here. `Duration::from_secs_f64` panics on a
/// negative, a NaN and an infinity, and the value reaching it is an `f64` that
/// came either from a flag or from a hand-edited `schedule.toml` — so
/// `max_hours = -1` in that file used to take down the scheduler daemon the
/// moment the job fired, and `--max-hours -1` took down the run it was given
/// to.
pub fn max_hours_duration(hours: f64) -> Result<Duration> {
    if !hours.is_finite() || hours <= 0.0 {
        bail!("max_hours must be a positive, finite number of hours, got {hours}");
    }
    if hours > MAX_HOURS_CAP {
        bail!("max_hours must be at most {MAX_HOURS_CAP} (one year), got {hours}");
    }
    Ok(Duration::from_secs_f64(hours * 3600.0))
}

/// One scheduled job (`[[entries]]` in schedule.toml).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleEntry {
    /// Unique key (`[a-zA-Z0-9_-]+`).
    pub name: String,
    /// Standard 5-field cron expression, evaluated in local time.
    pub cron: String,
    /// Task prompt handed to the spawned headless run.
    pub prompt: String,
    /// Directory the run executes in.
    pub cwd: PathBuf,
    /// "sovereign" (default) or "continuous".
    #[serde(default = "default_mode")]
    pub mode: String,
    /// Wall-clock cap in hours for the spawned run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_hours: Option<f64>,
    /// Disabled entries are kept in the file but never fired.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Fire once, then remove the entry. What `CronCreate` with
    /// `recurring = false` writes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub once: bool,
    /// After this instant the entry is removed: on the pass that fires it, or
    /// on the first pass after it when nothing was due. What keeps a
    /// forgotten `CronCreate` job from running forever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<chrono::Utc>>,
}

impl ScheduleEntry {
    /// Whether the entry is spent after the pass at `now` fired it.
    pub fn spent_after_firing(&self, now: DateTime<chrono::Utc>) -> bool {
        self.once || self.is_expired(now)
    }

    /// Whether the entry's `expires_at` has passed.
    pub fn is_expired(&self, now: DateTime<chrono::Utc>) -> bool {
        self.expires_at.is_some_and(|at| at <= now)
    }

    /// This entry's wall-clock budget: `None` when it has no limit, `Err`
    /// when the number in the file is not one [`max_hours_duration`] accepts.
    pub fn max_duration(&self) -> Result<Option<Duration>> {
        self.max_hours.map(max_hours_duration).transpose()
    }
}

fn default_mode() -> String {
    "sovereign".to_string()
}

fn default_enabled() -> bool {
    true
}

/// Contents of `~/.wizard/schedule.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScheduleFile {
    #[serde(default)]
    pub entries: Vec<ScheduleEntry>,
}

/// Dispatch a `wizard schedule` subcommand against the default schedule
/// file. Returns the process exit code (`run` propagates the child's).
pub async fn run(cmd: ScheduleCmd) -> Result<i32> {
    let path = Config::schedule_path()?;
    match cmd {
        ScheduleCmd::Add {
            name,
            cron,
            prompt,
            cwd,
            max_hours,
            mode,
        } => {
            let cwd = cwd
                .canonicalize()
                .with_context(|| format!("--cwd {}: directory must exist", cwd.display()))?;
            let entry = ScheduleEntry {
                name,
                cron,
                prompt,
                cwd,
                mode,
                max_hours,
                enabled: true,
                once: false,
                expires_at: None,
            };
            add_entry(&path, entry.clone())?;
            let next = next_occurrence(&entry, &Local::now())?;
            println!(
                "added '{}' — next fire {} ({})",
                entry.name,
                next.format("%Y-%m-%d %H:%M:%S %Z"),
                humanize_until(next - Local::now()),
            );
            Ok(0)
        }
        ScheduleCmd::List => {
            list(&path)?;
            Ok(0)
        }
        ScheduleCmd::Remove { name } => {
            remove_entry(&path, &name)?;
            println!("removed '{name}'");
            Ok(0)
        }
        ScheduleCmd::Enable { name } => {
            set_enabled(&path, &name, true)?;
            println!("enabled '{name}'");
            Ok(0)
        }
        ScheduleCmd::Disable { name } => {
            set_enabled(&path, &name, false)?;
            println!("disabled '{name}' — kept in the file, never fired");
            Ok(0)
        }
        ScheduleCmd::Run { name } => run_foreground(&path, &name).await,
    }
}

/// Strict 5-field cron parse (no seconds, no year field) — what crontab
/// users expect; rejects everything else up front.
pub fn parse_cron(expr: &str) -> Result<Cron> {
    CronParser::builder()
        .seconds(Seconds::Disallowed)
        .year(Year::Disallowed)
        .build()
        .parse(expr)
        .map_err(|err| {
            anyhow::anyhow!(
                "invalid cron expression {expr:?}: {err} \
                 (expected 5 fields: minute hour day month weekday)"
            )
        })
}

/// Entry names become toml keys and log labels; keep them path-safe.
fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        bail!("invalid entry name {name:?}: only [a-zA-Z0-9_-] is allowed");
    }
    Ok(())
}

/// Reject anything but the two headless job modes.
fn validate_mode(mode: &str) -> Result<()> {
    match mode {
        "sovereign" | "continuous" => Ok(()),
        other => bail!("invalid mode {other:?} (expected sovereign or continuous)"),
    }
}

/// Full per-entry validation: name, mode, cron, and an existing cwd.
fn validate_entry(entry: &ScheduleEntry) -> Result<()> {
    validate_name(&entry.name)?;
    validate_mode(&entry.mode)?;
    parse_cron(&entry.cron)?;
    if !entry.cwd.is_dir() {
        bail!(
            "cwd {} does not exist or is not a directory",
            entry.cwd.display()
        );
    }
    entry.max_duration()?;
    Ok(())
}

/// Load the schedule file; a missing file is an empty schedule.
pub fn load_schedule(path: &Path) -> Result<ScheduleFile> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ScheduleFile::default());
        }
        Err(err) => return Err(err).context(format!("reading {}", path.display())),
    };
    let file: ScheduleFile =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;

    // Say something about a field nobody can use, once, here.
    //
    // `due_entries` drops such an entry with a bare `return false`, which is
    // the right thing for a hot loop that runs every second — but it is the
    // *only* thing that happens to it. A job whose expression has a typo is
    // therefore never fired and never explains itself: the daemon keeps
    // running, `schedule list` keeps showing the entry as enabled, and the
    // next time is simply never. `schedule add` validates, so this is the
    // path where a hand-edited or externally-written file lands.
    //
    // `max_hours` is checked for the same reason and one more: the number
    // goes into `Duration::from_secs_f64`, which *panics* on a negative or a
    // NaN. Before this, `max_hours = -1` here went unremarked all the way to
    // the daemon spawning the job, and took the daemon down with it — a
    // long-running process killed by a line somebody typed into a config
    // file. `due_entries` now refuses to fire it and this says why.
    //
    // Warnings rather than an error, deliberately: one bad entry must not
    // stop the other jobs from firing, which is what returning `Err` here
    // would do.
    for entry in &file.entries {
        if let Err(err) = parse_cron(&entry.cron) {
            tracing::warn!(
                "schedule entry {:?} has an unusable cron ({:?}): {err}. It will never \
                 fire until this is fixed; `wizard schedule add` validates the \
                 expression if you want to rewrite it.",
                entry.name,
                entry.cron,
            );
        }
        if let Err(err) = entry.max_duration() {
            tracing::warn!(
                "schedule entry {:?} has an unusable max_hours: {err}. It will never \
                 fire until this is fixed.",
                entry.name,
            );
        }
    }
    Ok(file)
}

/// Write the schedule file, creating parent directories as needed.
pub fn save_schedule(path: &Path, file: &ScheduleFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(file).context("serializing schedule")?;
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

/// Validate and append a new entry; the name must be unique.
pub fn add_entry(path: &Path, entry: ScheduleEntry) -> Result<()> {
    validate_entry(&entry)?;
    let mut file = load_schedule(path)?;
    if file.entries.iter().any(|e| e.name == entry.name) {
        bail!(
            "entry '{}' already exists — remove it first or pick another name",
            entry.name
        );
    }
    file.entries.push(entry);
    save_schedule(path, &file)
}

/// Remove an entry by name; error when absent.
pub fn remove_entry(path: &Path, name: &str) -> Result<()> {
    let mut file = load_schedule(path)?;
    let before = file.entries.len();
    file.entries.retain(|e| e.name != name);
    if file.entries.len() == before {
        bail!("no entry named '{name}' — see `wizard schedule list`");
    }
    save_schedule(path, &file)
}

/// Set an entry's `enabled` flag; error when absent. Idempotent: enabling
/// an enabled entry (or disabling a disabled one) is a no-op that succeeds.
pub fn set_enabled(path: &Path, name: &str, enabled: bool) -> Result<()> {
    let mut file = load_schedule(path)?;
    let entry = file
        .entries
        .iter_mut()
        .find(|e| e.name == name)
        .with_context(|| format!("no entry named '{name}' — see `wizard schedule list`"))?;
    entry.enabled = enabled;
    save_schedule(path, &file)
}

/// Next fire strictly after `now` for one entry.
fn next_occurrence<Tz: TimeZone>(
    entry: &ScheduleEntry,
    now: &DateTime<Tz>,
) -> Result<DateTime<Tz>> {
    let cron = parse_cron(&entry.cron)?;
    cron.find_next_occurrence(now, false)
        .map_err(|err| anyhow::anyhow!("computing next fire for '{}': {err}", entry.name))
}

fn list(path: &Path) -> Result<()> {
    let file = load_schedule(path)?;
    if file.entries.is_empty() {
        println!("no entries — add one with `wizard schedule add`");
        return Ok(());
    }
    let now = Local::now();
    let name_width = file
        .entries
        .iter()
        .map(|e| e.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let cron_width = file
        .entries
        .iter()
        .map(|e| e.cron.len())
        .max()
        .unwrap_or(4)
        .max(4);
    println!(
        "{:<name_width$}  {:<cron_width$}  {:<3}  {:<32}  cwd",
        "name", "cron", "on", "next"
    );
    for entry in &file.entries {
        let on = if entry.enabled { "yes" } else { "no" };
        let next = match next_occurrence(entry, &now) {
            Ok(next) => format!(
                "{} ({})",
                next.format("%Y-%m-%d %H:%M"),
                humanize_until(next - now)
            ),
            Err(_) => "invalid cron".to_string(),
        };
        println!(
            "{:<name_width$}  {:<cron_width$}  {on:<3}  {next:<32}  {}",
            entry.name,
            entry.cron,
            entry.cwd.display()
        );
    }
    Ok(())
}

/// Argv (after the binary itself) of the wizard child that executes `entry`
/// — the single source of truth for both the daemon and `schedule run`.
/// `--max-hours` is passed through so the child winds down gracefully on
/// its own; the daemon's kill at `max_hours + grace` is only a backstop.
pub fn child_args(entry: &ScheduleEntry) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    if entry.mode == "continuous" {
        // --continuous implies sovereign.
        args.push("--continuous".to_string());
    } else {
        args.push("--mode".to_string());
        args.push("sovereign".to_string());
    }
    args.push("-p".to_string());
    args.push(entry.prompt.clone());
    args.push("--cwd".to_string());
    args.push(entry.cwd.display().to_string());
    if let Some(hours) = entry.max_hours {
        args.push("--max-hours".to_string());
        args.push(hours.to_string());
    }
    args
}

/// Build the child command: this binary, the entry's argv, cwd set (both
/// via `--cwd` and the process working directory, so relative paths inside
/// the run resolve correctly either way).
fn child_command(entry: &ScheduleEntry) -> Result<tokio::process::Command> {
    let exe = std::env::current_exe().context("locating the wizard binary for the job")?;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(child_args(entry)).current_dir(&entry.cwd);
    Ok(cmd)
}

/// `wizard schedule run <name>`: spawn the entry's job in the foreground
/// with inherited stdio and return its exit code. `max_hours` is enforced
/// here too: on the hard timeout the child is killed and the run exits 4
/// (the time-limit exit code).
async fn run_foreground(path: &Path, name: &str) -> Result<i32> {
    let file = load_schedule(path)?;
    let entry = file
        .entries
        .iter()
        .find(|e| e.name == name)
        .with_context(|| format!("no entry named '{name}' — see `wizard schedule list`"))?;
    validate_entry(entry)?;

    let mut child = child_command(entry)?
        .kill_on_drop(true)
        .spawn()
        .context("spawning the job")?;
    let deadline = entry.max_duration()?.map(|budget| budget + KILL_GRACE);
    let status = match deadline {
        Some(cap) => match tokio::time::timeout(cap, child.wait()).await {
            Ok(status) => status.context("waiting for the job")?,
            Err(_elapsed) => {
                child.kill().await.ok();
                eprintln!("job '{name}' exceeded max_hours — killed");
                return Ok(4);
            }
        },
        None => child.wait().await.context("waiting for the job")?,
    };
    Ok(status.code().unwrap_or(1))
}

// ---------------------------------------------------------------------------
// Daemon
// ---------------------------------------------------------------------------

/// A job the daemon has spawned and not yet reaped.
struct RunningJob {
    name: String,
    child: tokio::process::Child,
    started: Instant,
    /// `max_hours + KILL_GRACE` from spawn; `None` = unbounded.
    deadline: Option<Instant>,
    /// Where the job's stdout/stderr are captured.
    log_path: PathBuf,
}

/// Pure due-detection: which enabled entries have an occurrence at or
/// before `now`, measured strictly after their basis — the entry's last
/// fire within this daemon's lifetime, or `started` for entries that have
/// not fired yet. Entries with invalid cron expressions are never due.
pub fn due_entries<'a, Tz: TimeZone>(
    entries: &'a [ScheduleEntry],
    last_fired: &HashMap<String, DateTime<Tz>>,
    started: &DateTime<Tz>,
    now: &DateTime<Tz>,
) -> Vec<&'a ScheduleEntry> {
    entries
        .iter()
        .filter(|entry| {
            if !entry.enabled {
                return false;
            }
            let Ok(cron) = parse_cron(&entry.cron) else {
                return false;
            };
            // Same treatment as an unparseable cron, and for a sharper
            // reason: `spawn_job` turns `max_hours` into a `Duration`, and a
            // negative or NaN one panics the daemon rather than the job.
            // `load_schedule` has already warned about it by name.
            if entry.max_duration().is_err() {
                return false;
            }
            let basis = last_fired.get(&entry.name).unwrap_or(started);
            match cron.find_next_occurrence(basis, false) {
                Ok(next) => next <= *now,
                Err(_) => false,
            }
        })
        .collect()
}

/// Remove entries whose `expires_at` has passed without being due on this
/// pass. The ones that were due have already fired and gone.
fn prune_expired(path: &Path, entries: &[ScheduleEntry], log_path: &Path) {
    let now = chrono::Utc::now();
    for entry in entries.iter().filter(|e| e.is_expired(now)) {
        if remove_entry(path, &entry.name).is_ok() {
            log_line(
                log_path,
                &format!("'{}' expired and was removed", entry.name),
            );
        }
    }
}

/// Whether a `wizard scheduler` daemon holds the lock right now. A schedule
/// entry written while none does is stored and fires nothing until one starts.
#[cfg(unix)]
pub fn daemon_running() -> bool {
    use std::os::unix::io::AsRawFd;
    let Ok(dir) = Config::wizard_dir() else {
        return false;
    };
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.join("scheduler.lock"))
    else {
        return false;
    };
    // SAFETY: flock on a descriptor this function owns.
    let acquired = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    // Dropping the file releases the lock taken just now.
    !acquired
}

/// No lock file semantics off Unix; assume the daemon is not known to run.
#[cfg(not(unix))]
pub fn daemon_running() -> bool {
    false
}

/// Earliest next fire across enabled, valid entries, strictly after `now`.
pub fn next_fire_across<Tz: TimeZone>(
    entries: &[ScheduleEntry],
    now: &DateTime<Tz>,
) -> Option<DateTime<Tz>> {
    entries
        .iter()
        .filter(|entry| entry.enabled)
        .filter_map(|entry| {
            parse_cron(&entry.cron)
                .ok()?
                .find_next_occurrence(now, false)
                .ok()
        })
        .min()
}

/// Append a timestamped line to the scheduler log (rotating past
/// [`LOG_ROTATE_BYTES`]) and echo it to stdout for foreground / journald
/// visibility. Logging must never take the daemon down, so errors are
/// swallowed.
fn log_line(path: &Path, msg: &str) {
    let line = format!("{} {msg}", Local::now().format("%Y-%m-%d %H:%M:%S%z"));
    println!("{line}");
    if let Ok(meta) = std::fs::metadata(path)
        && meta.len() > LOG_ROTATE_BYTES
    {
        let _ = std::fs::rename(path, path.with_extension("log.old"));
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

/// Per-job log files kept per entry name in `~/.wizard/logs/jobs/`; older
/// ones are pruned on each spawn.
const JOB_LOGS_KEEP: usize = 10;

/// `<jobs_dir>/<name>-<timestamp>.log` for a job fired now. The timestamp
/// sorts lexically, so pruning can just sort file names.
fn job_log_path(jobs_dir: &Path, name: &str) -> PathBuf {
    jobs_dir.join(format!(
        "{name}-{}.log",
        Local::now().format("%Y%m%d-%H%M%S")
    ))
}

/// Keep only the newest `keep` logs for `name` in `jobs_dir` (best-effort:
/// pruning failures must never stop the daemon).
fn prune_job_logs(jobs_dir: &Path, name: &str, keep: usize) {
    let Ok(entries) = std::fs::read_dir(jobs_dir) else {
        return;
    };
    let prefix = format!("{name}-");
    let mut logs: Vec<PathBuf> = entries
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".log"))
        })
        .collect();
    logs.sort();
    let excess = logs.len().saturating_sub(keep);
    for old in &logs[..excess] {
        let _ = std::fs::remove_file(old);
    }
}

/// Spawn one entry's job for the daemon: stdout/stderr go to a per-job log
/// under `~/.wizard/logs/jobs/`, so early failures leave evidence (the
/// run's own transcript still lives in the child's `~/.wizard` state).
fn spawn_job(entry: &ScheduleEntry, jobs_dir: &Path) -> Result<RunningJob> {
    std::fs::create_dir_all(jobs_dir)
        .with_context(|| format!("creating {}", jobs_dir.display()))?;
    let log_path = job_log_path(jobs_dir, &entry.name);
    let log = std::fs::File::create(&log_path)
        .with_context(|| format!("creating {}", log_path.display()))?;
    let log_err = log
        .try_clone()
        .with_context(|| format!("cloning the log handle for {}", log_path.display()))?;
    // Before the child exists: a budget this process cannot represent is the
    // entry's problem, not a reason to leave an orphan running.
    let budget = entry.max_duration()?;
    let child = child_command(entry)?
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .kill_on_drop(true)
        .spawn()
        .context("spawning job")?;
    prune_job_logs(jobs_dir, &entry.name, JOB_LOGS_KEEP);
    let now = Instant::now();
    Ok(RunningJob {
        name: entry.name.clone(),
        child,
        started: now,
        deadline: budget.map(|budget| now + budget + KILL_GRACE),
        log_path,
    })
}

/// Reap finished children and kill the ones past their deadline.
async fn reap_jobs(jobs: &mut Vec<RunningJob>, log_path: &Path) {
    let mut kept = Vec::with_capacity(jobs.len());
    for mut job in jobs.drain(..) {
        match job.child.try_wait() {
            Ok(Some(status)) => {
                log_line(
                    log_path,
                    &format!(
                        "finished '{}' — exit {} after {:.0}s (log {})",
                        job.name,
                        status
                            .code()
                            .map_or_else(|| "signal".to_string(), |code| code.to_string()),
                        job.started.elapsed().as_secs_f64(),
                        job.log_path.display()
                    ),
                );
            }
            Ok(None) => {
                if job
                    .deadline
                    .is_some_and(|deadline| Instant::now() >= deadline)
                {
                    job.child.kill().await.ok();
                    log_line(
                        log_path,
                        &format!(
                            "timeout '{}' — killed after {:.0}s (max_hours exceeded; log {})",
                            job.name,
                            job.started.elapsed().as_secs_f64(),
                            job.log_path.display()
                        ),
                    );
                } else {
                    kept.push(job);
                }
            }
            Err(err) => {
                log_line(
                    log_path,
                    &format!("error waiting on '{}': {err} — dropping it", job.name),
                );
            }
        }
    }
    *jobs = kept;
}

/// Take an exclusive advisory lock on `~/.wizard/scheduler.lock` so two
/// daemons can never double-fire every job. Returns the held lock file;
/// it stays open for the daemon's lifetime and the kernel releases the
/// lock on process exit (including SIGKILL), so no stale-lock cleanup is
/// ever needed.
#[cfg(unix)]
fn acquire_daemon_lock(wizard_dir: &Path) -> Result<std::fs::File> {
    use std::io::Write as _;
    use std::os::unix::io::AsRawFd;

    std::fs::create_dir_all(wizard_dir)
        .with_context(|| format!("creating {}", wizard_dir.display()))?;
    let path = wizard_dir.join("scheduler.lock");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let holder = std::fs::read_to_string(&path).unwrap_or_default();
        let holder = holder.trim();
        bail!(
            "another `wizard scheduler` is already running{} — lock held on {}",
            if holder.is_empty() {
                String::new()
            } else {
                format!(" (pid {holder})")
            },
            path.display()
        );
    }
    // Record the holder's pid for humans; the flock is the actual gate.
    file.set_len(0).ok();
    let _ = writeln!(file, "{}", std::process::id());
    Ok(file)
}

/// Windows fallback: no flock; the daemon runs unlocked (wizard's daemon
/// paths are Unix-first).
#[cfg(not(unix))]
fn acquire_daemon_lock(_wizard_dir: &Path) -> Result<()> {
    Ok(())
}

/// Service name for the scheduler daemon: `wizard-scheduler.service` under
/// systemd, `com.teddytennant.wizard.scheduler` under launchd.
pub const SERVICE_NAME: &str = "wizard-scheduler";

/// Describe the scheduler as a supervised service.
///
/// The working directory is the home directory rather than the current one,
/// which is the opposite of the gateway's choice and for the opposite reason:
/// every entry in `schedule.toml` carries its own `cwd` and the daemon
/// `chdir`s each child into it, so the daemon's own directory is never the
/// project — capturing wherever the operator happened to be standing would
/// only pin a directory that might later be deleted, taking the service with
/// it.
pub fn service_spec() -> Result<crate::platform::service::ServiceSpec> {
    let home = dirs::home_dir().context("could not determine the home directory")?;
    crate::platform::service::ServiceSpec::for_surface(
        SERVICE_NAME,
        "Wizard scheduled runs",
        "https://github.com/teddytennant/wizard/blob/main/docs/services.md",
        "wizard scheduler",
        &["scheduler"],
        Some(home),
    )
}

/// `wizard scheduler <install|start|stop|restart|status|logs|uninstall>`.
///
/// Unlike the gateway there is no credential to arrange: the daemon spawns
/// `wizard` children that load their own config, so a scheduler service that
/// starts is a scheduler service that works. An empty schedule is still worth
/// saying out loud — a daemon with nothing to fire looks identical to a broken
/// one from the outside.
pub fn run_service(cmd: crate::platform::service::ServiceCmd) -> Result<i32> {
    let spec = service_spec()?;
    if matches!(cmd, crate::platform::service::ServiceCmd::Install) {
        let path = Config::schedule_path()?;
        let entries = load_schedule(&path)
            .map(|file| file.entries.len())
            .unwrap_or(0);
        if entries == 0 {
            println!(
                "note: {} has no entries yet, so the daemon will sit idle. \
                 Add one with `wizard schedule add`.",
                path.display()
            );
        }
    }
    crate::platform::service::dispatch(&spec, cmd)
}

/// `wizard scheduler`: the foreground daemon loop. Each pass it reaps
/// children, reloads the schedule, fires every due entry (concurrently —
/// one spawn each, never serialized), then sleeps until the next fire,
/// capped at [`MAX_SLEEP`] so reloads stay timely. Ctrl-C kills running
/// jobs and exits 0. A second daemon instance exits immediately with an
/// error (advisory lock on `~/.wizard/scheduler.lock`).
pub async fn run_daemon() -> Result<i32> {
    let schedule_path = Config::schedule_path()?;
    let _lock = acquire_daemon_lock(&Config::wizard_dir()?)?;
    let logs_dir = Config::logs_dir()?;
    let jobs_dir = logs_dir.join("jobs");
    std::fs::create_dir_all(&jobs_dir)
        .with_context(|| format!("creating {}", jobs_dir.display()))?;
    let log_path = logs_dir.join("scheduler.log");

    let started = Local::now();
    let mut last_fired: HashMap<String, DateTime<Local>> = HashMap::new();
    let mut jobs: Vec<RunningJob> = Vec::new();
    log_line(
        &log_path,
        &format!(
            "scheduler started — schedule {} (missed runs are not backfilled)",
            schedule_path.display()
        ),
    );

    loop {
        reap_jobs(&mut jobs, &log_path).await;

        let entries = match load_schedule(&schedule_path) {
            Ok(file) => file.entries,
            Err(err) => {
                log_line(&log_path, &format!("schedule load failed: {err:#}"));
                Vec::new()
            }
        };

        let now = Local::now();
        let due: Vec<ScheduleEntry> = due_entries(&entries, &last_fired, &started, &now)
            .into_iter()
            .cloned()
            .collect();
        for entry in due {
            last_fired.insert(entry.name.clone(), now);
            if entry.spent_after_firing(now.with_timezone(&chrono::Utc)) {
                match remove_entry(&schedule_path, &entry.name) {
                    Ok(()) => log_line(
                        &log_path,
                        &format!("'{}' fires one last time and is removed", entry.name),
                    ),
                    Err(err) => log_line(
                        &log_path,
                        &format!("could not remove spent '{}': {err:#}", entry.name),
                    ),
                }
            }
            match spawn_job(&entry, &jobs_dir) {
                Ok(job) => {
                    log_line(
                        &log_path,
                        &format!(
                            "fired '{}' (pid {}) — {} in {}",
                            job.name,
                            job.child.id().unwrap_or(0),
                            entry.mode,
                            entry.cwd.display()
                        ),
                    );
                    jobs.push(job);
                }
                Err(err) => {
                    log_line(
                        &log_path,
                        &format!("spawn failed for '{}': {err:#}", entry.name),
                    );
                }
            }
        }

        prune_expired(&schedule_path, &entries, &log_path);

        let sleep_for = match next_fire_across(&entries, &Local::now()) {
            Some(next) => (next - Local::now())
                .to_std()
                .unwrap_or(Duration::ZERO)
                .clamp(Duration::from_secs(1), MAX_SLEEP),
            None => MAX_SLEEP,
        };
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result.context("listening for ctrl-c")?;
                log_line(
                    &log_path,
                    &format!("interrupt — stopping; killing {} running job(s)", jobs.len()),
                );
                for mut job in jobs {
                    job.child.kill().await.ok();
                    log_line(&log_path, &format!("killed '{}' on shutdown", job.name));
                }
                log_line(&log_path, "scheduler stopped");
                return Ok(0);
            }
            () = tokio::time::sleep(sleep_for) => {}
        }
    }
}

/// "in 45s" / "in 10h 32m" / "in 3d 4h" for a future delta ("now" when it
/// is not in the future).
fn humanize_until(delta: chrono::TimeDelta) -> String {
    let secs = delta.num_seconds();
    if secs <= 0 {
        return "now".to_string();
    }
    let (days, hours, mins) = (secs / 86_400, (secs % 86_400) / 3_600, (secs % 3_600) / 60);
    if days > 0 {
        format!("in {days}d {hours}h")
    } else if hours > 0 {
        format!("in {hours}h {mins}m")
    } else if mins > 0 {
        format!("in {mins}m")
    } else {
        format!("in {secs}s")
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;

    fn entry(name: &str, cron: &str) -> ScheduleEntry {
        ScheduleEntry {
            name: name.to_string(),
            cron: cron.to_string(),
            prompt: "do the thing".to_string(),
            cwd: std::env::temp_dir(),
            mode: "sovereign".to_string(),
            max_hours: None,
            enabled: true,
            once: false,
            expires_at: None,
        }
    }

    /// A schedule file with one unusable cron still loads, and the entries
    /// beside it still fire.
    ///
    /// The silent case this guards: `due_entries` drops an entry it cannot
    /// parse and says nothing, so before the warning in `load_schedule` a
    /// typo'd expression meant a job that never ran and never explained
    /// itself. Returning `Err` from the loader would have been the other
    /// wrong answer — one bad line would stop every other job from firing,
    /// which is worse than the thing being fixed.
    #[test]
    fn a_bad_cron_does_not_stop_the_schedule_loading_or_the_good_entries_firing() {
        let dir = std::env::temp_dir().join(format!("wizard-cron-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("tempdir");
        let path = dir.join("schedule.toml");

        let file = ScheduleFile {
            entries: vec![
                entry("broken", "not a cron at all"),
                entry("nightly", "0 3 * * *"),
            ],
        };
        save_schedule(&path, &file).expect("write");

        let loaded = load_schedule(&path).expect("a bad entry must not fail the load");
        assert_eq!(
            loaded.entries.len(),
            2,
            "the bad entry is kept, not dropped"
        );

        // The good one still fires; the bad one is simply never due.
        let started = utc("2026-01-01T00:00:00Z");
        let now = utc("2026-01-02T03:00:00Z");
        let due: Vec<&str> = due_entries(&loaded.entries, &Default::default(), &started, &now)
            .into_iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert!(
            due.contains(&"nightly"),
            "the good entry must still fire: {due:?}"
        );
        assert!(
            !due.contains(&"broken"),
            "the unparseable one cannot fire: {due:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every number `Duration::from_secs_f64` would panic on is refused
    /// instead, and a sane one converts.
    #[test]
    fn max_hours_is_rejected_before_it_can_panic_a_duration() {
        assert_eq!(
            max_hours_duration(2.0).expect("two hours"),
            Duration::from_secs(7200)
        );
        for bad in [-1.0, 0.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(
                max_hours_duration(bad).is_err(),
                "max_hours = {bad} must be refused, not converted"
            );
        }
        // Bounded above as well: the value also lands in `Instant + Duration`,
        // which overflows and panics on absurd inputs.
        assert!(max_hours_duration(MAX_HOURS_CAP).is_ok());
        assert!(max_hours_duration(MAX_HOURS_CAP + 1.0).is_err());
        assert!(max_hours_duration(f64::MAX).is_err());
    }

    /// A hand-edited `max_hours = -1` cannot take the daemon down.
    ///
    /// The whole path used to be: `load_schedule` warns about a bad *cron* and
    /// nothing else, `due_entries` finds the entry due, and `spawn_job` calls
    /// `Duration::from_secs_f64(-3600.0)` — which panics, in a process that is
    /// meant to run for months. So the entry has to be refused at the same
    /// seam an unusable cron is, and the conversion has to be fallible.
    #[test]
    fn a_negative_max_hours_never_fires_and_never_panics() {
        let dir = std::env::temp_dir().join(format!("wizard-hours-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("tempdir");
        let path = dir.join("schedule.toml");

        let mut bad = entry("negative", "* * * * *");
        bad.max_hours = Some(-1.0);
        let mut good = entry("nightly", "* * * * *");
        good.max_hours = Some(1.0);
        save_schedule(
            &path,
            &ScheduleFile {
                entries: vec![bad, good],
            },
        )
        .expect("write");

        let loaded = load_schedule(&path).expect("a bad entry must not fail the load");
        assert_eq!(
            loaded.entries.len(),
            2,
            "the bad entry is kept, not dropped"
        );

        let started = utc("2026-01-01T00:00:00Z");
        let now = utc("2026-01-01T00:02:00Z");
        let due: Vec<&str> = due_entries(&loaded.entries, &Default::default(), &started, &now)
            .into_iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(
            due,
            vec!["nightly"],
            "an unusable max_hours must not fire; a usable one must: {due:?}"
        );

        // And the conversion the daemon and `schedule run` both perform is an
        // error rather than a panic.
        assert!(loaded.entries[0].max_duration().is_err());
        assert_eq!(
            loaded.entries[1].max_duration().expect("one hour"),
            Some(Duration::from_secs(3600))
        );
        // `schedule add` refuses to write one in the first place.
        let mut rejected = entry("nan", "* * * * *");
        rejected.max_hours = Some(f64::NAN);
        assert!(validate_entry(&rejected).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().expect("test timestamp parses")
    }

    /// A one-shot or an expired entry is gone after the pass that fires it, and
    /// the two fields survive the round trip through the file.
    #[test]
    fn once_and_expires_at_retire_an_entry_and_round_trip() {
        let now = utc("2026-01-10T00:00:00Z");
        let mut plain = entry("plain", "* * * * *");
        assert!(!plain.spent_after_firing(now));

        plain.once = true;
        assert!(plain.spent_after_firing(now));

        let mut aging = entry("aging", "* * * * *");
        aging.expires_at = Some(utc("2026-01-11T00:00:00Z"));
        assert!(!aging.spent_after_firing(now));
        assert!(aging.spent_after_firing(utc("2026-01-11T00:00:01Z")));
        assert!(aging.is_expired(utc("2026-01-11T00:00:00Z")));

        let dir = std::env::temp_dir().join(format!("wizard-once-{}", uuid::Uuid::new_v4()));
        let path = dir.join("schedule.toml");
        save_schedule(
            &path,
            &ScheduleFile {
                entries: vec![plain, aging],
            },
        )
        .expect("write");
        let loaded = load_schedule(&path).expect("read back");
        assert!(loaded.entries[0].once);
        assert_eq!(
            loaded.entries[1].expires_at,
            Some(utc("2026-01-11T00:00:00Z"))
        );
        let text = std::fs::read_to_string(&path).expect("file");
        assert_eq!(
            text.matches("once").count(),
            1,
            "false is not written: {text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An expired entry nothing was due for is pruned; a live one stays.
    #[test]
    fn prune_expired_removes_only_the_expired() {
        let dir = std::env::temp_dir().join(format!("wizard-prune-{}", uuid::Uuid::new_v4()));
        let path = dir.join("schedule.toml");
        let mut old = entry("old", "0 3 * * *");
        old.expires_at = Some(utc("2020-01-01T00:00:00Z"));
        let live = entry("live", "0 3 * * *");
        let entries = vec![old, live];
        save_schedule(
            &path,
            &ScheduleFile {
                entries: entries.clone(),
            },
        )
        .expect("write");
        prune_expired(&path, &entries, &dir.join("scheduler.log"));
        let left = load_schedule(&path).expect("read back").entries;
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].name, "live");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cron_validation_accepts_standard_five_fields() {
        for ok in ["0 3 * * *", "*/5 * * * *", "0 0 1 1 0", "30 6 * * MON-FRI"] {
            assert!(parse_cron(ok).is_ok(), "{ok:?} must parse");
        }
    }

    #[test]
    fn cron_validation_rejects_garbage_and_wrong_field_counts() {
        for bad in ["", "not a cron", "0 3 * *", "0 0 3 * * *", "61 * * * *"] {
            assert!(parse_cron(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn next_occurrence_from_a_known_instant() {
        let cron = parse_cron("0 3 * * *").expect("parses");
        let now = utc("2026-01-01T00:00:00Z");
        let next = cron.find_next_occurrence(&now, false).expect("next");
        assert_eq!(next, utc("2026-01-01T03:00:00Z"));

        // Strictly after: exactly at the fire instant the next one is tomorrow.
        let at_fire = utc("2026-01-01T03:00:00Z");
        let next = cron.find_next_occurrence(&at_fire, false).expect("next");
        assert_eq!(next, utc("2026-01-02T03:00:00Z"));
    }

    #[test]
    fn schedule_file_round_trip_add_remove() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("schedule.toml");

        add_entry(&path, entry("nightly", "0 3 * * *")).expect("first add");
        add_entry(&path, entry("hourly", "0 * * * *")).expect("second add");

        let file = load_schedule(&path).expect("loads");
        assert_eq!(file.entries.len(), 2);
        assert_eq!(file.entries[0].name, "nightly");
        assert_eq!(file.entries[0].cron, "0 3 * * *");
        assert_eq!(file.entries[0].mode, "sovereign");
        assert!(file.entries[0].enabled, "enabled defaults to true");

        // Duplicate names are rejected.
        let err = add_entry(&path, entry("nightly", "0 4 * * *")).unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");

        remove_entry(&path, "nightly").expect("removes");
        let file = load_schedule(&path).expect("loads");
        assert_eq!(file.entries.len(), 1);
        assert_eq!(file.entries[0].name, "hourly");

        let err = remove_entry(&path, "nightly").unwrap_err();
        assert!(err.to_string().contains("no entry"), "{err}");
    }

    #[test]
    fn set_enabled_toggles_and_errors_on_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("schedule.toml");
        add_entry(&path, entry("nightly", "0 3 * * *")).expect("add");

        set_enabled(&path, "nightly", false).expect("disable");
        assert!(!load_schedule(&path).unwrap().entries[0].enabled);

        // Idempotent: disabling again still succeeds.
        set_enabled(&path, "nightly", false).expect("disable again");

        set_enabled(&path, "nightly", true).expect("enable");
        assert!(load_schedule(&path).unwrap().entries[0].enabled);

        let err = set_enabled(&path, "absent", true).unwrap_err();
        assert!(err.to_string().contains("no entry"), "{err}");
    }

    #[test]
    fn job_logs_are_pruned_per_entry_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let jobs = dir.path().join("jobs");
        std::fs::create_dir_all(&jobs).unwrap();
        for i in 0..5 {
            std::fs::write(jobs.join(format!("nightly-2026010{i}-000000.log")), "x").unwrap();
        }
        // Another entry's logs are untouched.
        std::fs::write(jobs.join("other-20260101-000000.log"), "x").unwrap();
        // Non-log files are untouched.
        std::fs::write(jobs.join("nightly-notes.txt"), "x").unwrap();

        prune_job_logs(&jobs, "nightly", 2);
        let mut names: Vec<String> = std::fs::read_dir(&jobs)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "nightly-20260103-000000.log",
                "nightly-20260104-000000.log",
                "nightly-notes.txt",
                "other-20260101-000000.log",
            ],
            "keeps the newest two nightly logs"
        );

        // Pruning a missing dir is a no-op.
        prune_job_logs(&dir.path().join("absent"), "nightly", 2);
    }

    #[test]
    fn job_log_path_is_name_timestamped() {
        let path = job_log_path(Path::new("/tmp/jobs"), "nightly");
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("nightly-"), "{name}");
        assert!(name.ends_with(".log"), "{name}");
    }

    #[test]
    fn missing_schedule_file_is_an_empty_schedule() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = load_schedule(&dir.path().join("absent.toml")).expect("loads");
        assert!(file.entries.is_empty());
    }

    #[test]
    fn add_rejects_invalid_cron_name_mode_and_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("schedule.toml");

        let mut bad_cron = entry("a", "whenever");
        bad_cron.cron = "whenever".to_string();
        assert!(add_entry(&path, bad_cron).is_err());

        let bad_name = entry("no spaces", "0 3 * * *");
        assert!(add_entry(&path, bad_name).is_err());

        let mut bad_mode = entry("b", "0 3 * * *");
        bad_mode.mode = "genie".to_string();
        assert!(add_entry(&path, bad_mode).is_err());

        let mut bad_cwd = entry("c", "0 3 * * *");
        bad_cwd.cwd = dir.path().join("does-not-exist");
        assert!(add_entry(&path, bad_cwd).is_err());

        let mut bad_hours = entry("d", "0 3 * * *");
        bad_hours.max_hours = Some(0.0);
        assert!(add_entry(&path, bad_hours).is_err());

        assert!(
            load_schedule(&path).expect("loads").entries.is_empty(),
            "nothing invalid may be persisted"
        );
    }

    #[test]
    fn legacy_minimal_entry_parses_with_defaults() {
        let text = "[[entries]]\n\
                    name = \"n\"\n\
                    cron = \"0 3 * * *\"\n\
                    prompt = \"p\"\n\
                    cwd = \"/tmp\"\n";
        let file: ScheduleFile = toml::from_str(text).expect("parses");
        assert_eq!(file.entries[0].mode, "sovereign");
        assert_eq!(file.entries[0].max_hours, None);
        assert!(file.entries[0].enabled);
    }

    #[test]
    fn due_detection_fires_after_an_occurrence_passes() {
        let entries = vec![entry("minutely", "* * * * *")];
        let started = utc("2026-01-01T00:00:30Z");
        let mut last_fired: HashMap<String, DateTime<Utc>> = HashMap::new();

        // 10 seconds in: the first occurrence after start (00:01:00) has not
        // passed yet.
        let now = utc("2026-01-01T00:00:40Z");
        assert!(due_entries(&entries, &last_fired, &started, &now).is_empty());

        // Past 00:01:00: due exactly once.
        let now = utc("2026-01-01T00:01:05Z");
        let due = due_entries(&entries, &last_fired, &started, &now);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].name, "minutely");

        // After recording the fire, the same instant is no longer due...
        last_fired.insert("minutely".to_string(), now);
        assert!(due_entries(&entries, &last_fired, &started, &now).is_empty());

        // ...and several missed occurrences collapse into one fire.
        let later = utc("2026-01-01T00:05:30Z");
        let due = due_entries(&entries, &last_fired, &started, &later);
        assert_eq!(due.len(), 1);
    }

    #[test]
    fn due_detection_skips_disabled_and_invalid_entries() {
        let mut disabled = entry("off", "* * * * *");
        disabled.enabled = false;
        let mut invalid = entry("broken", "* * * * *");
        invalid.cron = "not a cron".to_string();
        let entries = vec![disabled, invalid, entry("on", "* * * * *")];

        let started = utc("2026-01-01T00:00:00Z");
        let now = utc("2026-01-01T00:02:00Z");
        let due = due_entries(&entries, &HashMap::new(), &started, &now);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].name, "on");
    }

    #[test]
    fn due_detection_does_not_backfill_before_startup() {
        // Daily 03:00 job; daemon starts at 10:00 — yesterday's and today's
        // 03:00 are gone, the entry is not due until tomorrow 03:00.
        let entries = vec![entry("daily", "0 3 * * *")];
        let started = utc("2026-01-01T10:00:00Z");
        let now = utc("2026-01-01T23:59:00Z");
        assert!(due_entries(&entries, &HashMap::new(), &started, &now).is_empty());

        let tomorrow = utc("2026-01-02T03:00:00Z");
        let due = due_entries(&entries, &HashMap::new(), &started, &tomorrow);
        assert_eq!(due.len(), 1);
    }

    #[test]
    fn next_fire_across_picks_the_earliest_enabled_entry() {
        let mut late = entry("late", "0 12 * * *");
        late.enabled = true;
        let early = entry("early", "0 6 * * *");
        let mut disabled = entry("disabled", "0 1 * * *");
        disabled.enabled = false;
        let entries = vec![late, early, disabled];

        let now = utc("2026-01-01T00:00:00Z");
        let next = next_fire_across(&entries, &now).expect("a next fire exists");
        assert_eq!(next, utc("2026-01-01T06:00:00Z"));

        assert!(next_fire_across(&[], &now).is_none());
    }

    #[test]
    fn child_args_sovereign_with_cap() {
        let mut e = entry("n", "0 3 * * *");
        e.prompt = "tidy the repo".to_string();
        e.cwd = PathBuf::from("/home/user/proj");
        e.max_hours = Some(2.0);
        assert_eq!(
            child_args(&e),
            vec![
                "--mode",
                "sovereign",
                "-p",
                "tidy the repo",
                "--cwd",
                "/home/user/proj",
                "--max-hours",
                "2",
            ]
        );
    }

    #[test]
    fn child_args_continuous_without_cap() {
        let mut e = entry("n", "0 3 * * *");
        e.mode = "continuous".to_string();
        e.prompt = "keep improving".to_string();
        e.cwd = PathBuf::from("/srv/app");
        assert_eq!(
            child_args(&e),
            vec!["--continuous", "-p", "keep improving", "--cwd", "/srv/app"]
        );
    }

    #[test]
    fn humanize_until_buckets() {
        use chrono::TimeDelta;
        assert_eq!(humanize_until(TimeDelta::seconds(-5)), "now");
        assert_eq!(humanize_until(TimeDelta::seconds(45)), "in 45s");
        assert_eq!(humanize_until(TimeDelta::seconds(150)), "in 2m");
        assert_eq!(humanize_until(TimeDelta::seconds(37_920)), "in 10h 32m");
        assert_eq!(humanize_until(TimeDelta::seconds(273_600)), "in 3d 4h");
    }
}
