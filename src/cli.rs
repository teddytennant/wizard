//! Command-line argument parsing (`clap` derive).
//!
//! Flags per `docs/architecture.md` (CLI section) and `docs/modes.md`.

use std::path::PathBuf;

use clap::Parser;

use crate::config::Mode;

/// Wizard — your sovereign coding wizard. Self-extending. Fully local.
#[derive(Debug, Clone, Parser)]
#[command(name = "wizard", version, about, long_about = None)]
pub struct Cli {
    /// Personality mode: genie (interactive TUI) or sovereign (autonomous).
    #[arg(long, value_enum)]
    pub mode: Option<Mode>,

    /// Initial task. Pre-fills the first message in genie mode; the task to
    /// complete in sovereign / evolve mode.
    #[arg(short, long)]
    pub prompt: Option<String>,

    /// Self-extension mode: run the /evolve pipeline from the CLI.
    #[arg(long)]
    pub evolve: bool,

    /// Deep evolve (tier 2): rebuild Wizard's own source. Implies --evolve.
    #[arg(long, requires = "evolve")]
    pub deep: bool,

    /// Skip confirmation prompts (implicit in sovereign mode).
    #[arg(long)]
    pub auto: bool,

    /// Time limit in hours for a sovereign-mode run.
    #[arg(long)]
    pub max_hours: Option<f64>,

    /// Max outer loop iterations for a sovereign-mode run.
    #[arg(long = "loop", value_name = "N")]
    pub loop_limit: Option<u32>,

    /// Run sovereign mode perpetually: keep working toward the goal,
    /// self-directing and self-improving, until stopped (loop-control
    /// `stop` or --max-hours). Implies --mode sovereign.
    #[arg(long)]
    pub continuous: bool,

    /// Project root override (defaults to the current directory).
    #[arg(long)]
    pub cwd: Option<PathBuf>,

    /// Resume the most recent session instead of starting fresh.
    #[arg(long)]
    pub resume: bool,
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::*;
    use crate::config::Mode;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("wizard").chain(args.iter().copied()))
    }

    #[test]
    fn clap_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn defaults_when_no_args() {
        let cli = parse(&[]).expect("bare invocation parses");
        assert_eq!(cli.mode, None);
        assert_eq!(cli.prompt, None);
        assert!(!cli.evolve);
        assert!(!cli.deep);
        assert!(!cli.auto);
        assert_eq!(cli.max_hours, None);
        assert_eq!(cli.loop_limit, None);
        assert!(!cli.continuous);
        assert_eq!(cli.cwd, None);
        assert!(!cli.resume);
    }

    #[test]
    fn parses_all_documented_flags() {
        let cli = parse(&[
            "--mode",
            "sovereign",
            "-p",
            "add tests",
            "--auto",
            "--max-hours",
            "1.5",
            "--loop",
            "10",
            "--continuous",
            "--cwd",
            "/tmp/project",
            "--resume",
        ])
        .expect("full flag set parses");
        assert_eq!(cli.mode, Some(Mode::Sovereign));
        assert_eq!(cli.prompt.as_deref(), Some("add tests"));
        assert!(cli.auto);
        assert_eq!(cli.max_hours, Some(1.5));
        assert_eq!(cli.loop_limit, Some(10));
        assert!(cli.continuous);
        assert_eq!(
            cli.cwd.as_deref(),
            Some(std::path::Path::new("/tmp/project"))
        );
        assert!(cli.resume);
    }

    #[test]
    fn long_prompt_flag_works() {
        let cli = parse(&["--prompt", "task"]).expect("long form parses");
        assert_eq!(cli.prompt.as_deref(), Some("task"));
    }

    #[test]
    fn evolve_flags() {
        let cli = parse(&["--evolve", "-p", "add a skill"]).expect("evolve parses");
        assert!(cli.evolve);
        assert!(!cli.deep);

        let cli = parse(&["--evolve", "--deep", "-p", "new panel"]).expect("deep evolve parses");
        assert!(cli.evolve);
        assert!(cli.deep);
    }

    #[test]
    fn deep_requires_evolve() {
        let err = parse(&["--deep"]).expect_err("--deep alone must be rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn rejects_unknown_mode() {
        let err = parse(&["--mode", "warlock"]).expect_err("unknown mode must be rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
    }
}
