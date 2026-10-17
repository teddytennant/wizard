//! `claudew`: Claude Code's terminal UI on the wizard backend.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use agent_core::Request;
use anyhow::{Context, Result};
use claudew::app::{self, App, AppOpts, BackendKind, Msg, CLAUDE_VERSION, VERSION};
use tokio::sync::mpsc::unbounded_channel;
use tuikit::term::{TermGuard, TermOptions};

const HELP: &str = "Wizard (claude look)

Usage: wizard-ui-claude [OPTIONS] [PROMPT]
       claudew [OPTIONS] [PROMPT]

Arguments:
  [PROMPT]                       Initial prompt for the interactive session

Options:
  -c, --continue                 Continue the most recent session for the current directory
      --cwd <CWD>                Working directory
      --model <MODEL>            Model id or name, as `/model` lists them
      --effort <LEVEL>           Reasoning effort
  -r, --resume [<SESSION_ID>]    Resume a session by id, or the most recent if omitted
  -p, --print <PROMPT>           Single turn: print the response to stdout and exit
      --permission-mode <MODE>   `plan` starts in plan mode; wizard has no other permission modes
      --theme <dark|light|auto>  Colours; auto reads COLORFGBG, and so does no flag
  -h, --help                     Print help
  -v, --version                  Print version

Environment:
  CLAUDEW_BACKEND=mock           Scripted backend, no model calls
  CLAUDEW_BACKEND=replay         Play back the events in the file CLAUDEW_REPLAY names
  CLAUDEW_WIZARD_BIN             Path to the wizard binary
  CLAUDEW_THEME=light|dark       Colours, over what COLORFGBG says
";

enum Parsed {
    Run(Box<AppOpts>, Option<String>),
    Exit(String),
}

fn version() -> String {
    format!("wizard-ui-claude {VERSION} (claude look {CLAUDE_VERSION})")
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Parsed, String> {
    let mut o = AppOpts::default();
    let mut single: Option<String> = None;
    let mut cwd: Option<PathBuf> = None;
    let mut it = args.into_iter().peekable();
    let mut prompt: Vec<String> = Vec::new();
    let mut rest = false;
    o.light = light_terminal();
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
            "-v" | "--version" => return Ok(Parsed::Exit(version())),
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
            "--effort" | "--reasoning-effort" => o.effort = Some(value("--effort")?),
            "-p" | "--print" | "--single" => single = Some(value("--print")?),
            "--permission-mode" => match value("--permission-mode")?.as_str() {
                "plan" => o.plan = true,
                "default" | "acceptEdits" | "dontAsk" | "bypassPermissions" | "auto" => {}
                other => return Err(format!("unknown permission mode {other}")),
            },
            "--theme" => match value("--theme")?.as_str() {
                "auto" => o.light = light_terminal(),
                "light" => o.light = true,
                "dark" => o.light = false,
                other => return Err(format!("unknown theme {other}")),
            },
            "--" => rest = true,
            f => return Err(format!("unknown option {f}\n\n{HELP}")),
        }
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
    o.backend = match std::env::var("CLAUDEW_BACKEND").as_deref() {
        Ok("mock") => BackendKind::Mock,
        Ok("replay") => match std::env::var_os("CLAUDEW_REPLAY") {
            Some(p) => BackendKind::Replay(PathBuf::from(p)),
            None => return Err("CLAUDEW_BACKEND=replay needs CLAUDEW_REPLAY=<file>".into()),
        },
        _ => BackendKind::Wizard,
    };
    Ok(Parsed::Run(Box::new(o), single))
}

/// A light background, as the terminal reports it in `COLORFGBG` (`15;0` is light on dark,
/// `0;15` dark on light), or as `CLAUDEW_THEME` says.
fn light_terminal() -> bool {
    match std::env::var("CLAUDEW_THEME").as_deref() {
        Ok("light") => return true,
        Ok("dark") => return false,
        _ => {}
    }
    std::env::var("COLORFGBG")
        .ok()
        .and_then(|v| v.rsplit(';').next().and_then(|b| b.parse::<u8>().ok()))
        .is_some_and(|bg| bg == 7 || bg >= 9)
}

/// `-p`: send the prompt, print what the model said, exit. Tool calls run inside wizard.
async fn single_mode(opts: AppOpts, prompt: String) -> Result<()> {
    use agent_core::Event;
    let (msg_tx, mut msg_rx) = unbounded_channel::<Msg>();
    let (tx, fwd) = app::spawn_backend(&opts, msg_tx)?;
    let mut sent = false;
    let mut out = String::new();
    while let Some(msg) = msg_rx.recv().await {
        let Msg::Backend(ev) = msg else { continue };
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
    std::thread::spawn(claudew::ui::diff::warm);
    let (msg_tx, msg_rx) = unbounded_channel::<Msg>();
    let (tx, fwd) = app::spawn_backend(&opts, msg_tx.clone())?;
    let wizard = opts.backend == BackendKind::Wizard;
    let mut app = App::new(opts, tx, msg_tx.clone());
    // the header names wizard's version from the first frame, before the session answers
    if wizard && app.cfg.backend.is_empty() {
        if let Some(v) = wizard_version() {
            app.cfg.backend = format!("wizard {v}");
        }
    }
    let cwd = app.opts.cwd.clone();
    tokio::task::spawn_blocking(move || {
        let _ = msg_tx.send(Msg::Files(app::list_files(&cwd)));
    });

    let mut guard = TermGuard::enter(TermOptions {
        focus_events: true,
        ..TermOptions::default()
    })
    .context("could not set up the terminal")?;
    // the cursor is never asked for once the input reader owns stdin
    let mut terminal = ratatui::Terminal::new(claudew::term::Quiet::new(
        ratatui::backend::CrosstermBackend::new(std::io::stdout()),
        false,
    ))?;
    let result = app::run(&mut app, &mut terminal, &mut guard, msg_rx).await;

    // Let wizard clean up its session before the process goes away.
    let _ = app.tx.send(Request::Shutdown);
    let _ = tokio::time::timeout(Duration::from_secs(3), fwd).await;
    drop(terminal);
    guard.restore();
    print!("{}", app.epilogue());
    result
}

/// `wizard --version`'s number, or nothing if it does not answer within a second.
fn wizard_version() -> Option<String> {
    let bin = std::env::var_os("OPENW_WIZARD_BIN")
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "wizard".into());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let out = std::process::Command::new(bin)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output();
        let _ = tx.send(out);
    });
    let out = rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .ok()?
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let v = text.split_whitespace().last()?.to_string();
    v.chars().next()?.is_ascii_digit().then_some(v)
}

fn main() -> ExitCode {
    // the shared wizard backend reads OPENW_WIZARD_BIN
    if let Some(bin) = std::env::var_os("CLAUDEW_WIZARD_BIN") {
        std::env::set_var("OPENW_WIZARD_BIN", bin);
    }
    let (opts, single) = match parse_args(std::env::args().skip(1)) {
        Ok(Parsed::Run(o, s)) => (*o, s),
        Ok(Parsed::Exit(text)) => {
            println!("{text}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("claudew: {e}");
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
            eprintln!("claudew: {e}");
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
            eprintln!("claudew: {e:#}");
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
            "-c", "--model", "a/b", "--effort", "high", "fix", "the", "bug",
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
            matches!(parse_args(args(&["--version"])), Ok(Parsed::Exit(s)) if s == format!("wizard-ui-claude {VERSION} (claude look {CLAUDE_VERSION})"))
        );
        assert!(matches!(parse_args(args(&["--help"])), Ok(Parsed::Exit(_))));
        assert!(parse_args(args(&["--bogus"])).is_err());
        assert!(parse_args(args(&["--model"])).is_err());
        assert!(parse_args(args(&["--theme", "sepia"])).is_err());
    }
}
