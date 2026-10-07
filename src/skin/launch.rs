//! Start a full-UI skin. Those looks are separate binaries installed next to
//! `wizard`. This process is replaced, so a switch does not keep the in-memory
//! transcript; the session store is the same one `wizard acp` already uses.

use std::path::{Path, PathBuf};
use std::process::Command;

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

/// Replace this process with the binary for `skin`.
///
/// `Skin::Wizard` re-execs this binary, which is how a full UI gets back to
/// the house look. A full-UI skin execs `wizard-ui-*`, looked up next to this
/// executable and then on `PATH`.
pub fn exec_skin(skin: Skin) -> Result<()> {
    let wizard = std::env::current_exe().context("finding this wizard binary")?;
    let (program, label) = match skin.companion_bin() {
        Some(name) => (find_companion(&wizard, name)?, name.to_string()),
        None => (wizard.clone(), "wizard".to_string()),
    };
    let mut cmd = Command::new(&program);
    for var in WIZARD_BIN_ENVS {
        cmd.env(var, &wizard);
    }
    // A full UI must not see WIZARD_SKIN and also must not be the one that
    // reads it. The house binary reads [ui] skin on the next start.
    cmd.env_remove(super::ENV_SKIN);
    replace(cmd).with_context(|| format!("starting {label} ({})", program.display()))
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
    if let Some(dir) = wizard.parent() {
        let beside = dir.join(name);
        if beside.is_file() {
            return Ok(beside);
        }
    }
    if let Some(found) = which(name) {
        return Ok(found);
    }
    bail!(
        "the {name} look is not installed. It should sit next to wizard \
         ({}). Reinstall the release, or build the frontends and copy the \
         binary there.",
        wizard
            .parent()
            .map(|dir| dir.join(name))
            .unwrap_or_else(|| PathBuf::from(name))
            .display()
    )
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn replace(mut cmd: Command) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        bail!("{err}");
    }
    #[cfg(not(unix))]
    {
        let status = cmd.status()?;
        std::process::exit(status.code().unwrap_or(1));
    }
}
