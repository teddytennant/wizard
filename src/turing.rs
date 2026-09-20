//! Preference log of the user's judgments.
//!
//! Same on-disk JSONL as the standalone `turing` CLI (`SCHEMA.md` in
//! github.com/teddytennant/turing). Compile is offline (keyword overlap, then
//! recency). This is not a persona and not [`crate::memory`].

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::MutexGuard;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::commands::TuringAction;

const SCHEMA_V: u32 = 1;
const COMPILE_LIMIT: usize = 12;

/// Process-wide override so tests never touch `~/.local/state/turing`.
static LOG_OVERRIDE: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

fn override_slot() -> &'static Mutex<Option<PathBuf>> {
    LOG_OVERRIDE.get_or_init(|| Mutex::new(None))
}

/// JSONL preference log. Default: `$TURING_STATE_DIR/log.jsonl`, else
/// `~/.local/state/turing/log.jsonl`.
pub fn log_path() -> PathBuf {
    if let Ok(guard) = override_slot().lock() {
        if let Some(path) = guard.as_ref() {
            return path.clone();
        }
    }
    if let Ok(dir) = std::env::var("TURING_STATE_DIR") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return PathBuf::from(dir).join("log.jsonl");
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local/state/turing/log.jsonl")
}

/// One append-only row. Tagged `kind`, same shape as the CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Row {
    Question {
        v: u32,
        id: String,
        ts: String,
        text: String,
        status: String,
    },
    Answer {
        v: u32,
        id: String,
        ts: String,
        qid: String,
        text: String,
    },
    Pair {
        v: u32,
        id: String,
        ts: String,
        a: String,
        b: String,
        winner: String,
        why: String,
    },
    Veto {
        v: u32,
        id: String,
        ts: String,
        target: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        excerpt: Option<String>,
        why: String,
    },
    Endorse {
        v: u32,
        id: String,
        ts: String,
        target: String,
    },
    Attractor {
        v: u32,
        id: String,
        ts: String,
        skeleton: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
}

impl Row {
    pub fn question(text: impl Into<String>) -> Self {
        Row::Question {
            v: SCHEMA_V,
            id: fresh_id(),
            ts: now_rfc3339(),
            text: text.into(),
            status: "open".into(),
        }
    }

    pub fn answer(qid: impl Into<String>, text: impl Into<String>) -> Self {
        Row::Answer {
            v: SCHEMA_V,
            id: fresh_id(),
            ts: now_rfc3339(),
            qid: qid.into(),
            text: text.into(),
        }
    }

    pub fn pair(
        a: impl Into<String>,
        b: impl Into<String>,
        winner: &str,
        why: impl Into<String>,
    ) -> Result<Self> {
        let winner = winner.trim().to_ascii_lowercase();
        if winner != "a" && winner != "b" {
            bail!("winner must be a or b");
        }
        let why = why.into();
        if why.trim().is_empty() {
            bail!("pair requires a why");
        }
        Ok(Row::Pair {
            v: SCHEMA_V,
            id: fresh_id(),
            ts: now_rfc3339(),
            a: a.into(),
            b: b.into(),
            winner,
            why,
        })
    }

    pub fn veto(
        target: impl Into<String>,
        why: impl Into<String>,
        excerpt: Option<String>,
    ) -> Result<Self> {
        let why = why.into();
        if why.trim().is_empty() {
            bail!("veto requires a why");
        }
        Ok(Row::Veto {
            v: SCHEMA_V,
            id: fresh_id(),
            ts: now_rfc3339(),
            target: target.into(),
            excerpt: nonempty(excerpt),
            why,
        })
    }

    pub fn endorse(target: impl Into<String>) -> Self {
        Row::Endorse {
            v: SCHEMA_V,
            id: fresh_id(),
            ts: now_rfc3339(),
            target: target.into(),
        }
    }

    pub fn attractor(skeleton: impl Into<String>, note: Option<String>) -> Result<Self> {
        let skeleton = skeleton.into();
        if skeleton.trim().is_empty() {
            bail!("attractor requires a skeleton");
        }
        Ok(Row::Attractor {
            v: SCHEMA_V,
            id: fresh_id(),
            ts: now_rfc3339(),
            skeleton,
            note: nonempty(note),
        })
    }

    pub fn id(&self) -> &str {
        match self {
            Row::Question { id, .. }
            | Row::Answer { id, .. }
            | Row::Pair { id, .. }
            | Row::Veto { id, .. }
            | Row::Endorse { id, .. }
            | Row::Attractor { id, .. } => id,
        }
    }

    pub fn ts(&self) -> &str {
        match self {
            Row::Question { ts, .. }
            | Row::Answer { ts, .. }
            | Row::Pair { ts, .. }
            | Row::Veto { ts, .. }
            | Row::Endorse { ts, .. }
            | Row::Attractor { ts, .. } => ts,
        }
    }

    pub fn kind_name(&self) -> &'static str {
        match self {
            Row::Question { .. } => "question",
            Row::Answer { .. } => "answer",
            Row::Pair { .. } => "pair",
            Row::Veto { .. } => "veto",
            Row::Endorse { .. } => "endorse",
            Row::Attractor { .. } => "attractor",
        }
    }

    fn searchable_text(&self) -> String {
        match self {
            Row::Question { text, .. } | Row::Answer { text, .. } => text.clone(),
            Row::Pair { a, b, why, .. } => format!("{a} {b} {why}"),
            Row::Veto {
                target,
                excerpt,
                why,
                ..
            } => match excerpt {
                Some(excerpt) => format!("{target} {excerpt} {why}"),
                None => format!("{target} {why}"),
            },
            Row::Endorse { target, .. } => target.clone(),
            Row::Attractor { skeleton, note, .. } => match note {
                Some(note) => format!("{skeleton} {note}"),
                None => skeleton.clone(),
            },
        }
    }
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.and_then(|s| {
        let s = s.trim();
        if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        }
    })
}

fn fresh_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn answered_qids(rows: &[Row]) -> HashSet<&str> {
    rows.iter()
        .filter_map(|row| match row {
            Row::Answer { qid, .. } => Some(qid.as_str()),
            _ => None,
        })
        .collect()
}

fn open_questions(rows: &[Row]) -> Vec<&Row> {
    let answered = answered_qids(rows);
    rows.iter()
        .filter(|row| match row {
            Row::Question { id, .. } => !answered.contains(id.as_str()),
            _ => false,
        })
        .collect()
}

/// Append-only JSONL at a fixed path.
#[derive(Debug, Clone)]
pub struct Log {
    path: PathBuf,
}

impl Log {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
        }
        Ok(Log { path })
    }

    pub fn default_log() -> Result<Self> {
        Log::open(log_path())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, row: &Row) -> Result<()> {
        let line = serde_json::to_string(row).context("serializing turing row")?;
        if line.contains('\n') {
            bail!("refusing to append a row that contains a newline");
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;
        writeln!(file, "{line}")
            .with_context(|| format!("appending to {}", self.path.display()))?;
        file.sync_data()
            .with_context(|| format!("syncing {}", self.path.display()))?;
        Ok(())
    }

    pub fn load(&self) -> Result<Vec<Row>> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(err).with_context(|| format!("reading {}", self.path.display()));
            }
        };
        let mut rows = Vec::new();
        for (idx, line) in BufReader::new(file).lines().enumerate() {
            let line =
                line.with_context(|| format!("reading {} line {}", self.path.display(), idx + 1))?;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let row: Row = serde_json::from_str(line)
                .with_context(|| format!("parsing {} line {}", self.path.display(), idx + 1))?;
            rows.push(row);
        }
        Ok(rows)
    }
}

/// Slash `/turing` handler. Failures stay in the returned string.
pub fn report(action: &TuringAction) -> String {
    match run(action) {
        Ok(text) => text,
        Err(err) => format!("turing: {err}"),
    }
}

pub(crate) fn run(action: &TuringAction) -> Result<String> {
    let log = Log::default_log()?;
    match action {
        TuringAction::Status => status(&log),
        TuringAction::Compile(task) => {
            let rows = log.load()?;
            Ok(compile_slice(&rows, task))
        }
        TuringAction::Question => question_text(&log),
        TuringAction::Answer { id, text } => {
            if text.trim().is_empty() {
                bail!("answer text is empty");
            }
            let rows = log.load()?;
            let exists = rows
                .iter()
                .any(|row| matches!(row, Row::Question { id: qid, .. } if qid == id));
            if !exists {
                bail!("no question {id}");
            }
            let row = Row::answer(id.clone(), text.clone());
            log.append(&row)?;
            Ok(format!("answered {id}"))
        }
        TuringAction::Veto {
            target,
            why,
            excerpt,
        } => {
            let row = Row::veto(target.clone(), why.clone(), excerpt.clone())?;
            log.append(&row)?;
            Ok(format!("vetoed {target}"))
        }
        TuringAction::Endorse { target } => {
            if target.trim().is_empty() {
                bail!("endorse target is empty");
            }
            log.append(&Row::endorse(target.clone()))?;
            Ok(format!("endorsed {target}"))
        }
        TuringAction::Pair { a, b, winner, why } => {
            let row = Row::pair(a.clone(), b.clone(), winner, why.clone())?;
            log.append(&row)?;
            Ok(format!("pair {}", row.id()))
        }
        TuringAction::Attractor { skeleton, note } => {
            let row = Row::attractor(skeleton.clone(), note.clone())?;
            log.append(&row)?;
            Ok(format!("attractor {}", row.id()))
        }
    }
}

fn status(log: &Log) -> Result<String> {
    let rows = log.load()?;
    let mut counts: HashMap<&'static str, usize> = HashMap::new();
    for row in &rows {
        *counts.entry(row.kind_name()).or_default() += 1;
    }
    let open = open_questions(&rows);
    let mut out = format!("log: {}\nrows: {}\n", log.path().display(), rows.len());
    for kind in ["question", "answer", "pair", "veto", "endorse", "attractor"] {
        out.push_str(&format!(
            "{kind}: {}\n",
            counts.get(kind).copied().unwrap_or(0)
        ));
    }
    out.push_str(&format!("open questions: {}\n", open.len()));
    for row in open.iter().take(5) {
        if let Row::Question { id, text, .. } = row {
            out.push_str(&format!("  {id}: {}\n", truncate(text, 80)));
        }
    }
    Ok(out)
}

fn question_text(log: &Log) -> Result<String> {
    let rows = log.load()?;
    match open_questions(&rows).into_iter().next() {
        Some(Row::Question { id, text, .. }) => Ok(format!("id {id}\n{text}")),
        _ => Ok("no open questions".to_string()),
    }
}

/// Rank `rows` for `task` and render a short markdown slice.
pub fn compile_slice(rows: &[Row], task: &str) -> String {
    render(task, &rank(rows, task))
}

fn rank<'a>(rows: &'a [Row], task: &str) -> Vec<&'a Row> {
    let answered = answered_qids(rows);
    let task_tokens = tokenize(task);
    let mut scored: Vec<(u32, i64, usize, &'a Row)> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| match row {
            Row::Question { id, .. } => !answered.contains(id.as_str()),
            _ => true,
        })
        .map(|(idx, row)| {
            let overlap = overlap_count(&tokenize(&row.searchable_text()), &task_tokens);
            let recency = parse_ts(row.ts());
            (overlap, recency, idx, row)
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(b.2.cmp(&a.2)));
    scored
        .into_iter()
        .take(COMPILE_LIMIT)
        .map(|(_, _, _, row)| row)
        .collect()
}

fn render(task: &str, rows: &[&Row]) -> String {
    let mut out = String::from("# turing\n\n");
    let task = task.trim();
    if !task.is_empty() {
        out.push_str("task: ");
        out.push_str(task);
        out.push_str("\n\n");
    }
    if rows.is_empty() {
        out.push_str("no rows\n");
        return out;
    }
    for row in rows {
        match row {
            Row::Question { id, ts, text, .. } => {
                out.push_str(&format!("## question (open, id {id})\n{ts}\n"));
                out.push_str(text.trim());
                out.push_str("\n\n");
            }
            Row::Answer { qid, ts, text, .. } => {
                out.push_str(&format!("## answer (qid {qid})\n{ts}\n"));
                out.push_str(text.trim());
                out.push_str("\n\n");
            }
            Row::Pair {
                ts,
                a,
                b,
                winner,
                why,
                ..
            } => {
                out.push_str(&format!("## pair, picked {winner}\n{ts}\n"));
                out.push_str("a: ");
                out.push_str(a.trim());
                out.push('\n');
                out.push_str("b: ");
                out.push_str(b.trim());
                out.push('\n');
                out.push_str("why: ");
                out.push_str(why.trim());
                out.push_str("\n\n");
            }
            Row::Veto {
                ts,
                target,
                excerpt,
                why,
                ..
            } => {
                out.push_str(&format!("## veto `{target}`\n{ts}\nwhy: "));
                out.push_str(why.trim());
                out.push('\n');
                if let Some(excerpt) = excerpt {
                    out.push_str("excerpt: ");
                    out.push_str(excerpt.trim());
                    out.push('\n');
                }
                out.push('\n');
            }
            Row::Endorse { ts, target, .. } => {
                out.push_str(&format!("## endorse `{target}`\n{ts}\n\n"));
            }
            Row::Attractor {
                ts, skeleton, note, ..
            } => {
                out.push_str(&format!("## attractor\n{ts}\nskeleton: "));
                out.push_str(skeleton.trim());
                out.push('\n');
                if let Some(note) = note {
                    out.push_str("note: ");
                    out.push_str(note.trim());
                    out.push('\n');
                }
                out.push('\n');
            }
        }
    }
    out
}

fn tokenize(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 1 && !is_stop(w))
        .map(|w| w.to_string())
        .collect()
}

fn overlap_count(row: &HashSet<String>, task: &HashSet<String>) -> u32 {
    if task.is_empty() {
        return 0;
    }
    row.intersection(task).count() as u32
}

fn is_stop(w: &str) -> bool {
    matches!(
        w,
        "the"
            | "and"
            | "for"
            | "that"
            | "this"
            | "with"
            | "from"
            | "are"
            | "was"
            | "were"
            | "have"
            | "has"
            | "had"
            | "not"
            | "but"
            | "you"
            | "your"
            | "into"
            | "out"
            | "about"
            | "than"
            | "then"
            | "them"
            | "they"
            | "its"
            | "it"
            | "of"
            | "to"
            | "in"
            | "on"
            | "at"
            | "as"
            | "or"
            | "an"
            | "be"
            | "by"
            | "is"
    )
}

fn parse_ts(ts: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|dt| dt.timestamp())
        .unwrap_or(0)
}

fn truncate(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Point [`log_path`] at a tempfile for the life of the guard.
#[cfg(test)]
static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub struct TestLogGuard {
    _lock: MutexGuard<'static, ()>,
    _dir: tempfile::TempDir,
}

#[cfg(test)]
impl TestLogGuard {
    pub fn new() -> Self {
        let lock = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("log.jsonl");
        *override_slot().lock().unwrap_or_else(|p| p.into_inner()) = Some(path);
        TestLogGuard {
            _lock: lock,
            _dir: dir,
        }
    }
}

#[cfg(test)]
impl Drop for TestLogGuard {
    fn drop(&mut self) {
        *override_slot().lock().expect("override lock") = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_ts(mut row: Row, ts: &str) -> Row {
        match &mut row {
            Row::Question { ts: slot, .. }
            | Row::Answer { ts: slot, .. }
            | Row::Pair { ts: slot, .. }
            | Row::Veto { ts: slot, .. }
            | Row::Endorse { ts: slot, .. }
            | Row::Attractor { ts: slot, .. } => *slot = ts.to_string(),
        }
        row
    }

    #[test]
    fn schema_roundtrip_matches_cli_shape() {
        let raw = r#"{"v":1,"id":"q1","ts":"2026-01-01T00:00:00Z","kind":"question","text":"hello","status":"open"}"#;
        let row: Row = serde_json::from_str(raw).expect("de");
        match row {
            Row::Question {
                id, text, status, ..
            } => {
                assert_eq!(id, "q1");
                assert_eq!(text, "hello");
                assert_eq!(status, "open");
            }
            other => panic!("wrong kind: {other:?}"),
        }
        let veto = Row::veto("src/auth.rs", "retries hide the error", None).unwrap();
        let json = serde_json::to_string(&veto).unwrap();
        assert!(json.contains("\"kind\":\"veto\""), "{json}");
        assert!(json.contains("\"v\":1"), "{json}");
        assert!(!json.contains("excerpt"), "{json}");
    }

    #[test]
    fn append_then_load_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = Log::open(dir.path().join("log.jsonl")).expect("open");
        let q = Row::question("is the log append-only?");
        let id = q.id().to_string();
        log.append(&q).unwrap();
        log.append(&Row::endorse("SCHEMA.md")).unwrap();
        log.append(&Row::answer(&id, "yes")).unwrap();
        let rows = log.load().unwrap();
        assert_eq!(rows.len(), 3);
        assert!(open_questions(&rows).is_empty());
    }

    #[test]
    fn answer_does_not_rewrite_history() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("log.jsonl");
        let log = Log::open(&path).unwrap();
        let q = Row::question("rewrite?");
        let id = q.id().to_string();
        log.append(&q).unwrap();
        log.append(&Row::answer(&id, "no")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains("\"kind\":\"question\""));
        assert!(text.contains("\"kind\":\"answer\""));
        assert!(text.contains("\"qid\""));
    }

    #[test]
    fn compile_ranking_prefers_overlap_then_recency() {
        let veto = with_ts(
            Row::veto("src/auth.rs", "oauth retries hide the real error", None).unwrap(),
            "2020-01-01T00:00:00Z",
        );
        let attractor = with_ts(
            Row::attractor(
                "name the conflict, then the rule",
                Some("oauth bind".into()),
            )
            .unwrap(),
            "2021-01-01T00:00:00Z",
        );
        let unrelated_new = with_ts(Row::endorse("docs/gardening.md"), "2026-01-01T00:00:00Z");
        let rows = vec![veto, unrelated_new, attractor];
        let ranked = rank(&rows, "oauth auth bind");
        assert_eq!(ranked[0].kind_name(), "attractor");
        assert_eq!(ranked[1].kind_name(), "veto");
    }

    #[test]
    fn compile_drops_answered_questions() {
        let q = with_ts(Row::question("rewrite history?"), "2026-01-01T00:00:00Z");
        let id = q.id().to_string();
        let a = with_ts(
            Row::answer(&id, "no, stay append-only"),
            "2026-01-02T00:00:00Z",
        );
        let md = compile_slice(&[q, a], "append-only history");
        assert!(!md.contains("## question"), "{md}");
        assert!(md.contains("## answer"), "{md}");
    }

    #[test]
    fn veto_without_why_fails() {
        assert!(Row::veto("src/foo.rs", "  ", None).is_err());
    }

    #[test]
    fn tests_do_not_touch_the_real_state_dir() {
        let _guard = TestLogGuard::new();
        let path = log_path();
        assert!(path.ends_with("log.jsonl"));
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        assert!(!path.starts_with(home.join(".local/state/turing")));
    }

    #[test]
    fn report_compile_and_status() {
        let _guard = TestLogGuard::new();
        let log = Log::default_log().unwrap();
        log.append(&Row::endorse("src/ok.rs")).unwrap();
        let status = report(&TuringAction::Status);
        assert!(status.contains("endorse: 1"), "{status}");
        let compiled = report(&TuringAction::Compile("ok.rs".into()));
        assert!(compiled.contains("endorse"), "{compiled}");
        assert!(!compiled.to_lowercase().contains("write like"));
    }
}
