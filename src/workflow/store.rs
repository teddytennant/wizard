//! Where workflow runs and named workflows live on disk.
//!
//! A run is a directory, `<wizard dir>/workflows/runs/<run id>/`:
//!
//! - `run.json`: the [`RunRecord`], rewritten as the run moves
//! - `script.js`: the script exactly as launched
//! - `journal.jsonl`: one line per finished `agent()` call, the cache `resumeFromRunId` reads
//!
//! A named workflow is a `*.js` file in `.wizard/workflows/` or
//! `.claude/workflows/`, in the project or its ancestors, or in the same
//! directories under the home directory.

use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::script;

/// A fresh run id in Claude's shape, `wf_` and eight hex digits, a dash, three more.
pub fn new_run_id() -> String {
    let hex = uuid::Uuid::new_v4().simple().to_string();
    format!("wf_{}-{}", &hex[..8], &hex[8..11])
}

/// Whether `id` could be a run id. Guards the path join.
pub fn valid_run_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix("wf_") else {
        return false;
    };
    rest.len() >= 6
        && rest
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Where a run ended up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Running,
    Completed,
    Failed,
    Stopped,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Completed => "completed",
            Status::Failed => "failed",
            Status::Stopped => "stopped",
        }
    }
}

/// One run, as `/workflows` shows it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub run_id: String,
    pub name: String,
    pub description: String,
    pub status: Status,
    pub started_ms: u64,
    pub ended_ms: Option<u64>,
    pub cwd: String,
    /// `inline`, `name:<name>` or `path:<file>`.
    pub source: String,
    pub args: Value,
    /// Calls to `agent()` so far, cached ones included.
    pub agents: u32,
    /// Of those, how many came back from the journal.
    pub cached: u32,
    pub phases: Vec<String>,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub failures: Vec<String>,
    pub pid: u32,
    /// The background task id the run holds in its session.
    pub task_id: Option<u32>,
}

/// One finished `agent()` call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub i: u32,
    pub key: String,
    pub label: String,
    pub phase: Option<String>,
    pub ok: bool,
    pub result: Value,
    pub error: Option<String>,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub fn runs_root() -> Result<PathBuf> {
    Ok(crate::config::Config::wizard_dir()?
        .join("workflows")
        .join("runs"))
}

pub fn run_dir(run_id: &str) -> Result<PathBuf> {
    anyhow::ensure!(valid_run_id(run_id), "{run_id:?} is not a workflow run id");
    Ok(runs_root()?.join(run_id))
}

/// A stable digest of what an `agent()` call asks for.
///
/// Label and phase are left out: they change how the call is shown, not what it
/// does, and renaming a label should not re-run the work.
pub fn call_key(prompt: &str, relevant: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prompt.as_bytes());
    hasher.update([0]);
    hasher.update(canonical(relevant).as_bytes());
    let digest = hasher.finalize();
    digest.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

fn canonical(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let inner: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{}:{}", Value::String(k.clone()), canonical(&map[k])))
                .collect();
            format!("{{{}}}", inner.join(","))
        }
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(canonical).collect();
            format!("[{}]", inner.join(","))
        }
        other => other.to_string(),
    }
}

/// Write the run's script and first record.
pub fn create(record: &RunRecord, script_text: &str) -> Result<()> {
    let dir = run_dir(&record.run_id)?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::write(dir.join("script.js"), script_text)?;
    write_record(record)
}

pub fn script_path(run_id: &str) -> Result<PathBuf> {
    Ok(run_dir(run_id)?.join("script.js"))
}

pub fn journal_path(run_id: &str) -> Result<PathBuf> {
    Ok(run_dir(run_id)?.join("journal.jsonl"))
}

/// Rewrite `run.json` through a temp file so a reader never sees half of it.
pub fn write_record(record: &RunRecord) -> Result<()> {
    let dir = run_dir(&record.run_id)?;
    let tmp = dir.join("run.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(record)?)?;
    std::fs::rename(&tmp, dir.join("run.json"))?;
    Ok(())
}

pub fn load(run_id: &str) -> Result<RunRecord> {
    let path = run_dir(run_id)?.join("run.json");
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("no workflow run {run_id} ({})", path.display()))?;
    Ok(serde_json::from_str(&raw)?)
}

pub fn append_journal(run_id: &str, entry: &JournalEntry) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(journal_path(run_id)?)?;
    writeln!(file, "{}", serde_json::to_string(entry)?)?;
    Ok(())
}

/// The journal, lines that do not parse skipped.
pub fn read_journal(run_id: &str) -> Vec<JournalEntry> {
    let Ok(path) = journal_path(run_id) else {
        return Vec::new();
    };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    raw.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// A run id a user typed, which may be a prefix of one.
pub fn resolve_id(typed: &str) -> Result<String, String> {
    let typed = typed.trim();
    if typed.is_empty() {
        return Err("name a run id (see /workflows)".into());
    }
    let all = list();
    if let Some(exact) = all.iter().find(|r| r.run_id == typed) {
        return Ok(exact.run_id.clone());
    }
    let hits: Vec<&RunRecord> = all
        .iter()
        .filter(|r| r.run_id.starts_with(typed) || r.run_id.starts_with(&format!("wf_{typed}")))
        .collect();
    match hits.as_slice() {
        [] => Err(format!("no workflow run matches '{typed}'")),
        [one] => Ok(one.run_id.clone()),
        many => Err(format!(
            "'{typed}' matches {} runs; give more of the id",
            many.len()
        )),
    }
}

/// Every run on disk, newest first.
pub fn list() -> Vec<RunRecord> {
    let Ok(root) = runs_root() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut runs: Vec<RunRecord> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| std::fs::read_to_string(e.path().join("run.json")).ok())
        .filter_map(|raw| serde_json::from_str(&raw).ok())
        .collect();
    runs.sort_by_key(|run| std::cmp::Reverse(run.started_ms));
    runs
}

#[cfg(unix)]
pub fn pid_alive(pid: u32) -> bool {
    // Signal 0 checks that the process exists and can be signalled.
    pid != 0 && unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(not(unix))]
pub fn pid_alive(pid: u32) -> bool {
    pid == std::process::id()
}

/// The status to show for a record: a run marked running whose process is gone
/// was cut off.
pub fn effective_status(record: &RunRecord) -> &'static str {
    if record.status == Status::Running
        && !super::live::is_live(&record.run_id)
        && !(record.pid != std::process::id() && pid_alive(record.pid))
    {
        return "interrupted";
    }
    record.status.label()
}

/// Where a named workflow came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Project,
    User,
}

impl Origin {
    pub fn label(self) -> &'static str {
        match self {
            Origin::Project => "project",
            Origin::User => "user",
        }
    }
}

/// A workflow file found in a workflows directory.
#[derive(Debug, Clone)]
pub struct Named {
    pub name: String,
    pub path: PathBuf,
    pub origin: Origin,
    pub meta: script::Meta,
}

/// File names a workflow may have, Claude's rule.
pub fn valid_file_name(file: &str) -> bool {
    file.strip_suffix(".js").is_some_and(|stem| {
        !stem.is_empty()
            && stem
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    })
}

/// The directories to look in, closest first: each ancestor of `cwd` that has
/// them (`.wizard` before `.claude`), then the home directory's.
pub fn search_dirs(cwd: &Path, home: Option<&Path>) -> Vec<(PathBuf, Origin)> {
    let mut dirs = Vec::new();
    let mut seen = HashSet::new();
    for ancestor in cwd.ancestors() {
        if Some(ancestor) == home {
            break;
        }
        for sub in [".wizard", ".claude"] {
            let dir = ancestor.join(sub).join("workflows");
            if dir.is_dir() && seen.insert(dir.clone()) {
                dirs.push((dir, Origin::Project));
            }
        }
    }
    if let Some(home) = home {
        for sub in [".wizard", ".claude"] {
            let dir = home.join(sub).join("workflows");
            if dir.is_dir() && seen.insert(dir.clone()) {
                dirs.push((dir, Origin::User));
            }
        }
    }
    dirs
}

/// Every named workflow visible from `cwd`, the closest definition of a name
/// winning, with a line for each file that could not be loaded.
pub fn discover(cwd: &Path, home: Option<&Path>) -> (Vec<Named>, Vec<String>) {
    let mut found: Vec<Named> = Vec::new();
    let mut problems = Vec::new();
    for (dir, origin) in search_dirs(cwd, home) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        files.sort();
        for path in files {
            let file = path.file_name().and_then(|f| f.to_str()).unwrap_or("");
            if !valid_file_name(file) {
                continue;
            }
            let loaded = std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|text| script::parse(&text));
            match loaded {
                Ok(parsed) => {
                    let name = parsed.meta.name.clone();
                    if !found.iter().any(|n| n.name == name) {
                        found.push(Named {
                            name,
                            path,
                            origin,
                            meta: parsed.meta,
                        });
                    }
                }
                Err(why) => problems.push(format!("{}: {why}", path.display())),
            }
        }
    }
    (found, problems)
}

/// The workflow called `name`, by its `meta.name` or its file stem.
pub fn find_named(cwd: &Path, home: Option<&Path>, name: &str) -> Option<Named> {
    let (found, _) = discover(cwd, home);
    found.into_iter().find(|n| {
        n.name == name
            || n.path
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s == name)
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wizard-wf-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const SCRIPT: &str = "export const meta = {name: 'wf-name', description: 'desc'}\nreturn 1\n";

    fn record(id: &str) -> RunRecord {
        RunRecord {
            run_id: id.into(),
            name: "n".into(),
            description: "d".into(),
            status: Status::Running,
            started_ms: now_ms(),
            ended_ms: None,
            cwd: ".".into(),
            source: "inline".into(),
            args: Value::Null,
            agents: 0,
            cached: 0,
            phases: vec![],
            result: None,
            error: None,
            failures: vec![],
            pid: std::process::id(),
            task_id: None,
        }
    }

    #[test]
    fn run_ids_have_claudes_shape_and_are_safe_to_join() {
        let id = new_run_id();
        assert!(valid_run_id(&id), "{id}");
        assert_eq!(id.len(), "wf_".len() + 8 + 1 + 3);
        assert!(!valid_run_id("../etc"));
        assert!(!valid_run_id("wf_../x"));
        assert!(!valid_run_id("abc"));
    }

    #[test]
    fn a_record_and_its_journal_round_trip() {
        let id = new_run_id();
        create(&record(&id), SCRIPT).unwrap();
        assert_eq!(load(&id).unwrap().name, "n");
        assert_eq!(
            std::fs::read_to_string(script_path(&id).unwrap()).unwrap(),
            SCRIPT
        );
        for i in 0..3 {
            append_journal(
                &id,
                &JournalEntry {
                    i,
                    key: format!("k{i}"),
                    label: "l".into(),
                    phase: None,
                    ok: i != 1,
                    result: json!({"n": i}),
                    error: None,
                },
            )
            .unwrap();
        }
        let journal = read_journal(&id);
        assert_eq!(journal.len(), 3);
        assert!(!journal[1].ok);
        assert!(list().iter().any(|r| r.run_id == id));
        assert_eq!(resolve_id(&id[3..8]).unwrap(), id);
        assert!(resolve_id("zzzz").is_err());
    }

    #[test]
    fn keys_ignore_object_order_and_change_with_the_work() {
        let a = call_key("p", &json!({"a": 1, "b": 2}));
        let b = call_key("p", &json!({"b": 2, "a": 1}));
        assert_eq!(a, b);
        assert_ne!(a, call_key("p2", &json!({"a": 1, "b": 2})));
        assert_ne!(a, call_key("p", &json!({"a": 1, "b": 3})));
    }

    #[test]
    fn the_closest_definition_wins_and_project_beats_user() {
        let root = temp("discover");
        let home = root.join("home");
        let project = root.join("home/work/proj");
        let inner = project.join("pkg");
        std::fs::create_dir_all(&inner).unwrap();
        let write = |dir: &Path, sub: &str, file: &str, name: &str| {
            let d = dir.join(sub).join("workflows");
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join(file),
                format!("export const meta = {{name: '{name}', description: '{sub}'}}\nreturn 1"),
            )
            .unwrap();
        };
        write(&home, ".claude", "shared.js", "shared");
        write(&project, ".claude", "shared.js", "shared");
        write(&inner, ".wizard", "shared.js", "shared");
        write(&project, ".claude", "only-claude.js", "only-claude");
        write(&home, ".wizard", "mine.js", "mine");
        std::fs::write(project.join(".claude/workflows/Bad Name.js"), "x").unwrap();
        std::fs::write(project.join(".claude/workflows/broken.js"), "return 1").unwrap();

        let (found, problems) = discover(&inner, Some(&home));
        let names: Vec<&str> = found.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["shared", "only-claude", "mine"]);
        assert_eq!(found[0].meta.description, ".wizard");
        assert_eq!(found[0].origin, Origin::Project);
        assert_eq!(found[2].origin, Origin::User);
        assert!(
            problems.iter().any(|p| p.contains("broken.js")),
            "{problems:?}"
        );
        assert!(find_named(&inner, Some(&home), "mine").is_some());
        assert!(find_named(&inner, Some(&home), "nope").is_none());
        let _ = std::fs::remove_dir_all(root);
    }
}
