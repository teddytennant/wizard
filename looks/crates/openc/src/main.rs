// OWNER: app
//! `openc`: a full-screen terminal frontend for Claude Code.
//!
//! The loop here does three things: feed terminal and backend events into `App`, draw when the
//! app is dirty (at most once per 16 ms), and sleep until the next input or the next animation
//! frame. There is no fixed tick, so an idle session wakes up for nothing.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use agent_core::{Backend, BackendHandle, Request};
use anyhow::{Context, Result};
use futures_util::FutureExt;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc::unbounded_channel;
use tuikit::input::{Input, InputControl};
use tuikit::term::{EventPump, TermGuard, TermOptions};

use openc::app::{App, External, Msg, Opts, ThemeChoice};
use openc::palette::{self, Depth};
use openc::term::{self, FrameWriter};
use openc::ui;

const HELP: &str = "\
openc: a full-screen terminal frontend for Claude Code

usage: openc [options]

  -c, --continue        continue the newest session of this directory
  -r, --resume <id>     resume a session by id
      --model <name>    model alias or id (haiku, sonnet, opus, ...)
      --mode <mode>     ask, edits, plan or bypass (default ask)
      --prompt <text>   send this as the first message
      --cwd <dir>       working directory
      --theme <name>    hearth, parchment or auto (default auto)
      --reset           write the terminal restore sequence and exit
      --version
      --help

environment: OPENC_BACKEND=mock uses a scripted backend with no model;
NO_COLOR, OPENC_ASCII=1, OPENC_NO_SPINNER=1 are honoured. OPENC_GLYPH_WIDTHS=codepoint
counts emoji sequences one code point at a time, for a terminal without grapheme clusters
(mode 2027) that draws a ZWJ family or a keycap at another width.
";

#[derive(Default)]
struct Args {
    cont: bool,
    resume: Option<String>,
    model: Option<String>,
    mode: Option<String>,
    prompt: Option<String>,
    cwd: Option<PathBuf>,
    theme: Option<String>,
    reset: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut val = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "-c" | "--continue" => a.cont = true,
            "-r" | "--resume" => a.resume = Some(val("--resume")?),
            "--model" => a.model = Some(val("--model")?),
            "--mode" => a.mode = Some(val("--mode")?),
            "--prompt" => a.prompt = Some(val("--prompt")?),
            "--cwd" => a.cwd = Some(PathBuf::from(val("--cwd")?)),
            "--theme" => a.theme = Some(val("--theme")?),
            "--reset" => a.reset = true,
            "--version" | "-V" => {
                println!("openc {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--help" | "-h" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?}; try --help")),
        }
    }
    Ok(a)
}

fn mode_name(w: &str) -> String {
    match w {
        "ask" => "default",
        "edits" => "acceptEdits",
        "bypass" => "bypassPermissions",
        o => o,
    }
    .to_string()
}

fn main() -> ExitCode {
    // `openc --guard <claude> <args>` is the process that owns claude's tool tree; it never
    // touches the terminal.
    if std::env::args_os().nth(1).is_some_and(|a| a == "--guard") {
        let code = backend_claude::guard::run(std::env::args_os().skip(2).collect());
        return ExitCode::from(code.clamp(0, 255) as u8);
    }
    if let Ok(exe) = std::env::current_exe() {
        backend_claude::set_guard_exe(exe);
    }
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("openc: {e}");
            return ExitCode::from(2);
        }
    };
    if args.reset {
        print!("{}", term::RESTORE);
        let _ = std::io::stdout().flush();
        return ExitCode::SUCCESS;
    }
    if !is_tty(0) || !is_tty(1) {
        eprintln!("openc: needs a terminal on stdin and stdout");
        return ExitCode::from(2);
    }
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("openc: {e}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(run(args)) {
        Ok(summary) => {
            if !summary.is_empty() {
                print!("{summary}");
                let _ = std::io::stdout().flush();
            }
            // Do not wait for backend tasks that are still winding down.
            rt.shutdown_timeout(Duration::from_millis(200));
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("openc: {e:#}");
            rt.shutdown_timeout(Duration::from_millis(200));
            ExitCode::FAILURE
        }
    }
}

fn spawn_backend(args: &Args, cwd: &std::path::Path) -> Result<BackendHandle> {
    if std::env::var("OPENC_BACKEND").is_ok_and(|v| v == "mock") {
        return agent_core::mock::MockBackend::spawn(cwd.to_path_buf(), None);
    }
    let resume = match (&args.resume, args.cont) {
        (Some(id), _) => Some(id.clone()),
        (None, true) => backend_claude::history::list_sessions(cwd, 1)
            .into_iter()
            .next()
            .map(|s| s.id),
        _ => None,
    };
    let mut o = backend_claude::ClaudeOptions::from_env(cwd.to_path_buf());
    if let Some(m) = &args.model {
        o.model = Some(m.clone());
    }
    if let Some(m) = &args.mode {
        o.mode = mode_name(m);
    } else if std::env::var_os("OPENC_CLAUDE_MODE").is_none() {
        o.mode = "default".into();
    }
    backend_claude::ClaudeBackend::spawn_with(o, resume)
}

fn is_tty(fd: i32) -> bool {
    extern "C" {
        fn isatty(fd: i32) -> i32;
    }
    unsafe { isatty(fd) == 1 }
}

fn ignore_sigquit() {
    extern "C" {
        fn signal(sig: i32, handler: usize) -> usize;
    }
    // SIGQUIT is 3 and SIG_IGN is 1 on Linux and macOS.
    unsafe {
        signal(3, 1);
    }
}

fn tmux_rgb() -> bool {
    std::process::Command::new("tmux")
        .args(["display-message", "-p", "#{client_termfeatures}"])
        .output()
        .ok()
        .is_some_and(|o| String::from_utf8_lossy(&o.stdout).contains("RGB"))
}

type Term = ratatui::Terminal<CrosstermBackend<FrameWriter>>;

async fn run(args: Args) -> Result<String> {
    let cwd = match &args.cwd {
        Some(c) => c
            .canonicalize()
            .with_context(|| format!("--cwd {}", c.display()))?,
        None => std::env::current_dir()?,
    };
    let theme = match &args.theme {
        Some(t) => ThemeChoice::parse(t)
            .with_context(|| format!("--theme {t}: expected hearth, parchment or auto"))?,
        None => ThemeChoice::Auto,
    };
    // The backend starts first so it warms up while the screen is set up.
    let handle = spawn_backend(&args, &cwd)?;
    let BackendHandle {
        tx,
        rx: mut backend_rx,
    } = handle;

    // One query batch, answered or not within the probe's timeout, before anything else reads
    // the terminal. Raw mode first, so replies are not echoed and arrive without a newline.
    let _ = crossterm::terminal::enable_raw_mode();
    let ssh = std::env::var_os("SSH_CONNECTION").is_some();
    let (caps, typed_ahead) =
        term::probe_keeping_input(Duration::from_millis(if ssh { 500 } else { 150 }));
    // The toolkit would otherwise ask the same question again and wait out its own timeout.
    tuikit::term::set_keyboard_enhancement_supported(caps.kitty_kbd);
    let mut guard = TermGuard::enter(TermOptions {
        alt_screen: true,
        mouse: true,
        bracketed_paste: true,
        focus_events: true,
        keyboard_enhancement: true,
        hide_cursor: false,
    })?;
    // Ctrl+\\ is a key here, not a core dump.
    ignore_sigquit();
    let depth = palette::upgrade_depth(palette::depth_from_env(tmux_rgb), &caps);
    let glyphs = palette::glyphs_from_env();
    let (w, h) = crossterm::terminal::size().unwrap_or((80, 24));
    if std::env::var("OPENC_GLYPH_WIDTHS").is_ok_and(|v| v == "codepoint") {
        tuikit::width::set_cluster_widths(false);
    }
    let _ = guard.set_title(&term::window_title(glyphs.sep, &cwd));
    if caps.grapheme {
        let _ = guard.enable_grapheme_clusters();
    }
    if caps.sync {
        guard.note_synchronized_output();
    }

    let (app_tx, app_rx) = unbounded_channel::<Msg>();
    let fwd = app_tx.clone();
    tokio::spawn(async move {
        while let Some(ev) = backend_rx.recv().await {
            // Whatever deltas queued up while the UI was busy go over as one message.
            let (ev, held) = openc::pump::merge_deltas(ev, || backend_rx.try_recv().ok());
            if fwd.send(Msg::Backend(ev)).is_err() {
                break;
            }
            if let Some(next) = held {
                if fwd.send(Msg::Backend(next)).is_err() {
                    break;
                }
            }
        }
    });

    let spinner = std::env::var_os("OPENC_NO_SPINNER").is_none();
    let opts = Opts {
        cwd: cwd.clone(),
        theme,
        prompt: args.prompt.clone(),
        mouse: true,
        spinner,
    };
    openc::ui::links::init(
        openc::ui::links::detect(
            &std::env::var("TERM").unwrap_or_default(),
            std::env::var("OPENC_LINKS").ok().as_deref(),
        ),
        &cwd.to_string_lossy(),
    );
    let mut app = App::new(
        opts,
        tx.clone(),
        app_tx,
        caps.clone(),
        depth,
        glyphs,
        (w, h),
    );
    app.spawn_git();
    app.spawn_files();

    let (writer, frames) = FrameWriter::new(caps.sync);
    let mut terminal: Term = ratatui::Terminal::new(CrosstermBackend::new(writer))?;
    // Our own reader, not crossterm's: it decodes shift+enter as tmux sends it, hears colour
    // scheme changes, and can be paused while $EDITOR has the terminal.
    let input = Input::spawn_with(typed_ahead).context("reading the terminal")?;
    let input_ctl = input.control();
    let mut pump = EventPump::<Msg>::spawn_raw(input, Duration::ZERO, app_rx);
    let _ = guard.enable_scheme_reports();
    // The first code block would otherwise pay for loading the grammars while you wait.
    std::thread::spawn(|| {
        let theme = palette::Palette::new(palette::Kind::Hearth, Depth::True).theme();
        let _ = tuikit::syntax::highlight_token("rust", "fn main() {}", &theme);
    });
    // A hook for tools/openc-checks.sh, which runs the debug build; a release build ignores it.
    let panic_test = cfg!(debug_assertions) && std::env::var_os("OPENC_TEST_PANIC").is_some();
    let stats = std::env::var_os("OPENC_STATS").map(PathBuf::from);
    let mut stats_file = stats.and_then(|p| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .ok()
    });

    loop {
        let now = Instant::now();
        if app.repaint {
            app.repaint = false;
            repaint(&mut terminal);
            app.dirty = true;
        }
        if guard.mouse_enabled() != app.mouse_on {
            let _ = guard.set_mouse(app.mouse_on);
        }
        if app.dirty && now.saturating_duration_since(app.last_draw) >= openc::app::FRAME {
            let t0 = Instant::now();
            terminal.draw(|f| ui::draw(&mut app, f, now))?;
            frames.commit()?;
            if panic_test {
                // Used by tools/openc-checks.sh to prove the terminal comes back after a panic.
                panic!("OPENC_TEST_PANIC");
            }
            if let Some(f) = stats_file.as_mut() {
                let _ = writeln!(f, "{} {}", t0.elapsed().as_micros(), frames.last_bytes());
            }
        }
        if app.suspend {
            app.suspend = false;
            guard.suspend();
            term::suspend_process();
            guard.resume()?;
            repaint(&mut terminal);
            app.dirty = true;
        }
        if let Some(ext) = app.external.take() {
            run_external(&mut guard, &input_ctl, &mut app, ext);
            repaint(&mut terminal);
            app.dirty = true;
        }
        if app.should_quit {
            break;
        }
        let wake = app.next_wake(Instant::now());
        tokio::select! {
            ev = pump.next() => {
                let Some(ev) = ev else { break };
                app.handle(ev, Instant::now());
                // Coalesce whatever else is already waiting into the same frame, but only for a
                // slice of time: a backlog must not keep the screen and the keyboard waiting.
                openc::pump::drain_slice(
                    || pump.next().now_or_never().flatten(),
                    |ev| app.handle(ev, Instant::now()),
                    openc::pump::SLICE,
                );
            }
            _ = async {
                match wake {
                    Some(t) => tokio::time::sleep_until(tokio::time::Instant::from_std(t)).await,
                    None => std::future::pending::<()>().await,
                }
            } => app.on_wake(Instant::now()),
        }
    }

    app.save_draft();
    let _ = tx.send(Request::Shutdown);
    drop(pump);
    drop(terminal);
    drop(guard);
    // The guard has put back every mode and the title; this is only the colors.
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b[0m");
    let _ = out.flush();
    Ok(app.exit_summary())
}

/// Clear the screen and force every cell to be written again. `Terminal::clear` asks the
/// terminal for the cursor position first and waits up to two seconds for the answer, which
/// stalls the loop; `resize` does the same clear without that query.
fn repaint(terminal: &mut Term) {
    if let Ok(size) = terminal.size() {
        let _ = terminal.resize(ratatui::layout::Rect::new(0, 0, size.width, size.height));
    }
}

/// Run `$EDITOR` or `$PAGER` with the terminal handed back, then take it again.
fn run_external(guard: &mut TermGuard, input: &InputControl, app: &mut App, ext: External) {
    let (text, editor) = match &ext {
        External::Editor(t) => (t.clone(), true),
        External::Pager(t) => (t.clone(), false),
    };
    // A prompt can hold anything you were about to send, so the copy is for you alone: a fresh
    // 0600 file with a random name, never a path somebody could have planted a link at.
    let Ok((path, mut file)) = openc::external::temp_file() else {
        return;
    };
    if file.write_all(text.as_bytes()).is_err() {
        let _ = std::fs::remove_file(&path);
        return;
    }
    drop(file);
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let cmd = if editor {
        var("VISUAL")
            .or_else(|| var("EDITOR"))
            .unwrap_or_else(|| "vi".into())
    } else {
        var("PAGER").unwrap_or_else(|| "less".into())
    };
    // The reader must be parked first, or it eats half of what is typed into the editor.
    input.pause();
    guard.suspend();
    let what = if editor { "editor" } else { "pager" };
    let result = openc::external::run(&cmd, &path, what);
    let _ = guard.resume();
    input.resume();
    if editor {
        // A failed editor leaves the draft alone; a quit without saving returns the same text.
        let new = result
            .is_ok()
            .then(|| std::fs::read_to_string(&path).ok())
            .flatten();
        app.editor_returned(&text, new);
    }
    if let Err(e) = result {
        app.set_flash(e, true, Instant::now());
    }
    let _ = std::fs::remove_file(&path);
}
