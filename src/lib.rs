//! Wizard — a single-binary agent.
//!
//! A Ratatui front end on top of a multi-provider agent loop (local
//! llama.cpp/Ollama or remote APIs) with an extensible tool set
//! (native + LuaJIT-scripted + MCP) and tiered self-extension.
//! See `docs/architecture.md` for the full design.

pub mod acp;
pub mod agent;
pub mod app;
pub mod checkpoint;
pub mod claude_resume;
pub mod claude_session;
pub mod cli;
pub mod commands;
pub mod config;
pub mod credentials;
pub mod dispatch;
pub mod doctor;
pub mod event;
pub mod evolve;
pub mod fleet;
pub mod gates;
pub mod gateway;
pub mod git_util;
pub mod graph;
// The agent core the native window is built on (sessions, config store, git,
// OAuth). It used to carry a browser GUI too — an axum server and a JavaScript
// page — and was compiled into every build for it. That surface is gone, and
// with it the reason: the window is now the only caller, so the module follows
// it behind the same feature. See `docs/native-gui.md`.
#[cfg(feature = "native")]
pub mod gui;
pub mod hardware;
pub mod harness;
pub mod headless;
pub mod hooks;
pub mod image_view;
pub mod images;
pub mod import_claude;
pub mod instructions;
pub mod kernel;
pub mod llm;
pub mod local_setup;
pub mod logging;
pub mod mcp;
pub mod memory;
pub mod mesh;
#[cfg(feature = "native")]
pub mod native;
pub mod onboarding;
pub mod output;
pub mod platform;
pub mod progress;
pub mod registry_client;
pub mod schedule;
pub mod server;
pub mod session_registry;
pub mod skills;
pub mod skin;
pub mod sync;
pub mod theme;
pub mod tools;
pub mod transcript;
pub mod trust;
pub mod ui;
pub mod update;
pub mod usage;
pub mod vim;

use std::io::IsTerminal;

use anyhow::Result;

use crate::config::Mode;

/// Top-level entry point: load config, apply CLI overrides, and dispatch to
/// the selected run mode (genie TUI, sovereign headless loop, or `--evolve`).
///
/// Returns the process exit code: headless runs map their outcome through
/// [`output::exit_code`] (0 completed, 2 max-steps, 3 circuit breaker, 4 time
/// limit); every other mode exits 0 on success. Hard errors surface as `Err`
/// and exit 1 from `main`.
pub async fn run(mut cli: cli::Cli) -> Result<i32> {
    // Top-level flags are not global: the self-contained subcommands below
    // read none of them (only --cwd). Reject the combination loudly instead
    // of silently dropping the flags (`wizard --plan fleet run` must not run
    // an un-planned fleet). `wizard agents` is exempt: it goes through the
    // normal config path, where the flags do apply.
    //
    // Every subcommand reaches this check, `resume` included. That is the
    // reason it has to run before the dispatch chain rather than inside it:
    // `resume` clears its own subcommand on the way past (it rewrites the
    // invocation into the `--resume` one it is equivalent to), so a check
    // placed after the chain would see no subcommand and let
    // `wizard --plan resume` through with the flag silently dropped.
    if let Some(command) = &cli.command
        && !matches!(command, cli::Command::Agents)
    {
        let ignored = cli.ignored_top_level_flags();
        if !ignored.is_empty() {
            anyhow::bail!(
                "{} cannot be combined with a `wizard` subcommand; these top-level flags \
                 would be ignored, so drop them (only --cwd applies)",
                ignored.join(", ")
            );
        }
    }

    // Harness bundle tooling is self-contained: no config, no LLM.
    // (`--harness-dir` itself is published as `$WIZARD_HARNESS_DIR` in
    // `main`, pre-runtime.)
    if let Some(cli::Command::Harness { cmd }) = &cli.command {
        if let Some(dir) = &cli.cwd {
            std::env::set_current_dir(dir)?;
        }
        return harness::run(cmd.clone()).map(|()| 0);
    }

    // `desktop-setup` is system provisioning for the `computer` tool: it
    // installs OS packages and reports permissions, so it wants no config, no
    // onboarding and no LLM.
    if let Some(cli::Command::DesktopSetup) = &cli.command {
        return tools::computer::setup::run().map(|()| 0);
    }

    // MCP server: expose Wizard's native tools over stdio to another MCP
    // client. Self-contained — no config, no onboarding, no LLM — so it
    // dispatches before the config load like the other tooling subcommands.
    if let Some(cli::Command::McpServe { scripted }) = &cli.command {
        if let Some(dir) = &cli.cwd {
            std::env::set_current_dir(dir)?;
        }
        return mcp::serve::run(*scripted).await.map(|()| 0);
    }

    // Usage rollup: reads ~/.wizard/usage.jsonl only.
    if let Some(cli::Command::Usage { since }) = &cli.command {
        return usage::run_cli(since.as_deref());
    }

    // The window opens existing sessions and builds agents lazily per chat, so
    // it loads config directly (defaults on a fresh install) and never
    // onboards — startup must not depend on a reachable provider.
    if let Some(cli::Command::Gui { native: _ }) = &cli.command {
        if let Some(dir) = &cli.cwd {
            std::env::set_current_dir(dir)?;
        }
        #[cfg(feature = "native")]
        {
            let config = config::Config::load()?;
            return native::run(config).await.map(|()| 0);
        }
        // Two routes, and naming both matters: `wizard app` shipped for a
        // year telling people to rebuild, while the release page carried a
        // binary that already worked. Whoever reads this has a terminal in
        // front of them, so give them the line to paste.
        //
        // The list of what to do instead is not a consolation prize: it is
        // what replaced the browser GUI, which used to be the answer here and
        // was deleted. A headless box is reached by running the TUI over SSH,
        // by `wizard -p`, by an ACP editor, or through the Telegram gateway —
        // none of which need a window or a port.
        #[cfg(not(feature = "native"))]
        anyhow::bail!(
            "this build has no native GUI — it was built without the `native` feature.\n\
             \n\
             The window is a separate build because it links iced, several hundred crates\n\
             that a headless `wizard -p` or `wizard acp` never executes a line of, and that\n\
             cannot go into the static musl binary.\n\
             \n\
             To get one:\n\
             \x20 curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh \\\n\
             \x20   | WIZARD_NATIVE=1 bash        # installs `wizard-native` beside `wizard`\n\
             \x20 cargo build --release --features native   # from a checkout\n\
             \n\
             To drive this machine without one: `wizard` over SSH, `wizard -p '<prompt>'`,\n\
             `wizard acp` from an ACP editor, or `wizard gateway` for Telegram.\n\
             See docs/native-gui.md."
        );
    }

    // ACP server: an editor drives Wizard over stdin/stdout, so it must not
    // onboard or open a TUI. Loads config directly (defaults on a fresh
    // install) like the window, then serves until the client closes the pipe.
    if let Some(cli::Command::Acp) = &cli.command {
        if let Some(dir) = &cli.cwd {
            std::env::set_current_dir(dir)?;
        }
        let config = config::Config::load()?;
        return acp::run(config).await.map(|()| 0);
    }

    // Evolution history: reads ~/.wizard/evolution.jsonl and touches the
    // recorded artifacts directly (list / undo) — no config, no LLM.
    if let Some(cli::Command::Evolve { cmd }) = &cli.command {
        return evolve::run_history_cli(cmd.clone());
    }

    // Self-update: `wizard update` downloads a release binary from GitHub,
    // verifies its checksum, and swaps it in. Self-contained — no config, no
    // onboarding, no LLM — so it dispatches before the config load too.
    if let Some(cli::Command::Update {
        check,
        to,
        force,
        rollback,
    }) = &cli.command
    {
        if let Some(dir) = &cli.cwd {
            std::env::set_current_dir(dir)?;
        }
        return update::run(*check, to.clone(), *force, *rollback).await;
    }

    // Config/skills sync: `wizard sync` packs and pulls signed bundles of
    // portable ~/.wizard state. Self-contained — no config load (pull reads
    // `[sync].source` from config.toml directly), no onboarding, no LLM — so
    // it dispatches before the config load like `update`.
    if let Some(cli::Command::Sync { cmd }) = &cli.command {
        if let Some(dir) = &cli.cwd {
            std::env::set_current_dir(dir)?;
        }
        return sync::run(cmd.clone()).await;
    }

    // Doctor diagnoses the environment, starting with "does the config
    // parse?", so it too dispatches before the config load and can never
    // trigger onboarding. Exits 0 when no check failed, 1 otherwise; with
    // `--bundle` it also writes the redacted bug-report bundle.
    if let Some(bundle) = doctor_request(&cli) {
        if let Some(dir) = &cli.cwd {
            std::env::set_current_dir(dir)?;
        }
        return doctor::run(bundle).await;
    }

    // Schedule CRUD and the scheduler daemon are config-independent too:
    // they only touch ~/.wizard/schedule.toml, and the jobs they spawn are
    // wizard child processes that load config themselves. `schedule run`
    // propagates the child's exit code.
    if let Some(cli::Command::Schedule { cmd }) = &cli.command {
        if let Some(dir) = &cli.cwd {
            std::env::set_current_dir(dir)?;
        }
        return schedule::run(cmd.clone()).await;
    }
    // `wizard scheduler` bare is still the foreground daemon; with a
    // subcommand it manages that daemon as a background service. Both are
    // config-independent, so both dispatch here.
    if let Some(cli::Command::Scheduler { cmd }) = &cli.command {
        if let Some(dir) = &cli.cwd {
            std::env::set_current_dir(dir)?;
        }
        return match cmd {
            Some(cmd) => schedule::run_service(*cmd),
            None => schedule::run_daemon().await,
        };
    }

    // Gateway service management writes a unit under ~/.config/systemd/user
    // (or ~/Library/LaunchAgents) and talks to the supervisor. The chdir
    // above is load-bearing here: `install` captures the current directory as
    // the gateway's project root, and a gateway turn runs in a project.
    //
    // `setup` dispatches from the same arm but *not* through
    // `gateway::service`: it writes no unit and asks the supervisor nothing,
    // so it has to keep working on the hosts where `service::dispatch` refuses
    // outright (Termux, a Linux without systemd). Getting a bot configured is
    // exactly as useful there — you just supervise it yourself.
    if let Some(cli::Command::Gateway { cmd }) = &cli.command {
        if let Some(dir) = &cli.cwd {
            std::env::set_current_dir(dir)?;
        }
        return match cmd {
            cli::GatewayCmd::Setup => gateway::setup::run().await,
            cli::GatewayCmd::Service(cmd) => gateway::service::run(*cmd),
        };
    }

    // Fleet dispatches before the normal flow too, but `fleet run` loads
    // config itself (its planning and synthesis turns drive a real
    // in-process agent); `fleet status` / `fleet stop` only touch the
    // project's `.wizard/fleet/` directory.
    if let Some(cli::Command::Fleet { cmd }) = &cli.command {
        if let Some(dir) = &cli.cwd {
            std::env::set_current_dir(dir)?;
        }
        return fleet::run(cmd.clone()).await;
    }

    // The registry client and the mesh peer store are self-contained in the
    // way `sync` is: no config, no onboarding, no LLM. Neither one reads the
    // working directory (both work entirely under ~/.wizard), which is why
    // they are the only arms in this chain without a chdir.
    if let Some(cli::Command::Skills { cmd }) = &cli.command {
        return registry_client::run_cli(cmd.clone()).await;
    }
    if let Some(cli::Command::Peers { cmd }) = &cli.command {
        return mesh::cli::run(cmd.clone()).await;
    }

    // `wizard resume` is the subcommand spelling of `--resume`, so unlike
    // every arm above it does not return: it rewrites this invocation into the
    // equivalent `--resume` one and falls through into the normal flow below.
    // With `--claude` the rewrite converts a Claude Code conversation into a
    // Wizard session first (see [`claude_resume`]), which is why it runs ahead
    // of the config load rather than inside the resume path proper: it decides
    // *which* session the `--resume` below will then find. It also applies
    // `--cwd` itself, before the chdir further down, because Claude Code files
    // its sessions under a slug of the working directory and a listing taken
    // from the wrong directory is a different project's.
    if let Some(request) = claude_resume::Request::from_cli(&cli) {
        // Detached before the rewrite: what comes back is a plain `--resume`
        // invocation, and leaving the subcommand attached would send it
        // straight back into this arm.
        cli.command = None;
        match claude_resume::prepare(cli, request)? {
            Some(rewritten) => cli = rewritten,
            // `--list` printed what it was asked for, or the picker was
            // cancelled. Either way there is nothing left to resume, and
            // neither is an error.
            None => return Ok(0),
        }
    }

    // `--login` is a one-shot credential flow: no config, no onboarding,
    // no TUI. Tokens land in a dedicated file under ~/.wizard/.
    if let Some(provider) = &cli.login {
        // This flow owns the terminal, so a session whose browser cannot reach
        // our loopback listener — an SSH login without a forwarded port — can
        // still carry the redirect back by hand. Piped stdin has no human to
        // prompt, and its EOF would only close the channel again.
        let paste = if std::io::stdin().is_terminal() {
            llm::oauth_callback::PasteChannel::Stdin
        } else {
            llm::oauth_callback::PasteChannel::Disabled
        };
        return match provider.as_str() {
            "xai" => llm::xai_oauth::login(|line: &str| println!("{line}"), paste, false)
                .await
                .map(|()| 0),
            "chatgpt" => llm::chatgpt_oauth::login(|line: &str| println!("{line}"), paste)
                .await
                .map(|()| 0),
            other => {
                anyhow::bail!("unknown login provider '{other}' (supported: xai, chatgpt)")
            }
        };
    }

    // First-run onboarding: build a fresh config interactively when requested,
    // or automatically on a fresh install in an interactive terminal. A
    // cancelled wizard exits gracefully without touching anything.
    let mut config = if should_onboard(&cli)? {
        match onboarding::run().await? {
            Some(config) => config,
            None => {
                println!("onboarding cancelled — run `wizard --onboard` any time.");
                return Ok(0);
            }
        }
    } else {
        let config_path = config::Config::path()?;
        if !config_path.exists() {
            // Non-interactive first runs (piped stdout, CI, cron) must not
            // silently fall back to a baked-in local provider — there is no
            // config yet and onboarding needs a TTY.
            let headless_with_prompt =
                cli.prompt.is_some() && (cli.mode == Some(Mode::Sovereign) || cli.continuous);
            if !headless_with_prompt {
                anyhow::bail!(
                    "no config at {} — run `wizard` in an interactive terminal \
                     (or `wizard --onboard`) to pick a provider",
                    config_path.display()
                );
            }
        }
        config::Config::load()?
    };
    config.apply_cli(&cli);

    if let Some(dir) = &cli.cwd {
        std::env::set_current_dir(dir)?;
    }

    if cli.publish {
        return evolve::run_publish_cli(config, cli).await.map(|()| 0);
    }

    if cli.evolve {
        return evolve::run_cli(config, cli).await.map(|()| 0);
    }

    if cli.gateway {
        return gateway::run(config, cli).await.map(|()| 0);
    }

    // `wizard agents` always opens the TUI dashboard, regardless of the
    // configured default mode.
    if matches!(cli.command, Some(cli::Command::Agents)) {
        // Passive self-update: print any cached "update available" notice now,
        // before the TUI takes the alternate screen, then refresh the cache in
        // the background (fire-and-forget, so it never delays the TUI).
        // Sovereign is headless and skips both (handled in the match below).
        update::print_startup_notice(&config.update);
        update::maybe_check_on_startup(&config.update).await;
        return app::run_tui(config, cli).await;
    }

    match config.mode {
        Mode::Genie => {
            update::print_startup_notice(&config.update);
            update::maybe_check_on_startup(&config.update).await;
            app::run_tui(config, cli).await
        }
        Mode::Sovereign => headless::run(config, cli).await,
    }
}

/// `Some(bundle)` when this invocation is a doctor run, where `bundle` is the
/// `--bundle` flag; `None` otherwise.
///
/// Split out of [`run`] so the wiring from the parsed flag to
/// [`doctor::run_bundle`] is unit-testable: [`run`] itself probes every
/// configured provider over the network and writes under `~/.wizard`, so no
/// test can call it. `wizard doctor --bundle` shipped dead once already
/// (the subcommand was a unit variant and clap rejected the flag), and the
/// only reason nothing caught it was that this decision had no name.
fn doctor_request(cli: &cli::Cli) -> Option<bool> {
    match &cli.command {
        Some(cli::Command::Doctor { bundle }) => Some(*bundle),
        _ => None,
    }
}

/// Decide whether to run onboarding before the normal flow.
///
/// `--onboard` forces it (when a terminal is available); otherwise it runs
/// only on a genuine first run: the config file is absent, stdin/stdout are a
/// terminal, and this is not a publish / evolve / gateway invocation or a
/// headless-with-prompt sovereign run. A non-interactive run never onboards,
/// so piping into Wizard never blocks.
fn should_onboard(cli: &cli::Cli) -> Result<bool> {
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    should_onboard_given(cli, interactive)
}

/// Testable core of [`should_onboard`]; `interactive` is whether stdin and
/// stdout are both terminals.
fn should_onboard_given(cli: &cli::Cli, interactive: bool) -> Result<bool> {
    // Subcommands are dispatched before this is ever consulted; the
    // check here is a defensive guarantee that they can never onboard.
    if cli.command.is_some() {
        return Ok(false);
    }
    if cli.publish || cli.evolve || cli.gateway || cli.login.is_some() {
        return Ok(false);
    }
    if !interactive {
        return Ok(false);
    }
    if cli.onboard {
        return Ok(true);
    }
    // Headless-with-prompt sovereign runs are batch jobs — don't interrupt them.
    let headless_with_prompt =
        cli.prompt.is_some() && (cli.mode == Some(Mode::Sovereign) || cli.continuous);
    let config_missing = !config::Config::path()?.exists();
    Ok(config_missing && !headless_with_prompt)
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    fn parse(args: &[&str]) -> cli::Cli {
        cli::Cli::try_parse_from(args).expect("cli parses")
    }

    #[test]
    fn onboard_flag_forces_onboarding_in_an_interactive_terminal() {
        assert!(should_onboard_given(&parse(&["wizard", "--onboard"]), true).unwrap());
    }

    #[test]
    fn non_interactive_runs_never_onboard() {
        assert!(!should_onboard_given(&parse(&["wizard", "--onboard"]), false).unwrap());
        assert!(!should_onboard_given(&parse(&["wizard"]), false).unwrap());
    }

    #[test]
    fn subcommands_never_onboard() {
        assert!(!should_onboard_given(&parse(&["wizard", "doctor"]), true).unwrap());
        assert!(!should_onboard_given(&parse(&["wizard", "agents"]), true).unwrap());
    }

    #[test]
    fn dedicated_run_modes_suppress_onboarding() {
        for args in [
            &["wizard", "--gateway"][..],
            &["wizard", "--publish"],
            &["wizard", "--evolve"],
            &["wizard", "--login", "xai"],
        ] {
            assert!(
                !should_onboard_given(&parse(args), true).unwrap(),
                "{args:?} must not onboard"
            );
        }
    }

    #[test]
    fn doctor_subcommand_routes_bundle_to_the_bundle_run() {
        // `wizard doctor` prints the report; `wizard doctor --bundle` must
        // reach doctor::run_bundle. The flag used to be invisible to clap, so
        // the bundle was only reachable through an undocumented env var.
        assert_eq!(doctor_request(&parse(&["wizard", "doctor"])), Some(false));
        assert_eq!(
            doctor_request(&parse(&["wizard", "doctor", "--bundle"])),
            Some(true)
        );
        assert!(doctor::bundle_requested(true));
        // Any other invocation is not a doctor run at all.
        assert_eq!(doctor_request(&parse(&["wizard", "agents"])), None);
        assert_eq!(doctor_request(&parse(&["wizard"])), None);
    }

    #[test]
    fn headless_sovereign_prompts_skip_onboarding_even_when_interactive() {
        let sovereign = parse(&["wizard", "--mode", "sovereign", "-p", "task"]);
        assert!(!should_onboard_given(&sovereign, true).unwrap());
        let continuous = parse(&["wizard", "--continuous", "-p", "task"]);
        assert!(!should_onboard_given(&continuous, true).unwrap());
    }
}
