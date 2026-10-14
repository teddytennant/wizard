//! What picking "Claude (subscription)" in `/provider` does: find Claude
//! Code, offer to install it, and get it signed in. It runs on the plain
//! terminal with the TUI suspended, because both the installer and
//! `claude auth login` need the terminal to themselves.

use std::ffi::OsString;
use std::io::{BufRead, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use super::{Cli, INSTALL_COMMAND, LOGIN_COMMAND};
use crate::config::Config;

pub struct Setup<'a> {
    pub path: Option<OsString>,
    pub home: Option<PathBuf>,
    pub install_command: &'a str,
    pub input: &'a mut dyn BufRead,
    pub output: &'a mut dyn Write,
}

impl Setup<'_> {
    /// `Ok(true)` once Claude Code is installed and signed in, `Ok(false)`
    /// when the user declined the install.
    pub fn run(&mut self) -> Result<bool> {
        let cli = match self.find() {
            Some(cli) => cli,
            None => {
                let command = self.install_command;
                writeln!(
                    self.output,
                    "Claude Code is not installed. Wizard can install it with Anthropic's \
                     installer:\n\n    {command}\n"
                )?;
                if !self.confirm("Run it now? [y/N] ")? {
                    writeln!(
                        self.output,
                        "Nothing installed. To install it yourself, run:\n\n    {command}"
                    )?;
                    return Ok(false);
                }
                if !shell(command)
                    .status()
                    .context("running the installer")?
                    .success()
                {
                    bail!("the Claude Code installer failed");
                }
                self.find()
                    .context("the installer finished but `claude` is not on PATH")?
            }
        };
        writeln!(self.output, "Found claude {}.", cli.version()?)?;

        if !cli.signed_in()? {
            writeln!(
                self.output,
                "Claude Code is not signed in. Starting `{LOGIN_COMMAND}`."
            )?;
            cli.login()?;
            if !cli.signed_in()? {
                bail!("Claude Code is still not signed in. Run: {LOGIN_COMMAND}");
            }
        }
        Ok(true)
    }

    fn find(&self) -> Option<Cli> {
        Cli::find_in(self.path.as_deref(), self.home.clone())
    }

    fn confirm(&mut self, question: &str) -> Result<bool> {
        write!(self.output, "{question}")?;
        self.output.flush()?;
        let mut answer = String::new();
        self.input.read_line(&mut answer)?;
        Ok(matches!(
            answer.trim().to_ascii_lowercase().as_str(),
            "y" | "yes"
        ))
    }
}

fn shell(command: &str) -> std::process::Command {
    let (program, flag) = if cfg!(windows) {
        ("powershell", "-Command")
    } else {
        ("sh", "-c")
    };
    let mut shell = std::process::Command::new(program);
    shell.args([flag, command]);
    shell
}

/// Install and sign in Claude Code on this process's terminal. `Ok(false)`
/// when the user declined the install.
pub fn ensure_ready() -> Result<bool> {
    Setup {
        path: std::env::var_os("PATH"),
        home: dirs::home_dir(),
        install_command: INSTALL_COMMAND,
        input: &mut std::io::stdin().lock(),
        output: &mut std::io::stdout(),
    }
    .run()
}

/// Run the setup on this process's terminal, then store the provider and make
/// it the active one. `Ok(None)` when the user declined the install.
pub fn run_blocking() -> Result<Option<Config>> {
    if !ensure_ready()? {
        return Ok(None);
    }
    let mut config = Config::load().context("loading config for Claude Code setup")?;
    let provider = super::provider_config();
    config.providers.retain(|other| other.name != provider.name);
    config.active_provider = Some(provider.name.clone());
    config.providers.push(provider);
    config
        .save()
        .context("saving config from Claude Code setup")?;
    Ok(Some(config))
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::fake::FakeClaude;
    use super::*;

    /// Runs the setup against `fake` with `answer` typed at any prompt, and
    /// returns what it decided and what it printed.
    fn run(fake: &FakeClaude, install_command: &str, answer: &str) -> (Result<bool>, String) {
        let mut output = Vec::new();
        let outcome = Setup {
            path: Some(fake.path()),
            home: None,
            install_command,
            input: &mut answer.as_bytes(),
            output: &mut output,
        }
        .run();
        (outcome, String::from_utf8(output).expect("utf-8"))
    }

    #[test]
    fn a_missing_claude_is_installed_only_after_a_yes() {
        let fake = FakeClaude::new(false);
        let installer = fake.installer();

        let (outcome, printed) = run(&fake, &installer, "y\n");

        assert!(outcome.expect("installed and signed in"));
        let shown = printed.find(&installer).expect("the command is shown");
        let asked = printed.find("Run it now?").expect("the question is asked");
        assert!(shown < asked, "{printed}");
        assert!(printed.contains("Found claude 9.9.9"), "{printed}");
    }

    #[test]
    fn declining_the_install_prints_the_command_and_installs_nothing() {
        for answer in ["n\n", "\n", ""] {
            let fake = FakeClaude::new(false);
            let installer = fake.installer();

            let (outcome, printed) = run(&fake, &installer, answer);

            assert!(!outcome.expect("a clean stop"), "answer {answer:?}");
            assert!(!fake.file("claude").exists(), "answer {answer:?}");
            assert!(
                printed.ends_with(&format!("run:\n\n    {installer}\n")),
                "{printed}"
            );
        }
    }

    #[test]
    fn a_failing_installer_is_an_error() {
        let fake = FakeClaude::new(false);
        let (outcome, _) = run(&fake, "exit 3", "yes\n");
        assert!(outcome.is_err());
    }

    #[test]
    fn a_signed_out_claude_is_handed_to_its_own_login_and_checked_again() {
        let fake = FakeClaude::new(true);

        let (outcome, printed) = run(&fake, "false", "");

        assert!(outcome.expect("signed in after the login"));
        assert!(fake.file("signed-in").exists());
        assert!(printed.contains("claude auth login"), "{printed}");
    }

    #[test]
    fn a_login_that_does_not_take_is_an_error_naming_the_command() {
        let fake = FakeClaude::new(true);
        fake.write("login-fails", "");

        let (outcome, _) = run(&fake, "false", "");

        let err = outcome.expect_err("still signed out");
        assert!(err.to_string().ends_with("Run: claude auth login"), "{err}");
    }

    #[test]
    fn a_signed_in_claude_needs_no_questions() {
        let fake = FakeClaude::signed_in();

        let (outcome, printed) = run(&fake, "false", "");

        assert!(outcome.expect("ready"));
        assert_eq!(printed, "Found claude 9.9.9 (Claude Code).\n");
    }
}
