// OWNER: shared (arguments, runtime, terminal setup and teardown)
//! `codexw`: the OpenAI Codex CLI 0.147.0 terminal UI on the wizard backend.

use std::io::{BufWriter, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use agent_core::Request;
use anyhow::{Context, Result, bail};
use codexw::app::{self, App, AppOpts, CODEX_VERSION, Msg, VERSION};
use codexw::style::{ColorLevel, Palette, set_palette};
use codexw::term::{modes, probe};
use ratatui::layout::{Position, Size};
use tokio::sync::mpsc::unbounded_channel;
use tuikit::input::Input;
use tuikit::term::EventPump;

const HELP: &str = "Wizard (codex look)

Usage: wizard-ui-codex [OPTIONS] [PROMPT]
       codexw [OPTIONS] [PROMPT]
       wizard-ui-codex [OPTIONS] resume [SESSION_ID] [PROMPT]

Commands:
  resume  Resume a previous interactive session (picker by default; use --last to continue the
          most recent)

Arguments:
  [PROMPT]
          Optional user prompt to start the session

Options:
  -c, --config <key=value>
          Override a configuration value. Only `model=\"...\"` is read by wizard

  -m, --model <MODEL>
          Model the agent should use, as <provider>/<model>

  -s, --sandbox <SANDBOX_MODE>
          Accepted for compatibility; wizard has no sandbox and runs with danger-full-access

  -C, --cd <DIR>
          Tell the agent to use the specified directory as its working root

  -a, --ask-for-approval <APPROVAL_POLICY>
          Accepted for compatibility; wizard never asks for approval (`never`)

      --no-alt-screen
          Disable alternate screen mode

          Runs the TUI in inline mode, preserving terminal scrollback history.

  -h, --help
          Print help

  -V, --version
          Print version

Environment:
  CODEXW_BACKEND=mock      scripted backend, no model calls
  CODEXW_WIZARD_BIN        path to the wizard binary
  CODEXW_STATE_DIR         where prompt history is kept (default $XDG_STATE_HOME/codexw or
                           ~/.local/state/codexw)
";

#[derive(Debug)]
enum Parsed {
    Run(Box<AppOpts>),
    Exit(String),
}

/// Flags Codex has and wizard cannot back. Naming them beats silently ignoring them.
const UNSUPPORTED: &[&str] = &[
    "--enable",
    "--disable",
    "--remote",
    "--remote-auth-token-env",
    "--strict-config",
    "-i",
    "--image",
    "--oss",
    "--local-provider",
    "-p",
    "--profile",
    "--approve-for-me",
    "--dangerously-bypass-hook-trust",
    "--add-dir",
    "--search",
];

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Parsed, String> {
    let mut o = AppOpts::default();
    let mut cwd: Option<PathBuf> = None;
    let mut it = args.into_iter().peekable();
    let mut positional: Vec<String> = Vec::new();
    let mut resume_mode = false;
    while let Some(a) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match a.as_str() {
            "-h" | "--help" => return Ok(Parsed::Exit(HELP.to_string())),
            "-V" | "--version" => {
                return Ok(Parsed::Exit(format!(
                    "wizard-ui-codex {VERSION} (codex look {CODEX_VERSION})"
                )));
            }
            "-m" | "--model" => o.model = Some(value("--model")?),
            "-C" | "--cd" => cwd = Some(PathBuf::from(value("--cd")?)),
            "--no-alt-screen" => o.no_alt_screen = true,
            "-c" | "--config" => {
                let kv = value("--config")?;
                if let Some(m) = kv.strip_prefix("model=") {
                    o.model = Some(m.trim_matches('"').to_string());
                } else if kv.replace(' ', "") == "tui.status_line=[]" {
                    o.status_line_off = true;
                }
            }
            "-s" | "--sandbox" => {
                let v = value("--sandbox")?;
                if !["read-only", "workspace-write", "danger-full-access"].contains(&v.as_str()) {
                    return Err(format!("invalid value '{v}' for '--sandbox'"));
                }
            }
            "-a" | "--ask-for-approval" => {
                let v = value("--ask-for-approval")?;
                if !["untrusted", "on-request", "never"].contains(&v.as_str()) {
                    return Err(format!("invalid value '{v}' for '--ask-for-approval'"));
                }
            }
            "--dangerously-bypass-approvals-and-sandbox" => {}
            "resume" if positional.is_empty() && !resume_mode => resume_mode = true,
            "--last" if resume_mode => o.resume_last = true,
            "--all" if resume_mode => {}
            s if UNSUPPORTED.contains(&s) => return Err(format!("{s} is not supported by wizard")),
            s if s.starts_with('-') && s.len() > 1 => {
                return Err(format!(
                    "unexpected argument '{s}' found\n\nFor more information, try '--help'."
                ));
            }
            s => positional.push(s.to_string()),
        }
    }
    if resume_mode {
        let mut p = positional.into_iter();
        if !o.resume_last {
            match p.next() {
                Some(id) => o.resume = Some(id),
                None => o.resume_picker = true,
            }
        }
        o.prompt = p.next();
    } else {
        o.prompt = positional.into_iter().next();
    }
    let cwd = match cwd {
        Some(c) => std::path::absolute(&c).map_err(|e| format!("bad directory: {e}"))?,
        None => std::env::current_dir().map_err(|e| e.to_string())?,
    };
    if !cwd.is_dir() {
        return Err(format!("not a directory: {}", cwd.display()));
    }
    o.cwd = cwd;
    o.mock = std::env::var("CODEXW_BACKEND").is_ok_and(|v| v == "mock");
    // Test knob: pin the composer placeholder (0 to 7) so screenshots are repeatable.
    o.placeholder = std::env::var("CODEXW_PLACEHOLDER")
        .ok()
        .and_then(|s| s.parse().ok());
    Ok(Parsed::Run(Box::new(o)))
}

fn flush_stdin() {
    modes::flush_input();
}

async fn real_main(opts: AppOpts) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!("stdin is not a terminal");
    }
    if !std::io::stdout().is_terminal() {
        bail!("stdout is not a terminal");
    }
    let (msg_tx, msg_rx) = unbounded_channel::<Msg>();
    let (tx, fwd) = app::spawn_backend(
        opts.cwd.clone(),
        opts.resume.clone(),
        opts.mock,
        msg_tx.clone(),
    )?;

    let mut out = BufWriter::with_capacity(64 * 1024, std::io::stdout());
    modes::set_modes(&mut out).context("could not set up the terminal")?;
    modes::install_signal_restore();
    flush_stdin();
    let pr = probe::run(!modes::keyboard_enhancement_disabled());
    set_palette(Palette::new(pr.fg, pr.bg, ColorLevel::from_env()));
    let (w, h) = crossterm::terminal::size().context("could not read the terminal size")?;
    let cursor = pr.cursor.map_or(Position::new(0, 0), |(x, y)| {
        Position::new(x, y.min(h.saturating_sub(1)))
    });
    let _ = modes::set_title(
        &mut out,
        &opts
            .cwd
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default(),
    );

    let mut app = App::new(opts, tx, out, Size::new(w, h), cursor);
    app.msg_tx = Some(msg_tx.clone());
    app.pane.composer.set_paste_burst(true);
    app.pane.composer.enable_history_persistence();
    let input = Input::spawn().context("could not read the terminal")?;
    app.input_ctl = Some(input.control());
    // Backend events reach the loop through `msg_rx`; the pump merges them with key events.
    let pump = EventPump::spawn_raw(input, Duration::ZERO, msg_rx);
    let result = app::run(&mut app, pump).await;

    app.finish_screen();
    let lines = app.exit_lines(std::io::stdout().is_terminal());
    let _ = app.tx.send(Request::Shutdown);
    drop(app);
    modes::restore_after_exit();
    let mut so = std::io::stdout();
    for l in lines {
        let _ = writeln!(so, "{l}");
    }
    let _ = so.flush();
    let _ = tokio::time::timeout(Duration::from_secs(3), fwd).await;
    result
}

fn main() -> ExitCode {
    let opts = match parse_args(std::env::args().skip(1)) {
        Ok(Parsed::Run(o)) => *o,
        Ok(Parsed::Exit(text)) => {
            println!("{text}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("codexw: {e}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(real_main(opts)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ERROR: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn flags_parse() {
        let Ok(Parsed::Run(o)) = parse_args(args(&["-m", "a/b", "--no-alt-screen", "hello"]))
        else {
            panic!("did not parse")
        };
        assert_eq!(o.model.as_deref(), Some("a/b"));
        assert!(o.no_alt_screen);
        assert_eq!(o.prompt.as_deref(), Some("hello"));
    }

    #[test]
    fn resume_forms() {
        let Ok(Parsed::Run(o)) = parse_args(args(&["resume", "--last"])) else {
            panic!()
        };
        assert!(o.resume_last);
        let Ok(Parsed::Run(o)) = parse_args(args(&["resume", "abc", "go"])) else {
            panic!()
        };
        assert_eq!(o.resume.as_deref(), Some("abc"));
        assert_eq!(o.prompt.as_deref(), Some("go"));
        let Ok(Parsed::Run(o)) = parse_args(args(&["resume"])) else {
            panic!()
        };
        assert!(o.resume_picker);
    }

    #[test]
    fn version_help_and_errors() {
        assert!(
            matches!(parse_args(args(&["--version"])), Ok(Parsed::Exit(s)) if s.starts_with("wizard-ui-codex "))
        );
        assert!(matches!(parse_args(args(&["-h"])), Ok(Parsed::Exit(_))));
        assert!(parse_args(args(&["--bogus"])).is_err());
        assert!(
            parse_args(args(&["--oss"]))
                .unwrap_err()
                .contains("not supported by wizard")
        );
        assert!(parse_args(args(&["-m"])).is_err());
        assert!(parse_args(args(&["-s", "bad"])).is_err());
    }
}
