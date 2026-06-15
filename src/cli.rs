//! Command-line interface. Wizard has exactly one mode — the interactive
//! TUI — so the CLI is a thin set of overrides applied before the TUI starts.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Wizard — a Claude-Code-style chat TUI over the NexAU code agent.
#[derive(Parser, Debug, Default)]
#[command(name = "wizard", version, about)]
pub struct Cli {
    /// Run the agent from this directory instead of the current one.
    #[arg(long, value_name = "DIR")]
    pub cwd: Option<PathBuf>,

    /// Override the model tag for this session.
    #[arg(long, value_name = "TAG")]
    pub model: Option<String>,

    /// Pre-fill the input line with this prompt (still requires Enter).
    #[arg(short = 'p', long, value_name = "TEXT")]
    pub prompt: Option<String>,

    /// Force the first-run setup flow even when a config already exists.
    #[arg(long)]
    pub onboard: bool,

    /// Subcommand (e.g. `login xai`). Absent = launch the TUI.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Wizard subcommands.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Sign in to a provider account (`wizard login xai`).
    Login {
        /// The provider to sign in to. Currently only `xai`.
        provider: String,
    },
    /// Drive AHE's harness-evolution loop (`wizard evolve start|status|…`).
    Evolve {
        #[command(subcommand)]
        action: EvolveAction,
    },
}

/// `wizard evolve <action>` — control the AHE evolve loop.
#[derive(Subcommand, Debug)]
pub enum EvolveAction {
    /// Preflight and launch the evolve loop in a detached tmux session.
    Start,
    /// Show the latest experiment's progress (scores + last iteration).
    Status,
    /// List running `ahe-*` tmux sessions.
    Sessions,
    /// Kill a running evolve tmux session.
    Stop {
        /// The tmux session name (see `wizard evolve sessions`).
        session: String,
    },
    /// Print the command to attach to the running evolve session.
    Attach,
}
