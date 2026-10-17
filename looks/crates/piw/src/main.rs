// OWNER: shared (arguments, runtime, terminal guard)
//! `piw`: Pi's terminal UI on the wizard backend.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use agent_core::Request;
use anyhow::{Context, Result};
use piw::app::{self, App, AppOpts, Msg, VERSION};
use piw::theme::COLOR_QUERY_TIMEOUT;
use tokio::sync::mpsc::unbounded_channel;
use tuikit::input::Input;
use tuikit::term::{TermGuard, TermOptions};

const HELP: &str = "Wizard (pi look)

Usage:
  wizard-ui-pi [options] [messages...]
  piw [options] [messages...]

Options:
  --continue, -c          Continue the latest session in this directory
  --resume, -r            Select a session to resume
  --session <id>          Open a session by id
  --model <pattern>       Model id or name, e.g. xai-oauth/grok-4.6
  --thinking <level>      Set the thinking level (the backend's own list)
  --name, -n <name>       Set the session display name
  --print, -p             Non-interactive: send the messages, print the answer, exit
  --use-theme <name>      Theme for this run: dark, light or system
  --theme <path>          Load a Pi theme file and use it
  --verbose               Accepted for Pi compatibility
  --                      End option parsing; treat the rest as messages
  --help, -h              Show this help
  --version, -v           Show the version

Environment:
  PIW_BACKEND=mock        Scripted backend, no model calls
  PIW_STATE_DIR           Where prompt history is kept
  PIW_CONFIG_DIR          Where settings.json is kept
  PIW_WIZARD_BIN          Path to the wizard binary
";

enum Parsed {
    Run(Box<AppOpts>, bool),
    Exit(String),
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Parsed, String> {
    let mut o = AppOpts::default();
    let mut print = false;
    let mut it = args.into_iter();
    let mut rest_is_messages = false;
    while let Some(a) = it.next() {
        if rest_is_messages || !a.starts_with('-') || a == "-" {
            o.prompts.push(a);
            continue;
        }
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match a.as_str() {
            "-h" | "--help" => return Ok(Parsed::Exit(HELP.to_string())),
            "-v" | "--version" => return Ok(Parsed::Exit(format!("piw {VERSION}"))),
            "-c" | "--continue" => o.continue_latest = true,
            "-r" | "--resume" => o.pick_session = true,
            "--session" => o.resume = Some(value("--session")?),
            "--model" => o.model = Some(value("--model")?),
            "--thinking" => o.thinking = Some(value("--thinking")?),
            "-n" | "--name" => o.name = Some(value("--name")?),
            "-p" | "--print" => print = true,
            "--use-theme" | "--theme" => o.theme = Some(value(&a)?),
            "--verbose" => o.verbose = true,
            "--" => rest_is_messages = true,
            s => return Err(format!("unknown option {s}\n\n{HELP}")),
        }
    }
    if let Some(t) = &o.theme {
        app::check_theme(t)?;
    }
    o.cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    o.mock = std::env::var("PIW_BACKEND").is_ok_and(|v| v == "mock");
    o.guard_exe = std::env::current_exe().ok();
    o.state_dir = match std::env::var_os("PIW_STATE_DIR") {
        Some(d) if !d.is_empty() => Some(PathBuf::from(d)),
        _ if o.mock => None,
        _ => std::env::var_os("XDG_STATE_HOME")
            .filter(|s| !s.is_empty())
            .map(|s| PathBuf::from(s).join("piw"))
            .or_else(|| {
                std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state/piw"))
            }),
    };
    o.config_dir = match std::env::var_os("PIW_CONFIG_DIR") {
        Some(d) if !d.is_empty() => Some(PathBuf::from(d)),
        _ if o.mock => None,
        _ => piw::settings::default_dir(),
    };
    Ok(Parsed::Run(Box::new(o), print))
}

/// The answer goes to the terminal as the model wrote it only when it is piped; on a terminal it
/// goes through `plain_text`, because a model can write `ESC ] 52 ; c ; ...` as easily as words.
fn print_answer(out: &str) {
    use std::io::IsTerminal;
    if std::io::stdout().is_terminal() {
        println!("{}", tuikit::width::plain_text(out.trim_end()));
    } else {
        println!("{}", out.trim_end());
    }
}

/// `-p`: send each message, print what the model said, exit. Tool calls run inside wizard.
async fn print_mode(opts: AppOpts) -> Result<()> {
    use agent_core::Event;
    let (msg_tx, mut msg_rx) = unbounded_channel::<Msg>();
    let (tx, fwd) = app::spawn_backend(opts.cwd.clone(), opts.resume.clone(), opts.mock, msg_tx)?;
    let mut prompts = opts.prompts.clone().into_iter();
    let mut out = String::new();
    let mut ready = false;
    while let Some(Msg::Backend(ev)) = msg_rx.recv().await {
        match ev {
            Event::Ready { .. } if !ready => {
                ready = true;
                if let Some(m) = &opts.model {
                    let _ = tx.send(Request::SetModel(m.clone()));
                }
                match prompts.next() {
                    Some(p) => {
                        let _ = tx.send(Request::Prompt(p));
                    }
                    None => break,
                }
            }
            Event::TextDelta(t) => out.push_str(&t),
            Event::Notice { text, .. } if !text.is_empty() => {
                eprintln!("{}", tuikit::width::plain_text(&text))
            }
            Event::TurnEnd(_) => match prompts.next() {
                Some(p) => {
                    out.push_str("\n\n");
                    let _ = tx.send(Request::Prompt(p));
                }
                None => break,
            },
            Event::Fatal(e) => {
                // what the model did say before the backend died is still the answer so far
                print_answer(&out);
                let _ = tx.send(Request::Shutdown);
                anyhow::bail!(tuikit::width::plain_text(&e).into_owned());
            }
            _ => {}
        }
    }
    print_answer(&out);
    let _ = tx.send(Request::Shutdown);
    let _ = tokio::time::timeout(Duration::from_secs(3), fwd).await;
    Ok(())
}

async fn real_main(opts: AppOpts) -> Result<()> {
    let (msg_tx, msg_rx) = unbounded_channel::<Msg>();
    let (tx, fwd) = app::spawn_backend(
        opts.cwd.clone(),
        opts.resume.clone(),
        opts.mock,
        msg_tx.clone(),
    )?;
    let mut app = App::new(opts, tx, msg_tx.clone());
    app.fwd = Some(fwd);
    app.branch = app::git_branch(&app.opts.cwd);
    let cwd = app.opts.cwd.clone();
    let files_tx = msg_tx.clone();
    tokio::task::spawn_blocking(move || {
        let _ = files_tx.send(Msg::Files(app::list_files(&cwd)));
    });

    tuikit::depth::set(tuikit::depth::from_env());
    let mut guard = TermGuard::enter(TermOptions {
        mouse: true,
        bracketed_paste: true,
        focus_events: true,
        hide_cursor: true,
        ..Default::default()
    })
    .context("could not set up the terminal")?;
    let mut terminal = guard.terminal()?;
    // One reader for keys and for the terminal's answers, so a late colour reply never lands in
    // the editor as typed characters.
    let (input, mut replies) =
        Input::spawn_with_replies(Vec::new()).context("could not read the terminal")?;
    // Pi builds its theme from what the terminal reports, waiting up to 100 ms for the batch
    // (a DA1 reply ends it early). A terminal that answers late still gets its theme, from the
    // forwarder below.
    app.start_color_query();
    app::query_colors();
    let deadline = tokio::time::Instant::now() + COLOR_QUERY_TIMEOUT;
    loop {
        match tokio::time::timeout_at(deadline, replies.recv()).await {
            Ok(Some(r)) => {
                if app.on_term_reply(r) {
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => {
                app.color_query_timed_out();
                break;
            }
        }
    }
    if app.theme_follows_terminal() {
        let _ = guard.enable_scheme_notifications();
    }
    let reply_tx = msg_tx.clone();
    tokio::spawn(async move {
        while let Some(r) = replies.recv().await {
            if reply_tx.send(Msg::TermReply(r)).is_err() {
                break;
            }
        }
    });
    let result = app::run(&mut app, &mut terminal, &mut guard, msg_rx, input).await;

    // Give the terminal back first: wizard may need seconds to clean up its session, and a frozen
    // alternate screen that eats keys for that long reads as a hang.
    app.kill_bash();
    drop(terminal);
    guard.restore();
    let w = crossterm::terminal::size().map_or(app.size.0, |s| s.0);
    print!("{}", app.epilogue(w));
    let _ = std::io::Write::flush(&mut std::io::stdout());
    // Let wizard clean up its session before the process goes away.
    let _ = app.tx.send(Request::Shutdown);
    if let Some(fwd) = app.fwd.take() {
        let _ = tokio::time::timeout(Duration::from_secs(3), fwd).await;
    }
    if let Some(e) = app.startup_error.take() {
        anyhow::bail!(e);
    }
    result
}

fn main() -> ExitCode {
    // `piw --guard sh -c <line>` owns the process tree of a `!cmd`; it never touches the terminal
    if std::env::args_os().nth(1).is_some_and(|a| a == "--guard") {
        let code = backend_claude::guard::run(std::env::args_os().skip(2).collect());
        return ExitCode::from(code.clamp(0, 255) as u8);
    }
    // the shared wizard backend reads OPENW_WIZARD_BIN
    if let Some(bin) = std::env::var_os("PIW_WIZARD_BIN") {
        std::env::set_var("OPENW_WIZARD_BIN", bin);
    }
    let (opts, print) = match parse_args(std::env::args().skip(1)) {
        Ok(Parsed::Run(o, p)) => (*o, p),
        Ok(Parsed::Exit(text)) => {
            println!("{text}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("piw: {e}");
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
            eprintln!("piw: {e}");
            return ExitCode::FAILURE;
        }
    };
    let res = if print {
        rt.block_on(print_mode(opts))
    } else {
        rt.block_on(real_main(opts))
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("piw: {e:#}");
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
        let Ok(Parsed::Run(o, print)) = parse_args(args(&[
            "-c",
            "--model",
            "a/b",
            "--use-theme",
            "light",
            "--thinking",
            "high",
            "hello",
            "world",
        ])) else {
            panic!("did not parse")
        };
        assert!(o.continue_latest && !print);
        assert_eq!(o.model.as_deref(), Some("a/b"));
        assert_eq!(o.theme.as_deref(), Some("light"));
        assert_eq!(o.thinking.as_deref(), Some("high"));
        assert_eq!(o.prompts, ["hello", "world"]);
        let Ok(Parsed::Run(o, print)) = parse_args(args(&["-p", "--", "- bullet"])) else {
            panic!()
        };
        assert!(print);
        assert_eq!(o.prompts, ["- bullet"]);
        let Ok(Parsed::Run(o, _)) = parse_args(args(&["-r", "--session", "abc"])) else {
            panic!()
        };
        assert!(o.pick_session);
        assert_eq!(o.resume.as_deref(), Some("abc"));
    }

    #[test]
    fn version_help_and_errors() {
        assert!(
            matches!(parse_args(args(&["--version"])), Ok(Parsed::Exit(s)) if s.starts_with("piw "))
        );
        assert!(matches!(parse_args(args(&["--help"])), Ok(Parsed::Exit(_))));
        assert!(parse_args(args(&["--bogus"])).is_err());
        let e = parse_args(args(&["--use-theme", "nosuchtheme"]))
            .err()
            .unwrap();
        assert!(e.contains("unknown theme"), "{e}");
        assert!(parse_args(args(&["--use-theme", "system"])).is_ok());
        assert!(parse_args(args(&["--model"])).is_err());
    }
}
