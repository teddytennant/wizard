//! What picking "Claude (subscription)" in `/provider` does: find Claude
//! Code, offer to install it, and get it signed in. It runs on the plain
//! terminal with the TUI suspended, because both the installer and
//! `claude auth login` need the terminal to themselves.
//!
//! The install is a remote script, so it only runs after a yes: typed at a
//! one-line question on a terminal, or given up front with `--yes`. Without a
//! terminal and without `--yes` it stops and prints the command.

use std::ffi::OsString;
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use super::{Cli, INSTALL_COMMAND, LOGIN_COMMAND, check_version};
use crate::config::Config;

pub struct Setup<'a> {
    pub path: Option<OsString>,
    pub home: Option<PathBuf>,
    pub install_command: &'a str,
    /// `--yes`: run the installer without asking.
    pub assume_yes: bool,
    /// Whether a person is there to answer. When not, a missing `claude` is an
    /// error naming the command instead of a question nobody reads.
    pub interactive: bool,
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
                if self.assume_yes {
                    writeln!(
                        self.output,
                        "Claude Code is not installed. Running Anthropic's installer: {command}"
                    )?;
                } else if self.interactive {
                    let question = format!(
                        "Claude Code is not installed. Run Anthropic's installer ({command})? [y/N] "
                    );
                    if !self.confirm(&question)? {
                        writeln!(
                            self.output,
                            "Nothing installed. To install it yourself, run:\n\n    {command}"
                        )?;
                        return Ok(false);
                    }
                } else {
                    bail!(
                        "Claude Code is not installed. Install it with: {command} \
                         (or pass --yes to let Wizard run it)"
                    );
                }
                if !shell(command)
                    .status()
                    .context("running the installer")?
                    .success()
                {
                    bail!("the Claude Code installer failed");
                }
                self.find()
                    .context("the installer finished but `claude` was not found on PATH, in ~/.local/bin or in ~/.claude/local")?
            }
        };
        let version = cli.version()?;
        writeln!(self.output, "Found claude {version}.")?;
        check_version(&version)?;

        if !cli.signed_in()? {
            if !self.interactive {
                bail!("Claude Code is not signed in. Run: {LOGIN_COMMAND}");
            }
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
    ensure_ready_with(false)
}

/// [`ensure_ready`], with `assume_yes` standing for `--yes`.
pub fn ensure_ready_with(assume_yes: bool) -> Result<bool> {
    let mut input = std::io::stdin().lock();
    Setup {
        path: std::env::var_os("PATH"),
        home: dirs::home_dir(),
        install_command: INSTALL_COMMAND,
        assume_yes,
        interactive: std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        input: &mut input,
        output: &mut std::io::stdout(),
    }
    .run()
}

/// `--provider claude`: make sure Claude Code is installed and signed in, then
/// make it this run's provider. Nothing is written to `config.toml`; `/provider`
/// is what saves a choice.
pub fn select_for_run(config: &mut Config, assume_yes: bool) -> Result<()> {
    if !ensure_ready_with(assume_yes)? {
        bail!("Claude Code is not installed");
    }
    let provider = super::provider_config();
    config.active_provider = Some(provider.name.clone());
    match config
        .providers
        .iter_mut()
        .find(|other| other.name == provider.name)
    {
        Some(existing) => *existing = provider,
        None => config.providers.push(provider),
    }
    Ok(())
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
        run_as(fake, install_command, answer, false, true)
    }

    fn run_as(
        fake: &FakeClaude,
        install_command: &str,
        answer: &str,
        assume_yes: bool,
        interactive: bool,
    ) -> (Result<bool>, String) {
        let mut output = Vec::new();
        let outcome = Setup {
            path: Some(fake.path()),
            home: None,
            install_command,
            assume_yes,
            interactive,
            input: &mut answer.as_bytes(),
            output: &mut output,
        }
        .run();
        (outcome, String::from_utf8(output).expect("utf-8"))
    }

    #[test]
    fn yes_installs_without_asking() {
        let fake = FakeClaude::new(false);
        fake.write("signed-in", "");
        let installer = fake.installer();

        let (outcome, printed) = run_as(&fake, &installer, "", true, false);

        assert!(outcome.expect("installed and signed in"));
        assert!(fake.file("claude").exists());
        assert!(!printed.contains("[y/N]"), "{printed}");
        assert!(printed.contains(&installer), "{printed}");
    }

    #[test]
    fn nobody_to_ask_and_no_yes_installs_nothing_and_names_the_command() {
        let fake = FakeClaude::new(false);
        let installer = fake.installer();

        let (outcome, _) = run_as(&fake, &installer, "y\n", false, false);

        let err = outcome.expect_err("a script is not asked");
        assert!(err.to_string().contains(&installer), "{err}");
        assert!(err.to_string().contains("--yes"), "{err}");
        assert!(!fake.file("claude").exists());
    }

    #[test]
    fn nobody_to_sign_in_is_told_the_login_command() {
        let fake = FakeClaude::new(true);

        let (outcome, _) = run_as(&fake, "false", "", false, false);

        let err = outcome.expect_err("no browser to open");
        assert!(err.to_string().ends_with("Run: claude auth login"), "{err}");
        assert!(!fake.file("signed-in").exists());
    }

    #[test]
    fn a_claude_older_than_the_floor_is_told_to_update() {
        let fake = FakeClaude::signed_in();
        fake.write("version", "1.0.88 (Claude Code)");

        let (outcome, _) = run(&fake, "false", "");

        let err = outcome.expect_err("too old");
        assert!(err.to_string().ends_with("Run: claude update"), "{err}");
    }

    #[test]
    fn the_older_install_directory_is_searched_after_local_bin() {
        let home = tempfile::tempdir().expect("home");
        let local = home.path().join(".claude").join("local");
        std::fs::create_dir_all(&local).expect("dir");
        std::os::unix::fs::symlink(
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake-claude"),
            local.join("claude"),
        )
        .expect("link");

        let found = Cli::find_in(
            Some(std::ffi::OsStr::new("")),
            Some(home.path().to_path_buf()),
        );

        assert!(found.is_some());
    }

    #[test]
    fn a_missing_claude_is_installed_only_after_a_yes() {
        let fake = FakeClaude::new(false);
        let installer = fake.installer();

        let (outcome, printed) = run(&fake, &installer, "y\n");

        assert!(outcome.expect("installed and signed in"));
        assert_eq!(printed.matches("[y/N]").count(), 1, "{printed}");
        assert!(
            printed.contains(&installer),
            "the command is in the question: {printed}"
        );
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
