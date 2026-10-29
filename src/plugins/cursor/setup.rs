//! What picking "Cursor (subscription)" in `/provider` does: find
//! `cursor-agent` and get it signed in. It runs on the plain terminal with the
//! TUI suspended, because `cursor-agent login` needs the terminal to itself.

use anyhow::{Context, Result, bail};

use super::{Cli, INSTALL_COMMAND, LOGIN_COMMAND};
use crate::config::Config;

/// Make sure `cursor-agent` is installed and signed in on this process's
/// terminal. `Ok(false)` when it is not installed, after printing the command
/// that installs it: Cursor's installer is a script piped to a shell, which is
/// not something to run on a yes typed into a prompt.
pub fn ensure_ready() -> Result<bool> {
    let Some(cli) = Cli::find() else {
        println!("Cursor is not installed. Install it with:\n\n    {INSTALL_COMMAND}");
        return Ok(false);
    };
    if !cli.signed_in()? {
        println!("Cursor is not signed in. Starting `{LOGIN_COMMAND}`.");
        cli.login()?;
        if !cli.signed_in()? {
            bail!("Cursor is still not signed in. Run: {LOGIN_COMMAND}");
        }
    }
    Ok(true)
}

/// Run the setup on this process's terminal, then store the provider and make
/// it the active one. `Ok(None)` when Cursor is not installed.
pub fn run_blocking() -> Result<Option<Config>> {
    if !ensure_ready()? {
        return Ok(None);
    }
    let mut config = Config::load().context("loading config for Cursor setup")?;
    let provider = super::provider_config();
    config.providers.retain(|other| other.name != provider.name);
    config.active_provider = Some(provider.name.clone());
    config.providers.push(provider);
    config.save().context("saving config from Cursor setup")?;
    Ok(Some(config))
}
