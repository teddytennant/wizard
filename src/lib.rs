//! Wizard — a Claude-Code-style chat TUI whose backend is the NexAU code
//! agent, reached over a Python bridge subprocess.
//!
//! A Ratatui front end ([`ui`], [`app`]) streams [`agent::AgentEvent`]s from
//! the bridge ([`backend::nexau`]) and renders them. There is exactly one
//! mode: the interactive TUI.

pub mod agent;
pub mod app;
pub mod auth;
pub mod backend;
pub mod cli;
pub mod commands;
pub mod config;
pub mod event;
pub mod evolve;
pub mod onboarding;
pub mod ui;

use std::io::IsTerminal;

use anyhow::Result;

/// Top-level entry point: apply CLI overrides, load (or onboard) the config,
/// then launch the interactive TUI. Returns the process exit code.
pub async fn run(cli: cli::Cli) -> Result<i32> {
    if let Some(cli::Command::Login { provider }) = &cli.command {
        return login(provider).await.map(|()| 0);
    }

    if let Some(cli::Command::Evolve { action }) = &cli.command {
        return run_evolve(action);
    }

    if let Some(dir) = &cli.cwd {
        std::env::set_current_dir(dir)?;
    }

    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();

    let existing = config::Config::load()?;
    // Onboard when forced, when there is no config, or when the config it
    // loaded cannot authenticate (e.g. a key env var that is not set) — so a
    // broken config opens setup instead of hard-failing at launch.
    let needs_onboarding = cli.onboard
        || existing.is_none()
        || existing.as_ref().is_some_and(|c| !c.has_usable_auth());

    let mut config = if let Some(config) = existing.clone().filter(|_| !needs_onboarding) {
        config
    } else {
        if !interactive {
            anyhow::bail!(
                "no usable config at {} — run wizard in an interactive terminal \
                 (or `wizard --onboard`) to set up a provider",
                config::Config::path()?.display()
            );
        }
        match onboarding::run().await? {
            Some(config) => config,
            // Cancelled: fall back to the existing config only if it can
            // actually authenticate; otherwise there is nothing to launch.
            None => match existing.filter(config::Config::has_usable_auth) {
                Some(config) => config,
                None => {
                    println!("setup cancelled — run `wizard --onboard` any time.");
                    return Ok(0);
                }
            },
        }
    };

    if let Some(model) = &cli.model {
        config.model = model.clone();
    }

    app::run_tui(config, cli).await
}

/// Handle `wizard evolve <action>`: drive AHE's evolve loop. Loads the config
/// (without onboarding), reads its `[evolve]` section, and runs the matching
/// [`evolve`] action, printing results. Returns the process exit code.
fn run_evolve(action: &cli::EvolveAction) -> Result<i32> {
    let config = config::Config::load()?.unwrap_or_default();
    let evolve = config.evolve_ready()?;

    match action {
        cli::EvolveAction::Start => {
            let session = evolve::start(evolve)?;
            println!("evolve launched in tmux session '{session}'.");
            println!("  attach:  tmux attach -t {session}");
            println!("  status:  wizard evolve status");
            println!("  stop:    wizard evolve stop {session}");
        }
        cli::EvolveAction::Status => println!("{}", evolve::status(evolve)?),
        cli::EvolveAction::Sessions => {
            let sessions = evolve::sessions()?;
            if sessions.is_empty() {
                println!("no ahe-* evolve sessions running.");
            } else {
                println!("running evolve sessions:");
                for session in sessions {
                    println!("  {session}  (attach: tmux attach -t {session})");
                }
            }
        }
        cli::EvolveAction::Stop { session } => {
            evolve::stop(session)?;
            println!("stopped tmux session '{session}'.");
        }
        cli::EvolveAction::Attach => match evolve::sessions()?.first() {
            Some(session) => {
                println!("attach with:  tmux attach -t {session}");
            }
            None => {
                println!("no ahe-* evolve sessions running — start one with `wizard evolve start`.")
            }
        },
    }
    Ok(0)
}

/// Handle `wizard login <provider>`: run the provider's OAuth flow, printing
/// progress to stdout.
async fn login(provider: &str) -> Result<()> {
    match provider {
        "xai" => auth::xai_oauth::login(|line| println!("{line}")).await,
        other => anyhow::bail!("unknown login provider '{other}' (try: xai)"),
    }
}
