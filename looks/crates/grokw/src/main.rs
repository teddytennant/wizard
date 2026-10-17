// OWNER: shared (arguments, runtime, terminal guard)
//! `grokw`: Grok Build's terminal UI on the wizard backend.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use agent_core::Request;
use anyhow::{Context, Result};
use grokw::app::{self, App, AppOpts, Msg, GROK_VERSION, VERSION};
use grokw::term::TermGuard;
use tokio::sync::mpsc::unbounded_channel;

const HELP: &str = "Wizard (grok look)

Usage: wizard-ui-grok [OPTIONS] [PROMPT]
       grokw [OPTIONS] [PROMPT]

Arguments:
  [PROMPT]                       Initial prompt for the interactive session

Options:
  -c, --continue                 Continue the most recent session for the current directory
      --cwd <CWD>                Working directory
  -m, --model <MODEL>            Model id or name, as `/model` lists them
      --reasoning-effort <LEVEL> Reasoning effort (alias: --effort)
  -r, --resume [<SESSION_ID>]    Resume a session by id, or the most recent if omitted
  -p, --single <PROMPT>          Single turn: print the response to stdout and exit
      --permission-mode <MODE>   `plan` starts in plan mode; wizard has no other permission modes
      --always-approve           Accepted; wizard already runs every tool without asking
      --fullscreen               Accepted; grokw always uses the alternate screen
  -h, --help                     Print help
  -v, --version                  Print version

      --no-alt-screen            Draw in the normal screen, full height, and leave the last
                                 frame in the scrollback
      --minimal                  A few live rows at the bottom; finished blocks go to the
                                 terminal's own scrollback

Not supported because wizard cannot back them: --worktree, --session-id, --agent, --agents,
--allow, --deny, --tools, --sandbox and the subcommands.

Environment:
  GROKW_BACKEND=mock             Scripted backend, no model calls
  GROKW_STATE_DIR                Where prompt history and the tip cursor are kept
  GROKW_WIZARD_BIN               Path to the wizard binary
";

enum Parsed {
    Run(Box<AppOpts>, Option<String>),
    Exit(String),
}

/// Flags Grok has that a clone on wizard cannot honour: refused rather than ignored.
const UNSUPPORTED: &[&str] = &[
    "-w",
    "--worktree",
    "--worktree-ref",
    "--ref",
    "-s",
    "--session-id",
    "--agent",
    "--agents",
    "--allow",
    "--deny",
    "--tools",
    "--disallowed-tools",
    "--sandbox",
    "--fork-session",
    "--restore-code",
    "--json-schema",
    "--output-format",
    "--prompt-file",
    "--prompt-json",
    "--system-prompt-override",
    "--rules",
];

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Parsed, String> {
    let mut o = AppOpts::default();
    let mut single: Option<String> = None;
    let mut cwd: Option<PathBuf> = None;
    let mut it = args.into_iter().peekable();
    let mut prompt: Vec<String> = Vec::new();
    let mut rest = false;
    while let Some(a) = it.next() {
        if rest || !a.starts_with('-') || a == "-" {
            prompt.push(a);
            continue;
        }
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (a.clone(), None),
        };
        let mut value = |name: &str| -> Result<String, String> {
            match inline.clone() {
                Some(v) => Ok(v),
                None => it.next().ok_or_else(|| format!("{name} needs a value")),
            }
        };
        match flag.as_str() {
            "-h" | "--help" => return Ok(Parsed::Exit(HELP.to_string())),
            "-v" | "--version" => {
                return Ok(Parsed::Exit(format!(
                    "wizard-ui-grok {VERSION} (grok look {GROK_VERSION})"
                )))
            }
            "-c" | "--continue" => o.continue_latest = true,
            "-r" | "--resume" => match inline {
                Some(v) => o.resume = Some(v),
                None => match it.peek() {
                    Some(n) if !n.starts_with('-') && n.len() >= 8 && !n.contains(' ') => {
                        o.resume = it.next();
                    }
                    _ => o.continue_latest = true,
                },
            },
            "--cwd" => cwd = Some(PathBuf::from(value("--cwd")?)),
            "-m" | "--model" => o.model = Some(value("--model")?),
            "--reasoning-effort" | "--effort" => o.effort = Some(value("--reasoning-effort")?),
            "-p" | "--single" => single = Some(value("--single")?),
            "--permission-mode" => match value("--permission-mode")?.as_str() {
                "plan" => o.plan = true,
                "default" | "acceptEdits" | "dontAsk" | "bypassPermissions" | "auto" => {}
                other => return Err(format!("unknown permission mode {other}")),
            },
            "--no-alt-screen" => o.screen = grokw::term::ScreenMode::Inline,
            "--minimal" => o.screen = grokw::term::ScreenMode::Minimal,
            "--always-approve"
            | "--fullscreen"
            | "--oauth"
            | "--debug"
            | "--verbatim"
            | "--no-plan"
            | "--no-subagents"
            | "--disable-web-search" => {}
            "--" => rest = true,
            f if UNSUPPORTED.contains(&f) => {
                return Err(format!(
                    "{f} is not supported by wizard, so grokw has no way to honour it"
                ))
            }
            f => return Err(format!("unknown option {f}\n\n{HELP}")),
        }
    }
    if matches!(prompt.first().map(String::as_str), Some("version" | "v")) && prompt.len() == 1 {
        return Ok(Parsed::Exit(format!(
            "wizard-ui-grok {VERSION} (grok look {GROK_VERSION})"
        )));
    }
    o.prompts = if prompt.is_empty() {
        Vec::new()
    } else {
        vec![prompt.join(" ")]
    };
    let cwd = match cwd {
        Some(c) => std::path::absolute(&c).map_err(|e| format!("bad directory: {e}"))?,
        None => std::env::current_dir().map_err(|e| e.to_string())?,
    };
    if !cwd.is_dir() {
        return Err(format!("not a directory: {}", cwd.display()));
    }
    o.cwd = cwd;
    o.mock = std::env::var("GROKW_BACKEND").is_ok_and(|v| v == "mock");
    o.state_dir = match std::env::var_os("GROKW_STATE_DIR") {
        Some(d) if !d.is_empty() => Some(PathBuf::from(d)),
        _ if o.mock => None,
        _ => std::env::var_os("XDG_STATE_HOME")
            .filter(|s| !s.is_empty())
            .map(|s| PathBuf::from(s).join("grokw"))
            .or_else(|| {
                std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state/grokw"))
            }),
    };
    Ok(Parsed::Run(Box::new(o), single))
}

/// `-p`: send the prompt, print what the model said, exit. Tool calls run inside wizard.
async fn single_mode(opts: AppOpts, prompt: String) -> Result<()> {
    use agent_core::Event;
    let (msg_tx, mut msg_rx) = unbounded_channel::<Msg>();
    let (tx, fwd) = app::spawn_backend(opts.cwd.clone(), opts.resume.clone(), opts.mock, msg_tx)?;
    let mut sent = false;
    let mut out = String::new();
    while let Some(Msg::Backend(ev)) = msg_rx.recv().await {
        match ev {
            Event::Ready { .. } if !sent => {
                sent = true;
                if let Some(m) = &opts.model {
                    let _ = tx.send(Request::SetModel(m.clone()));
                }
                if let Some(e) = &opts.effort {
                    let _ = tx.send(Request::SetEffort(e.clone()));
                }
                let _ = tx.send(Request::Prompt(prompt.clone()));
            }
            Event::TextDelta(t) => out.push_str(&t),
            Event::Notice { text, .. } if !text.is_empty() => eprintln!("{text}"),
            Event::TurnEnd(_) => break,
            Event::Fatal(e) => {
                let _ = tx.send(Request::Shutdown);
                anyhow::bail!(e);
            }
            _ => {}
        }
    }
    println!("{}", out.trim_end());
    let _ = tx.send(Request::Shutdown);
    let _ = tokio::time::timeout(Duration::from_secs(3), fwd).await;
    Ok(())
}

async fn real_main(opts: AppOpts) -> Result<()> {
    std::thread::spawn(grokw::ui::syntax::warm);
    let (msg_tx, msg_rx) = unbounded_channel::<Msg>();
    let (tx, fwd) = app::spawn_backend(
        opts.cwd.clone(),
        opts.resume.clone(),
        opts.mock,
        msg_tx.clone(),
    )?;
    let mut app = App::new(opts, tx, msg_tx.clone());
    if app.opts.screen == grokw::term::ScreenMode::Minimal {
        // no welcome screen, and the colours are the terminal's own
        app.screen = app::Screen::Session;
        app.theme = grokw::theme::Theme::terminal();
    }
    app.home.warnings = grokw::ui::welcome::probe_warnings();
    app.branch = app::git_branch(&app.opts.cwd);
    let cwd = app.opts.cwd.clone();
    let files_tx = msg_tx.clone();
    tokio::task::spawn_blocking(move || {
        let _ = files_tx.send(Msg::Files(app::list_files(&cwd)));
    });

    let mut guard = TermGuard::enter(app.opts.screen).context("could not set up the terminal")?;
    let mut terminal = guard.terminal()?;
    let result = app::run(&mut app, &mut terminal, &mut guard, msg_rx).await;

    // Let wizard clean up its session before the process goes away.
    let _ = app.tx.send(Request::Shutdown);
    let _ = tokio::time::timeout(Duration::from_secs(3), fwd).await;
    drop(terminal);
    guard.restore();
    let w = crossterm::terminal::size().map_or(app.size.0, |s| s.0);
    print!("{}", app.epilogue(w));
    result
}

fn main() -> ExitCode {
    // the shared wizard backend reads OPENW_WIZARD_BIN
    if let Some(bin) = std::env::var_os("GROKW_WIZARD_BIN") {
        std::env::set_var("OPENW_WIZARD_BIN", bin);
    }
    let (opts, single) = match parse_args(std::env::args().skip(1)) {
        Ok(Parsed::Run(o, s)) => (*o, s),
        Ok(Parsed::Exit(text)) => {
            println!("{text}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("grokw: {e}");
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
            eprintln!("grokw: {e}");
            return ExitCode::FAILURE;
        }
    };
    let res = match single {
        Some(p) => rt.block_on(single_mode(opts, p)),
        None => rt.block_on(real_main(opts)),
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("grokw: {e:#}");
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
        let Ok(Parsed::Run(o, single)) = parse_args(args(&[
            "-c", "-m", "a/b", "--effort", "high", "fix", "the", "bug",
        ])) else {
            panic!("did not parse")
        };
        assert!(o.continue_latest && single.is_none());
        assert_eq!(o.model.as_deref(), Some("a/b"));
        assert_eq!(o.effort.as_deref(), Some("high"));
        assert_eq!(o.prompts, ["fix the bug"]);
        let Ok(Parsed::Run(o, single)) = parse_args(args(&["--permission-mode=plan", "-p", "hi"]))
        else {
            panic!()
        };
        assert!(o.plan);
        assert_eq!(single.as_deref(), Some("hi"));
    }

    #[test]
    fn screen_modes_parse() {
        use grokw::term::ScreenMode;
        let mode = |a: &[&str]| match parse_args(args(a)) {
            Ok(Parsed::Run(o, _)) => o.screen,
            _ => panic!("did not parse"),
        };
        assert_eq!(mode(&[]), ScreenMode::Fullscreen);
        assert_eq!(mode(&["--no-alt-screen"]), ScreenMode::Inline);
        assert_eq!(mode(&["--minimal"]), ScreenMode::Minimal);
    }

    #[test]
    fn resume_takes_an_id_or_means_the_latest() {
        let Ok(Parsed::Run(o, _)) = parse_args(args(&["-r", "0123456789abcdef"])) else {
            panic!()
        };
        assert_eq!(o.resume.as_deref(), Some("0123456789abcdef"));
        let Ok(Parsed::Run(o, _)) = parse_args(args(&["--resume"])) else {
            panic!()
        };
        assert!(o.continue_latest && o.resume.is_none());
    }

    #[test]
    fn version_help_and_refusals() {
        assert!(
            matches!(parse_args(args(&["--version"])), Ok(Parsed::Exit(s)) if s.starts_with("wizard-ui-grok "))
        );
        assert!(matches!(parse_args(args(&["--help"])), Ok(Parsed::Exit(_))));
        assert!(parse_args(args(&["--worktree"])).is_err());
        assert!(parse_args(args(&["--worktree=x"])).is_err());
        assert!(parse_args(args(&["--bogus"])).is_err());
        assert!(parse_args(args(&["--model"])).is_err());
    }
}
