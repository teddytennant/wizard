//! `/branch`, `/rename`, `/cd`, `/background` and `/stop`: commands about the
//! session itself rather than what is in it.

use std::path::{Path, PathBuf};

use crate::agent::session::Session;
use crate::config::Config;

fn sessions_dir() -> Result<PathBuf, String> {
    Config::sessions_dir().map_err(|err| format!("{err:#}"))
}

fn current(session: Option<&str>) -> Result<Session, String> {
    let id = session.ok_or("the session is busy with a turn; try again when it ends")?;
    Session::open_by_id(&sessions_dir()?, id)
        .map_err(|err| format!("{err:#}"))?
        .ok_or_else(|| format!("session {id} has no file yet (nothing has been said)"))
}

/// `/rename [name]`: name this session, or say what it is called.
pub fn rename(session: Option<&str>, name: &str) -> Result<String, String> {
    let current = current(session)?;
    let name = name.trim();
    if name.is_empty() {
        return Ok(match crate::session_flags::name_for(current.path()) {
            Some(name) => format!("this session is called '{name}'"),
            None => format!(
                "this session has no name ({}); /rename <name> gives it one",
                current.id
            ),
        });
    }
    crate::session_flags::set_name(current.path(), name).map_err(|err| format!("{err:#}"))?;
    Ok(format!("session {} is now called '{name}'", current.id))
}

/// `/branch [name]`: copy the conversation to a new session. Returns the new
/// id, for the surface to switch to, and what to tell the user.
pub fn branch(session: Option<&str>, name: &str) -> Result<(String, String), String> {
    let source = current(session)?;
    let copy = source
        .fork(&sessions_dir()?, None)
        .map_err(|err| format!("{err:#}"))?;
    let name = name.trim();
    if !name.is_empty() {
        crate::session_flags::set_name(copy.path(), name).map_err(|err| format!("{err:#}"))?;
    }
    let message = format!(
        "branched {} into {}; the original stays as it was, /resume {} comes back to it",
        source.id, copy.id, source.id
    );
    Ok((copy.id, message))
}

/// `/cd <dir>`: move the process into another directory. Tools that resolve
/// relative paths follow; the instruction files, hooks and settings were read
/// for the project the session started in and stay that project's.
pub fn change_dir(dir: &str, root: &Path) -> Result<String, String> {
    let dir = dir.trim();
    if dir.is_empty() {
        return Ok(format!(
            "in {}",
            std::env::current_dir().unwrap_or_default().display()
        ));
    }
    let expanded = shellexpand::tilde(dir).into_owned();
    let target = {
        let path = PathBuf::from(&expanded);
        if path.is_absolute() {
            path
        } else {
            root.join(path)
        }
    };
    let target = target
        .canonicalize()
        .map_err(|err| format!("{}: {err}", target.display()))?;
    if !target.is_dir() {
        return Err(format!("{} is not a directory", target.display()));
    }
    std::env::set_current_dir(&target).map_err(|err| format!("{}: {err}", target.display()))?;
    Ok(format!(
        "now in {}\ninstructions, hooks and settings still come from {}; start wizard there to switch them",
        target.display(),
        root.display()
    ))
}

/// `/background [task]`: continue this conversation in a detached headless
/// session, on a copy so the two never write to one file.
pub fn background(session: Option<&str>, task: &str, root: &Path) -> Result<String, String> {
    use std::os::unix::process::CommandExt;
    let source = current(session)?;
    let copy = source
        .fork(&sessions_dir()?, None)
        .map_err(|err| format!("{err:#}"))?;
    let task = match task.trim() {
        "" => "Continue the work in this conversation until it is done, then summarize it.",
        task => task,
    };
    let exe = std::env::current_exe().map_err(|err| format!("the wizard binary: {err}"))?;
    std::process::Command::new(exe)
        .arg("--bg")
        .arg("--mode")
        .arg("sovereign")
        .arg("--resume")
        .arg(&copy.id)
        .arg("-p")
        .arg(task)
        .arg("--cwd")
        .arg(root)
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|err| format!("could not start the background session: {err}"))?;
    Ok(format!(
        "background session {} started from a copy of this conversation; /stop lists and stops them, /dashboard shows them",
        copy.id
    ))
}

/// `/stop [id]`: list the background sessions, or stop one.
pub fn stop(which: &str) -> Result<String, String> {
    let me = std::process::id();
    let live: Vec<_> = crate::session_registry::list()
        .into_iter()
        .filter(|row| !row.state.is_terminal() && row.pid != me)
        .collect();
    let which = which.trim();
    if which.is_empty() {
        if live.is_empty() {
            return Ok("no background sessions are running".to_string());
        }
        let mut out = String::from("background sessions (/stop <id> stops one)\n");
        for row in &live {
            out.push_str(&format!(
                "  {:<22} {}  {}\n",
                row.id, row.activity, row.name
            ));
        }
        return Ok(out);
    }
    let matches: Vec<_> = live
        .iter()
        .filter(|row| row.id.starts_with(which))
        .collect();
    match matches.as_slice() {
        [] => Err(format!("no running background session matches '{which}'")),
        [row] => {
            let pid = row.pid as i32;
            // The session leads its own process group, so its tools go too.
            unsafe {
                if libc::kill(-pid, libc::SIGTERM) != 0 {
                    libc::kill(pid, libc::SIGTERM);
                }
            }
            crate::session_registry::remove(&row.id);
            Ok(format!("stopped {} ({})", row.id, row.name))
        }
        many => Err(format!(
            "'{which}' matches {} sessions; give more of the id",
            many.len()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cd_moves_into_a_directory_and_refuses_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(tmp.path().join("f"), "x").unwrap();
        assert!(
            change_dir("f", tmp.path())
                .unwrap_err()
                .contains("not a directory")
        );
        assert!(change_dir("missing", tmp.path()).is_err());
        // Not asserted by moving: the test process shares its cwd, so only the
        // refusals and the bare form are exercised here.
        assert!(change_dir("", tmp.path()).unwrap().starts_with("in "));
    }

    #[test]
    fn a_busy_session_cannot_be_renamed_or_branched() {
        assert!(rename(None, "x").unwrap_err().contains("busy"));
        assert!(branch(None, "").unwrap_err().contains("busy"));
        assert!(
            background(None, "", Path::new("."))
                .unwrap_err()
                .contains("busy")
        );
    }

    #[test]
    fn stop_without_sessions_says_so() {
        // Whatever the shared test registry holds, a name nothing has is an error.
        assert!(stop("no-such-session-zzz").is_err());
    }
}
