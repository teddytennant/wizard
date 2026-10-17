//! `jcodew`: jcode's terminal UI on the wizard backend.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use agent_core::Request;
use anyhow::{Context, Result};
use jcodew::app::{self, App, AppOpts, Msg, VERSION};
use tokio::sync::mpsc::unbounded_channel;
use tuikit::term::{TermGuard, TermOptions};

const HELP: &str = "Wizard (jcode look)

Usage: wizard-ui-jcode [OPTIONS] [PROMPT]
       jcodew [OPTIONS] [PROMPT]

Arguments:
  [PROMPT]                       Initial prompt for the interactive session

Options:
  -c, --continue                 Continue the most recent session for the current directory
  -C, --cwd <CWD>                Working directory
  -m, --model <MODEL>            Model id or name, as `/model` lists them
      --reasoning-effort <LEVEL> Reasoning effort (alias: --effort)
      --resume [<SESSION_ID>]    Resume a session by id, or pick one if omitted
  -h, --help                     Print help
  -v, --version                  Print version

Not supported because wizard cannot back them: --provider, --provider-profile, --ssh,
--socket, --tools, --tool-profile and the subcommands.

Environment:
  JCODEW_BACKEND=mock            Scripted backend, no model calls
  JCODEW_STATE_DIR               Where prompt history is kept
  JCODEW_WIZARD_BIN              Path to the wizard binary
";

enum Parsed {
    Run(Box<AppOpts>),
    Exit(String),
}

/// Flags jcode has that a look on wizard cannot honour: refused rather than ignored.
const UNSUPPORTED: &[&str] = &[
    "-p",
    "--provider",
    "--provider-profile",
    "--ssh",
    "--socket",
    "--tools",
    "--tool-profile",
    "--disabled-tools",
    "--mcp-tools",
    "--remote-working-dir",
];

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Parsed, String> {
    let mut o = AppOpts::default();
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
            "-v" | "-V" | "--version" => {
                return Ok(Parsed::Exit(format!("wizard-ui-jcode {VERSION}")))
            }
            "-c" | "--continue" => o.continue_latest = true,
            "-r" | "--resume" => match inline {
                Some(v) => o.resume = Some(v),
                None => match it.peek() {
                    Some(n) if !n.starts_with('-') && n.len() >= 8 && !n.contains(' ') => {
                        o.resume = it.next();
                    }
                    _ => o.pick_session = true,
                },
            },
            "-C" | "--cwd" => cwd = Some(PathBuf::from(value("--cwd")?)),
            "-m" | "--model" => o.model = Some(value("--model")?),
            "--reasoning-effort" | "--effort" => o.effort = Some(value("--reasoning-effort")?),
            "--no-update" | "--no-selfdev" | "--quiet" => {}
            "--" => rest = true,
            f if UNSUPPORTED.contains(&f) => {
                return Err(format!(
                    "{f} is not supported by wizard, so jcodew has no way to honour it"
                ))
            }
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
    o.mock = std::env::var("JCODEW_BACKEND").is_ok_and(|v| v == "mock");
    o.state_dir = match std::env::var_os("JCODEW_STATE_DIR") {
        Some(d) if !d.is_empty() => Some(PathBuf::from(d)),
        _ if o.mock => None,
        _ => std::env::var_os("XDG_STATE_HOME")
            .filter(|s| !s.is_empty())
            .map(|s| PathBuf::from(s).join("jcodew"))
            .or_else(|| {
                std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state/jcodew"))
            }),
    };
    Ok(Parsed::Run(Box::new(o)))
}

async fn real_main(opts: AppOpts) -> Result<()> {
    std::thread::spawn(jcodew::ui::markdown::warm);
    jcodew::theme::set_truecolor(jcodew::theme::detect_truecolor());
    let (msg_tx, msg_rx) = unbounded_channel::<Msg>();
    let (tx, fwd) = app::spawn_backend(
        opts.cwd.clone(),
        opts.resume.clone(),
        opts.mock,
        msg_tx.clone(),
    )?;
    let mut app = App::new(opts, tx, msg_tx.clone());
    app.git.branch = app::git_info(&app.opts.cwd).branch;

    let mut guard =
        TermGuard::enter(TermOptions::default()).context("could not set up the terminal")?;
    let _ = guard.set_title("wizard");
    let mut terminal = guard.terminal()?;
    let result = app::run(&mut app, &mut terminal, &mut guard, msg_rx).await;

    let _ = app.tx.send(Request::Shutdown);
    let _ = tokio::time::timeout(Duration::from_secs(3), fwd).await;
    drop(terminal);
    guard.restore();
    eprint!("{}", app.epilogue());
    result
}

fn main() -> ExitCode {
    if let Some(bin) = std::env::var_os("JCODEW_WIZARD_BIN") {
        std::env::set_var("OPENW_WIZARD_BIN", bin);
    }
    let opts = match parse_args(std::env::args().skip(1)) {
        Ok(Parsed::Run(o)) => *o,
        Ok(Parsed::Exit(text)) => {
            println!("{text}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("jcodew: {e}");
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
            eprintln!("jcodew: {e}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(real_main(opts)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("jcodew: {e:#}");
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
        let Ok(Parsed::Run(o)) =
            parse_args(args(&["-c", "-m", "a/b", "--effort", "high", "fix", "it"]))
        else {
            panic!("did not parse")
        };
        assert!(o.continue_latest);
        assert_eq!(o.model.as_deref(), Some("a/b"));
        assert_eq!(o.effort.as_deref(), Some("high"));
        assert_eq!(o.prompts, ["fix it"]);
    }

    #[test]
    fn resume_takes_an_id_or_opens_the_picker() {
        let Ok(Parsed::Run(o)) = parse_args(args(&["--resume", "0123456789abcdef"])) else {
            panic!()
        };
        assert_eq!(o.resume.as_deref(), Some("0123456789abcdef"));
        let Ok(Parsed::Run(o)) = parse_args(args(&["--resume"])) else {
            panic!()
        };
        assert!(o.pick_session && o.resume.is_none());
    }

    #[test]
    fn version_help_and_refusals() {
        assert!(
            matches!(parse_args(args(&["--version"])), Ok(Parsed::Exit(s)) if s.starts_with("wizard-ui-jcode "))
        );
        assert!(matches!(parse_args(args(&["--help"])), Ok(Parsed::Exit(_))));
        assert!(parse_args(args(&["--provider", "x"])).is_err());
        assert!(parse_args(args(&["--bogus"])).is_err());
        assert!(parse_args(args(&["--model"])).is_err());
    }
}
