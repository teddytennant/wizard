//! The small Claude Code subcommands: `setup-token`, `install`, `import`,
//! `purge`, `stop`/`kill`, `rm`, and the names that only explain themselves.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

/// `wizard setup-token`: Wizard has no long-lived token of its own. The
/// `claude` provider runs the real Claude Code, so when it is installed this
/// hands over to its `setup-token`.
pub fn run_setup_token() -> Result<i32> {
    match which_claude() {
        Some(claude) => {
            let status = std::process::Command::new(claude)
                .arg("setup-token")
                .status()?;
            Ok(status.code().unwrap_or(1))
        }
        None => {
            eprintln!(
                "Wizard has no long-lived token to set up. Sign in with `wizard auth login`, or \
                 export the provider's API key. For a Claude subscription token, install Claude \
                 Code and run `claude setup-token`."
            );
            Ok(1)
        }
    }
}

fn which_claude() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("claude"))
        .find(|candidate| candidate.is_file())
}

/// `wizard install [target]` is `wizard update`: `stable` and `latest` mean
/// the newest release, a bare version means that tag.
pub fn install_target(target: Option<&str>) -> Option<String> {
    match target {
        None | Some("" | "stable" | "latest") => None,
        Some(tag) if tag.starts_with('v') => Some(tag.to_string()),
        Some(version) => Some(format!("v{version}")),
    }
}

/// `wizard import [source]`.
pub fn run_import(source: Option<&str>, dry_run: bool, yes: bool) -> Result<i32> {
    match source.unwrap_or("claude") {
        "claude" | "claude-code" => {}
        other @ ("codex" | "gemini" | "cursor") => {
            bail!("Wizard imports from Claude Code only; there is no {other} importer")
        }
        other => bail!("unknown source '{other}' (Wizard imports from: claude)"),
    }
    if crate::import_claude::claude_home().is_none()
        && crate::import_claude::claude_json_path().is_none()
    {
        println!("No Claude Code configuration found; nothing to import.");
        return Ok(0);
    }
    let (mcp, commands, _) = crate::import_claude::counts();
    println!("Claude Code: {mcp} MCP servers, {commands} custom commands");
    if dry_run {
        println!("Dry run: nothing written.");
        return Ok(0);
    }
    if mcp + commands == 0 {
        return Ok(0);
    }
    if !yes {
        if !std::io::stdin().is_terminal() {
            bail!("not a terminal: pass --yes to import without asking");
        }
        print!("Import them into ~/.wizard? [y/N] ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("Cancelled.");
            return Ok(1);
        }
    }
    let outcome = crate::import_claude::run_import(&crate::import_claude::ImportSelection {
        mcp: true,
        commands: true,
        verbs: false,
    })?;
    let summary = outcome.summary();
    println!(
        "{}",
        if summary.is_empty() {
            "Nothing new to import."
        } else {
            &summary
        }
    );
    Ok(0)
}

/// Session files and sidecars that belong to one conversation.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Doomed {
    pub id: String,
    pub cwd: String,
    pub files: Vec<PathBuf>,
}

fn header_cwd(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut first = String::new();
    std::io::BufReader::new(file).read_line(&mut first).ok()?;
    serde_json::from_str::<serde_json::Value>(&first)
        .ok()?
        .get("cwd")?
        .as_str()
        .map(str::to_string)
}

/// The sessions in `dir` that were started in `project` (or every one, with
/// `None`), each with its sidecar files (`<id>.name` and the like).
pub(crate) fn sessions_of(dir: &Path, project: Option<&Path>) -> Vec<Doomed> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let want = project.map(|p| p.canonicalize().unwrap_or_else(|_| p.to_path_buf()));
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "jsonl") {
            continue;
        }
        let Some(id) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
            continue;
        };
        let cwd = header_cwd(&path).unwrap_or_default();
        if let Some(want) = &want {
            let here = Path::new(&cwd);
            let here = here.canonicalize().unwrap_or_else(|_| here.to_path_buf());
            if &here != want {
                continue;
            }
        }
        let prefix = format!("{id}.");
        let files = std::fs::read_dir(dir)
            .map(|siblings| {
                siblings
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.file_name()
                            .is_some_and(|n| n.to_string_lossy().starts_with(&prefix))
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.push(Doomed { id, cwd, files });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

fn delete(doomed: &[Doomed], images: Option<&Path>) -> usize {
    let mut removed = 0;
    for session in doomed {
        for file in &session.files {
            if std::fs::remove_file(file).is_ok() {
                removed += 1;
            }
        }
        if let Some(images) = images {
            let _ = std::fs::remove_dir_all(images.join(&session.id));
        }
    }
    removed
}

/// `wizard purge [path] [--all] [--dry-run] [-y] [-i]`: delete a project's
/// conversations from this machine. Claude Code's purge also clears tasks and
/// file history; Wizard's conversations are the only per-project data it keeps
/// outside the project itself.
pub fn purge(
    path: Option<&Path>,
    all: bool,
    dry_run: bool,
    yes: bool,
    interactive: bool,
) -> Result<i32> {
    if all && path.is_some() {
        bail!("--all and a path are mutually exclusive");
    }
    let dir = crate::config::Config::sessions_dir()?;
    let images = crate::config::Config::images_dir().ok();
    let project = if all {
        None
    } else {
        Some(match path {
            Some(path) => path.to_path_buf(),
            None => std::env::current_dir()?,
        })
    };
    let mut doomed = sessions_of(&dir, project.as_deref());
    if doomed.is_empty() {
        println!("No Wizard conversations to purge.");
        return Ok(0);
    }
    let tty = std::io::stdin().is_terminal();
    for session in &doomed {
        println!("{}  {}", session.id, session.cwd);
    }
    println!("{} conversation(s)", doomed.len());
    if dry_run {
        println!("Dry run: nothing deleted.");
        return Ok(0);
    }
    if interactive {
        let mut keep = Vec::new();
        for session in doomed {
            print!("Delete {}? [y/N] ", session.id);
            std::io::stdout().flush()?;
            let mut answer = String::new();
            std::io::stdin().lock().read_line(&mut answer)?;
            if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                keep.push(session);
            }
        }
        doomed = keep;
    } else if !yes {
        if !tty {
            bail!("not a terminal: pass --yes to delete without asking");
        }
        print!("Delete them? [y/N] ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("Cancelled.");
            return Ok(1);
        }
    }
    let removed = delete(&doomed, images.as_deref());
    println!("Deleted {removed} file(s).");
    Ok(0)
}

/// `wizard stop <id>` / `wizard kill <id>`: the `/stop` command.
pub fn stop_session(id: &str) -> Result<i32> {
    match crate::commands::stop_background_session(id) {
        Ok(message) => {
            println!("{message}");
            Ok(0)
        }
        Err(err) => {
            eprintln!("{err}");
            Ok(1)
        }
    }
}

/// `wizard rm <id>`: forget a finished background session's record.
pub fn remove_session(id: &str) -> Result<i32> {
    let records = crate::session_registry::list();
    let matches: Vec<_> = records.iter().filter(|r| r.id.starts_with(id)).collect();
    match matches.as_slice() {
        [] => {
            eprintln!("no background session matches '{id}'");
            Ok(1)
        }
        [record] if !record.state.is_terminal() => {
            eprintln!(
                "{} is still running; `wizard stop {}` first",
                record.id, record.id
            );
            Ok(1)
        }
        [record] => {
            crate::session_registry::remove(&record.id);
            println!("removed {}", record.id);
            Ok(0)
        }
        many => {
            eprintln!(
                "'{id}' matches {} sessions; give more of the id",
                many.len()
            );
            Ok(1)
        }
    }
}

/// Subcommands that parse and explain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stub {
    Attach,
    Logs,
    Respawn,
    AutoMode,
    Ultrareview,
}

/// What a stubbed subcommand says on stderr.
pub fn stub_message(stub: Stub) -> &'static str {
    match stub {
        Stub::Attach => {
            "Wizard cannot attach a terminal to a detached session. `wizard agents` lists them and \
             shows what each is doing; `wizard --resume <id>` reopens the conversation."
        }
        Stub::Logs => {
            "Wizard keeps no terminal log for background sessions. `wizard agents` shows their \
             activity, and the transcript is in ~/.wizard/sessions."
        }
        Stub::Respawn => {
            "Wizard background sessions are plain processes and pick up a new binary when started \
             again; `wizard stop <id>` and `/background` restart one."
        }
        Stub::AutoMode => {
            "Auto mode is Anthropic's permission classifier; Wizard has no such mode. See \
             `wizard --help` for --permission-mode."
        }
        Stub::Ultrareview => {
            "ultrareview is a cloud service. `/review` runs a local multi-pass review of the \
             current diff."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_targets_map_onto_release_tags() {
        assert_eq!(install_target(None), None);
        assert_eq!(install_target(Some("stable")), None);
        assert_eq!(install_target(Some("latest")), None);
        assert_eq!(install_target(Some("1.2.3")).as_deref(), Some("v1.2.3"));
        assert_eq!(install_target(Some("v1.2.3")).as_deref(), Some("v1.2.3"));
    }

    fn session(dir: &Path, id: &str, cwd: &str) {
        std::fs::write(
            dir.join(format!("{id}.jsonl")),
            format!("{{\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"{cwd}\",\"version\":1}}\n"),
        )
        .unwrap();
    }

    #[test]
    fn purge_picks_one_projects_sessions_and_their_sidecars() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("p");
        std::fs::create_dir(&project).unwrap();
        let dir = tmp.path().join("sessions");
        std::fs::create_dir(&dir).unwrap();
        session(&dir, "a", project.to_str().unwrap());
        session(&dir, "b", "/somewhere/else");
        std::fs::write(dir.join("a.name"), "named").unwrap();

        let mine = sessions_of(&dir, Some(&project));
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].id, "a");
        assert_eq!(mine[0].files.len(), 2);
        assert_eq!(sessions_of(&dir, None).len(), 2);

        assert_eq!(delete(&mine, None), 2);
        assert!(!dir.join("a.jsonl").exists() && !dir.join("a.name").exists());
        assert!(dir.join("b.jsonl").exists());
    }

    #[test]
    fn the_other_importers_are_refused_by_name() {
        let err = run_import(Some("codex"), true, true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("codex"), "{err}");
        assert!(run_import(Some("nope"), true, true).is_err());
    }

    #[test]
    fn every_stub_has_a_sentence() {
        for stub in [
            Stub::Attach,
            Stub::Logs,
            Stub::Respawn,
            Stub::AutoMode,
            Stub::Ultrareview,
        ] {
            assert!(stub_message(stub).len() > 20);
        }
    }
}
