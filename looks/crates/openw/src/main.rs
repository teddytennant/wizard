// OWNER: shared (arguments, runtime, terminal guard)
//! `openw`: opencode's terminal UI on the wizard backend.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use agent_core::Request;
use anyhow::{Context, Result};
use openw::app::{self, App, AppOpts, Msg, VERSION};
use tokio::sync::mpsc::unbounded_channel;
use tuikit::term::{TermGuard, TermOptions};

const HELP: &str = "Wizard (opencode look)

Usage: wizard-ui-opencode [options] [project]
       openw [options] [project]

Options:
  -c, --continue          continue the latest session in this directory
  -s, --session <id>      open a session by id
  -m, --model <id>        model to use, e.g. xai-oauth/grok-4.6
      --prompt <text>     send this prompt once the backend is ready
      --cwd <dir>         working directory (default: current)
      --theme <name>      theme name (default: opencode)
  -v, --version           print the version
  -h, --help              print this help

Environment:
  OPENW_BACKEND=mock      scripted backend, no model calls
  OPENW_WIZARD_BIN        path to the wizard binary
  OPENW_STATE_DIR         where history and settings are kept
";

enum Parsed {
    Run(Box<AppOpts>),
    Exit(String),
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Parsed, String> {
    let mut o = AppOpts::default();
    let mut cwd: Option<PathBuf> = None;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match a.as_str() {
            "-h" | "--help" => return Ok(Parsed::Exit(HELP.to_string())),
            "-v" | "--version" => return Ok(Parsed::Exit(format!("openw {VERSION}"))),
            "-c" | "--continue" => o.continue_latest = true,
            "-s" | "--session" => o.resume = Some(value("--session")?),
            "-m" | "--model" => o.model = Some(value("--model")?),
            "--prompt" => o.prompt = Some(value("--prompt")?),
            "--cwd" => cwd = Some(PathBuf::from(value("--cwd")?)),
            "--theme" => {
                let name = value("--theme")?;
                if tuikit::Theme::builtin_mode(&name, tuikit::Mode::Dark).is_none() {
                    return Err(format!("unknown theme {name}"));
                }
                o.theme = Some(name);
            }
            s if s.starts_with('-') => return Err(format!("unknown option {s}\n\n{HELP}")),
            s => cwd = Some(PathBuf::from(s)),
        }
    }
    let cwd = match cwd {
        Some(c) => std::path::absolute(&c).map_err(|e| format!("bad directory: {e}"))?,
        None => std::env::current_dir().map_err(|e| e.to_string())?,
    };
    if !cwd.is_dir() {
        return Err(format!("not a directory: {}", cwd.display()));
    }
    o.cwd = cwd;
    o.mock = std::env::var("OPENW_BACKEND").is_ok_and(|v| v == "mock");
    o.state_dir = match std::env::var_os("OPENW_STATE_DIR") {
        Some(d) if !d.is_empty() => Some(PathBuf::from(d)),
        _ if o.mock => None,
        _ => std::env::var_os("XDG_STATE_HOME")
            .filter(|s| !s.is_empty())
            .map(|s| PathBuf::from(s).join("openw"))
            .or_else(|| {
                std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state/openw"))
            }),
    };
    o.config_dir = match std::env::var_os("OPENW_CONFIG_DIR") {
        Some(d) if !d.is_empty() => Some(PathBuf::from(d)),
        _ if o.mock => None,
        _ => openw::ui::dialogs::prefs::default_dir(),
    };
    o.wizard_dir = match std::env::var_os("OPENW_WIZARD_DIR") {
        Some(d) if !d.is_empty() => Some(PathBuf::from(d)),
        _ if o.mock => None,
        _ => std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".wizard")),
    };
    o.seed = std::env::var("OPENW_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos() as u64 / 7)
        });
    Ok(Parsed::Run(Box::new(o)))
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
    app.branch = app::git_branch(&app.opts.cwd);

    let cwd = app.opts.cwd.clone();
    let files_tx = msg_tx.clone();
    tokio::task::spawn_blocking(move || {
        let _ = files_tx.send(Msg::Files(app::list_files(&cwd)));
    });

    let mut guard = TermGuard::enter(TermOptions {
        mouse: true,
        bracketed_paste: true,
        focus_events: true,
        ..Default::default()
    })
    .context("could not set up the terminal")?;
    if let Some(rgb) = tuikit::theme::rgb_of(app.theme.text) {
        let _ = guard.set_cursor_color(rgb);
    }
    let mut terminal = guard.terminal()?;
    let result = app::run(&mut app, &mut terminal, &mut guard, msg_rx).await;

    // A `!cmd` still running goes with us, and so does what it started.
    app.kill_shell();
    // Give the terminal back first: wizard may take a few seconds to clean up its session, and
    // a frozen alt screen looks like a hang.
    let _ = app.tx.send(Request::Shutdown);
    drop(terminal);
    guard.restore();
    if let Some(e) = app.epilogue() {
        print!("{e}");
    }
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
            eprintln!("openw: {e}");
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
            eprintln!("openw: {e}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(real_main(opts)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("openw: {e:#}");
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
        let Ok(Parsed::Run(o)) = parse_args(args(&[
            "-c", "--model", "a/b", "--theme", "nord", "--prompt", "hi",
        ])) else {
            panic!("did not parse")
        };
        assert!(o.continue_latest);
        assert_eq!(o.model.as_deref(), Some("a/b"));
        assert_eq!(o.theme.as_deref(), Some("nord"));
        assert_eq!(o.prompt.as_deref(), Some("hi"));
        let Ok(Parsed::Run(o)) = parse_args(args(&["-s", "abc"])) else {
            panic!()
        };
        assert_eq!(o.resume.as_deref(), Some("abc"));
    }

    #[test]
    fn version_help_and_errors() {
        assert!(
            matches!(parse_args(args(&["--version"])), Ok(Parsed::Exit(s)) if s.starts_with("openw "))
        );
        assert!(matches!(parse_args(args(&["--help"])), Ok(Parsed::Exit(_))));
        assert!(parse_args(args(&["--bogus"])).is_err());
        assert!(parse_args(args(&["--model"])).is_err());
        assert!(parse_args(args(&["--cwd", "/definitely/not/here"])).is_err());
        // a theme that does not exist is an error, not a silent fall back to the default
        assert!(matches!(
            parse_args(args(&["--theme", "nosuchtheme"])),
            Err(e) if e.contains("unknown theme nosuchtheme")
        ));
    }
}
