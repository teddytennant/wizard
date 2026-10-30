//! Claude Code's session flags: `--resume <id or name>`, `--session-id`,
//! `--name`, `--fork-session` and `--no-session-persistence`.
//!
//! They all come down to one question asked before anything starts: which
//! session file does this run open? [`select`] answers it, creating or copying
//! a file when the flags ask for a new one, and the caller hands the id to the
//! same `--resume <id>` path the terminal already has. A session's display
//! name lives next to its file as `<id>.name`, so no session format changes.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::agent::session::Session;

/// What the flags ask for.
#[derive(Debug, Clone, Copy, Default)]
pub struct Request<'a> {
    /// `--resume <token>`: an id, a unique id prefix, or a name.
    pub resume: Option<&'a str>,
    /// `--continue` (or a bare `--resume`): the latest session here.
    pub latest: bool,
    pub session_id: Option<&'a str>,
    pub name: Option<&'a str>,
    pub fork: bool,
}

impl Request<'_> {
    /// Whether any flag needs a decision. When none does, the default path
    /// (a fresh session, or the latest one for `--resume`) is left alone.
    pub fn is_empty(&self) -> bool {
        self.resume.is_none() && self.session_id.is_none() && self.name.is_none() && !self.fork
    }
}

/// The file holding a session's display name.
pub fn name_path(session_file: &Path) -> PathBuf {
    session_file.with_extension("name")
}

/// The display name given with `--name` or `/rename`, if any.
pub fn name_for(session_file: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(name_path(session_file)).ok()?;
    let name = raw.trim();
    (!name.is_empty()).then(|| name.to_string())
}

pub fn set_name(session_file: &Path, name: &str) -> Result<()> {
    let name = name.trim();
    anyhow::ensure!(!name.is_empty(), "a session name cannot be empty");
    std::fs::write(name_path(session_file), format!("{name}\n"))
        .with_context(|| format!("writing {}", name_path(session_file).display()))
}

/// Delete the sessions in `dir` that nothing has written to for more than
/// `days` days (`cleanupPeriodDays`), with their name files. Returns how many
/// went. `keep` is a session about to be resumed. A session that cannot be
/// removed is left alone, and 0 days means no cleanup.
pub fn prune(dir: &Path, days: u32, keep: Option<&str>) -> usize {
    if days == 0 {
        return 0;
    }
    let Some(cutoff) = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(u64::from(days) * 86_400))
    else {
        return 0;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for path in entries.flatten().map(|entry| entry.path()) {
        if path.extension().is_none_or(|ext| ext != "jsonl")
            || path
                .file_stem()
                .is_some_and(|stem| Some(stem.to_string_lossy().as_ref()) == keep)
        {
            continue;
        }
        let old = std::fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .is_ok_and(|modified| modified < cutoff);
        if old && std::fs::remove_file(&path).is_ok() {
            let _ = std::fs::remove_file(name_path(&path));
            removed += 1;
        }
    }
    removed
}

/// The session `token` names: an exact id, then an exact name (newest first),
/// then an id prefix that only one session has.
pub fn find(dir: &Path, token: &str) -> Result<Session> {
    if let Some(session) = Session::open_by_id(dir, token)? {
        return Ok(session);
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files.reverse();
    let stem = |path: &Path| {
        path.file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    if let Some(path) = files
        .iter()
        .find(|path| name_for(path).is_some_and(|name| name.eq_ignore_ascii_case(token)))
        && let Some(session) = Session::open_by_id(dir, &stem(path))?
    {
        return Ok(session);
    }
    let matches: Vec<String> = files
        .iter()
        .map(|path| stem(path))
        .filter(|id| id.starts_with(token))
        .collect();
    match matches.as_slice() {
        [one] => Session::open_by_id(dir, one)?
            .with_context(|| format!("session '{one}' disappeared while opening it")),
        [] => anyhow::bail!("no session with the id or name '{token}'"),
        many => anyhow::bail!(
            "'{token}' matches {} sessions ({}); give more of the id",
            many.len(),
            many.iter().take(4).cloned().collect::<Vec<_>>().join(", ")
        ),
    }
}

/// The session this run opens, or `None` to leave the default in place.
pub fn select(dir: &Path, request: Request<'_>) -> Result<Option<Session>> {
    if request.is_empty() {
        return Ok(None);
    }
    let base = match (request.resume, request.latest) {
        (Some(token), _) => Some(find(dir, token)?),
        (None, true) => Session::open_latest(dir)?,
        (None, false) => None,
    };
    let session = match (base, request.fork) {
        (Some(base), true) => Some(base.fork(dir, request.session_id)?),
        (Some(_), false) if request.session_id.is_some() => anyhow::bail!(
            "--session-id with --resume or --continue needs --fork-session, \
             or the new session would share the old one's id"
        ),
        (Some(base), false) => Some(base),
        (None, true) if request.resume.is_none() && request.latest => {
            // `--continue` with nothing to continue: start the named one.
            new_session(dir, request)?
        }
        (None, _) => new_session(dir, request)?,
    };
    if let (Some(session), Some(name)) = (&session, request.name) {
        set_name(session.path(), name)?;
    }
    Ok(session)
}

fn new_session(dir: &Path, request: Request<'_>) -> Result<Option<Session>> {
    match (request.session_id, request.name) {
        (Some(id), _) => Session::create_with_id(dir, id).map(Some),
        (None, Some(_)) => Session::create(dir).map(Some),
        (None, None) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ChatMessage;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn with_message(dir: &Path, id: &str) -> Session {
        let session = Session::create_with_id(dir, id).unwrap();
        session.append(&ChatMessage::user("hello")).unwrap();
        session
    }

    #[test]
    fn no_flags_leave_the_default_alone() {
        let tmp = dir();
        assert!(select(tmp.path(), Request::default()).unwrap().is_none());
    }

    #[test]
    fn session_id_creates_that_session_and_refuses_a_second_use() {
        let tmp = dir();
        let request = Request {
            session_id: Some("abc-123"),
            ..Request::default()
        };
        let session = select(tmp.path(), request).unwrap().unwrap();
        assert_eq!(session.id, "abc-123");
        assert!(tmp.path().join("abc-123.jsonl").is_file());
        let again = select(tmp.path(), request).unwrap_err().to_string();
        assert!(again.contains("already in use"), "{again}");
    }

    #[test]
    fn a_bad_session_id_is_refused() {
        let tmp = dir();
        let request = Request {
            session_id: Some("../escape"),
            ..Request::default()
        };
        assert!(select(tmp.path(), request).is_err());
    }

    #[test]
    fn resume_finds_an_id_a_name_and_a_unique_prefix() {
        let tmp = dir();
        let first = with_message(tmp.path(), "2026-01-01T00-00-00");
        with_message(tmp.path(), "2026-02-02T00-00-00");
        set_name(first.path(), "Refactor").unwrap();
        assert_eq!(
            find(tmp.path(), "2026-01-01T00-00-00").unwrap().id,
            first.id
        );
        assert_eq!(find(tmp.path(), "refactor").unwrap().id, first.id);
        assert_eq!(
            find(tmp.path(), "2026-02").unwrap().id,
            "2026-02-02T00-00-00"
        );
        let ambiguous = find(tmp.path(), "2026-0").unwrap_err().to_string();
        assert!(ambiguous.contains("matches 2 sessions"), "{ambiguous}");
        assert!(find(tmp.path(), "nope").is_err());
    }

    #[test]
    fn fork_copies_the_conversation_under_a_new_id() {
        let tmp = dir();
        let base = with_message(tmp.path(), "base");
        let forked = select(
            tmp.path(),
            Request {
                resume: Some("base"),
                fork: true,
                session_id: Some("copy"),
                ..Request::default()
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(forked.id, "copy");
        assert_eq!(
            forked.load_messages().unwrap().len(),
            base.load_messages().unwrap().len()
        );
        forked.append(&ChatMessage::user("more")).unwrap();
        assert_eq!(
            base.load_messages().unwrap().len(),
            1,
            "the original is untouched"
        );
    }

    #[test]
    fn resuming_under_a_new_id_needs_fork_session() {
        let tmp = dir();
        with_message(tmp.path(), "base");
        let err = select(
            tmp.path(),
            Request {
                resume: Some("base"),
                session_id: Some("copy"),
                ..Request::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("--fork-session"), "{err}");
    }

    #[test]
    fn name_alone_starts_a_named_fresh_session() {
        let tmp = dir();
        let session = select(
            tmp.path(),
            Request {
                name: Some("scratch"),
                ..Request::default()
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(name_for(session.path()).as_deref(), Some("scratch"));
    }

    #[test]
    fn prune_removes_only_sessions_older_than_the_period() {
        let tmp = dir();
        let old = with_message(tmp.path(), "old");
        set_name(old.path(), "stale").unwrap();
        with_message(tmp.path(), "fresh");
        let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(40 * 86_400);
        std::fs::File::options()
            .write(true)
            .open(old.path())
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
        assert_eq!(prune(tmp.path(), 30, Some("fresh")), 1);
        assert!(!old.path().exists());
        assert!(!name_path(old.path()).exists());
        assert!(tmp.path().join("fresh.jsonl").exists());
        assert_eq!(prune(tmp.path(), 30, None), 0);
        assert_eq!(prune(tmp.path(), 0, None), 0, "0 days prunes nothing");
    }

    #[test]
    fn a_name_shows_in_the_session_list() {
        let tmp = dir();
        let session = with_message(tmp.path(), "listed");
        set_name(session.path(), "my name").unwrap();
        let rows = crate::agent::session::summaries(tmp.path());
        assert_eq!(rows[0].summary, "my name");
    }
}
