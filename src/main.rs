//! Binary entry point: parse arguments, install a terminal-restoring panic
//! hook, and hand off to the library runner. Every routing decision (the genie
//! TUI, the sovereign headless loop, `--evolve`, and each subcommand) lives in
//! [`wizard::run`], so there is exactly one place that answers "what does this
//! invocation do".
//!
//! What stays here is only what cannot happen anywhere else: the environment
//! writes below are safe *because* no other thread exists yet, and the panic
//! hook has to be installed before any surface that could take the terminal
//! away has started.

use std::io::Write;

fn main() {
    // `cli::parse` rather than `Cli::parse`: the subcommand listing in
    // `--help` is built from what this build's plugins registered, and the
    // help *subcommand* for a plugin-owned tree is forwarded to the plugin.
    // Parsing is unchanged; see `wizard::cli::parse`.
    let cli = wizard::cli::parse();

    // Publish `--harness-dir` as `$WIZARD_HARNESS_DIR` before the tokio
    // runtime exists: every subsystem (prompts, registry, skills, subagents)
    // and every spawned wizard child process then resolves the same bundle
    // from one source.
    // SAFETY: no other threads have been spawned yet.
    if let Some(dir) = &cli.harness_dir {
        unsafe { std::env::set_var("WIZARD_HARNESS_DIR", dir) };
    }

    // Diagnostics before any surface starts. Every `tracing` event emitted
    // before the subscriber is installed is dropped, and the surfaces below
    // (the TUI, the ACP and MCP stdio servers, the GUI) are exactly the ones
    // that cannot print a diagnostic themselves. Failure is silent by design:
    // see `wizard::logging::init`.
    let _ = wizard::logging::init();

    // If the TUI is up when something panics, raw mode and the alternate
    // screen must be torn down before the panic message prints, or the
    // terminal is left unusable. The panic also goes to the session log: on
    // the alternate screen, or inside `wizard acp` where the editor swallows
    // the process's stderr, the printed message is often never seen at all.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        wizard::app::restore_terminal_best_effort();
        wizard::logging::log_panic(info);
        default_hook(info);
    }));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");
    let code = match runtime.block_on(wizard::run(cli)) {
        Ok(code) => code,
        Err(err) => {
            // Make sure the error lands on a sane terminal even when the TUI
            // errored out mid-frame.
            wizard::app::restore_terminal_best_effort();
            eprintln!("error: {err:#}");
            1
        }
    };
    // Headless runs encode their outcome in the exit code (see
    // `wizard::output::exit_code`); flush before exiting so structured
    // output is never cut off.
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}
