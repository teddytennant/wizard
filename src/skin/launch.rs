//! Start a full-UI skin. Those looks are separate binaries installed next to
//! `wizard`. The look runs as a child and this process waits for it, so the
//! update check it already started can finish and `/ui wizard` inside the look
//! can bring the house UI back when it quits. The in-memory transcript does not
//! carry over; the session store is the same one `wizard acp` already uses.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::Skin;

/// Env vars the full UIs read to find the wizard binary they should spawn as
/// `wizard acp`. Set all of them so a renamed binary still finds this one.
const WIZARD_BIN_ENVS: [&str; 4] = [
    "OPENW_WIZARD_BIN",
    "PIW_WIZARD_BIN",
    "GROKW_WIZARD_BIN",
    "CODEXW_WIZARD_BIN",
];

/// Run the `wizard-ui-*` binary for `skin` and wait for it. Returns its exit
/// code. `skin` must have a companion; [`Skin::Wizard`] is an error.
///
/// The terminal must already be restored: the look sets it up for itself.
pub async fn run(skin: Skin) -> Result<i32> {
    let Some(name) = skin.companion_bin() else {
        bail!("{} has no separate UI to start", skin.label());
    };
    let wizard = std::env::current_exe().context("finding this wizard binary")?;
    let program = find_companion(&wizard, name)?;
    let mut cmd = tokio::process::Command::new(&program);
    for var in WIZARD_BIN_ENVS {
        cmd.env(var, &wizard);
    }
    // The look must not see WIZARD_SKIN: its own `wizard acp` children would
    // inherit it, and the choice it already made is the one being run.
    cmd.env_remove(super::ENV_SKIN);
    #[cfg(unix)]
    let _interrupts = IgnoreInterrupts::during(&mut cmd);
    let status = cmd
        .status()
        .await
        .with_context(|| format!("starting {name} ({})", program.display()))?;
    Ok(exit_code(status))
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    status.code().unwrap_or(1)
}

/// What `system(3)` does around its child: a Ctrl+C typed while the look is
/// in cooked mode reaches the whole foreground group, and the look is the one
/// that should answer it. Without this the parent dies and the look is left
/// reading a terminal the shell has taken back.
///
/// `SIG_IGN` survives `exec`, so the child puts the defaults back before it
/// starts. Dropping the guard restores whatever this process had before.
#[cfg(unix)]
struct IgnoreInterrupts {
    int: libc::sighandler_t,
    quit: libc::sighandler_t,
}

#[cfg(unix)]
impl IgnoreInterrupts {
    fn during(cmd: &mut tokio::process::Command) -> Self {
        // SAFETY: `signal` is async-signal-safe, and the closure touches
        // nothing else between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                libc::signal(libc::SIGQUIT, libc::SIG_DFL);
                Ok(())
            });
        }
        // SAFETY: swapping a disposition and keeping the old one to restore.
        unsafe {
            Self {
                int: libc::signal(libc::SIGINT, libc::SIG_IGN),
                quit: libc::signal(libc::SIGQUIT, libc::SIG_IGN),
            }
        }
    }
}

#[cfg(unix)]
impl Drop for IgnoreInterrupts {
    fn drop(&mut self) {
        // SAFETY: restoring the dispositions `during` replaced.
        unsafe {
            libc::signal(libc::SIGINT, self.int);
            libc::signal(libc::SIGQUIT, self.quit);
        }
    }
}

/// True when `wizard-ui-*` is beside this binary or on `PATH`.
pub fn companion_installed(name: &str) -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|wizard| find_companion(&wizard, name).ok())
        .is_some()
}

/// `wizard-ui-*` beside `wizard`, then on `PATH`.
pub fn find_companion(wizard: &Path, name: &str) -> Result<PathBuf> {
    if let Some(found) = search_dirs(wizard)
        .into_iter()
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
    {
        return Ok(found);
    }
    bail!(
        "the {name} look is not installed. Re-run the installer, or put wizard-ui-{name} next to wizard ({}) or on PATH.",
        wizard
            .parent()
            .map(|dir| dir.join(name))
            .unwrap_or_else(|| PathBuf::from(name))
            .display()
    )
}

#[cfg(not(test))]
fn search_dirs(wizard: &Path) -> Vec<PathBuf> {
    let beside = wizard.parent().map(Path::to_path_buf);
    let path = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();
    beside.into_iter().chain(path).collect()
}

/// Tests never look beside the test binary or on `PATH`: a machine with the
/// looks installed would otherwise start one in the middle of the suite.
#[cfg(test)]
fn search_dirs(_wizard: &Path) -> Vec<PathBuf> {
    SEARCH.with(|dirs| dirs.borrow().clone())
}

#[cfg(test)]
thread_local! {
    static SEARCH: std::cell::RefCell<Vec<PathBuf>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Make `dir` the only place this thread's lookups search, until the guard
/// drops.
#[cfg(test)]
pub fn pin_search(dir: &Path) -> SearchPinned {
    let previous = SEARCH.with(|dirs| dirs.replace(vec![dir.to_path_buf()]));
    SearchPinned { previous }
}

#[cfg(test)]
pub struct SearchPinned {
    previous: Vec<PathBuf>,
}

#[cfg(test)]
impl Drop for SearchPinned {
    fn drop(&mut self) {
        let previous = std::mem::take(&mut self.previous);
        SEARCH.with(|dirs| *dirs.borrow_mut() = previous);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_look_runs_as_a_child_and_its_exit_code_comes_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let look = dir.path().join("wizard-ui-pi");
        std::fs::write(&look, "#!/bin/sh\nexit 7\n").expect("write the look");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&look, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let _search = pin_search(dir.path());
        assert_eq!(run(Skin::Pi).await.expect("the look runs"), 7);
    }

    #[tokio::test]
    async fn a_missing_look_names_where_it_should_be() {
        let dir = tempfile::tempdir().expect("tempdir");
        let _search = pin_search(dir.path());
        let err = run(Skin::Opencode).await.expect_err("nothing is installed");
        assert!(format!("{err:#}").contains("wizard-ui-opencode"), "{err:#}");
        assert!(run(Skin::Wizard).await.is_err());
    }
}
