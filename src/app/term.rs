//! Terminal lifecycle: raw mode + alternate screen setup/teardown, editor
//! suspension (`$EDITOR`), and clipboard writes (native tools, OSC 52, and
//! the multiplexer's own paste buffer).

use anyhow::{Context, Result};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::config::Config;

use super::App;

pub(super) type Tui = Terminal<CrosstermBackend<std::io::Stdout>>;

pub(super) fn setup_terminal() -> Result<Tui> {
    crossterm::terminal::enable_raw_mode().context("enabling raw mode")?;
    let mut stdout = std::io::stdout();
    // Capture the mouse so the scroll wheel scrolls the transcript (see the
    // ScrollUp/ScrollDown handler in `handle_event`). Without capture, the
    // terminal translates the wheel into ↑/↓ arrow keys in the alternate
    // screen, which the composer reads as input-history recall — so spinning
    // the wheel cycled previous messages instead of scrolling the text.
    // Tradeoff: capture pre-empts the terminal's native click-drag-to-select,
    // so wizard draws its own selection instead — drag to highlight, and the
    // covered text is copied on release by every route the terminal stack
    // offers (see `copy_to_clipboard`, the Down/Drag/Up handlers in
    // `handle_event`, and the highlight overlay in `crate::ui`). Ctrl-Y is the
    // keyboard path to the same copy, for the last reply. Holding
    // Shift still forces the terminal's own selection as a fallback. Bracketed
    // paste stays on so pasted text lands in the composer as one chunk.
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableBracketedPaste,
        crossterm::event::EnableMouseCapture,
    )
    .context("entering alternate screen")?;
    // Kitty keyboard protocol (best-effort): with disambiguation on, terminals
    // report Shift+Enter as Enter+SHIFT instead of a bare Enter, which lets the
    // composer bind it to a newline. Push unconditionally — unsupported
    // terminals ignore the CSI, and Pop in `restore_terminal` is also a no-op
    // there. Do **not** call `supports_keyboard_enhancement()` here: that
    // probe writes a CSI query and blocks on the reply (up to 2s, or forever
    // if another reader is draining stdin), and we already entered the
    // alternate screen above, so a hang looks like a blank Wizard that only
    // Ctrl-C can leave. Alt+Enter remains the fallback where the push is a
    // no-op.
    let _ = crossterm::execute!(
        stdout,
        crossterm::event::PushKeyboardEnhancementFlags(
            crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
        ),
    );
    Terminal::new(CrosstermBackend::new(stdout)).context("creating terminal")
}

/// Resolve the external editor: `$VISUAL`, then `$EDITOR`, then `nvim` when
/// it's on PATH. `None` means nothing usable is configured.
fn resolve_editor() -> Option<String> {
    for var in ["VISUAL", "EDITOR"] {
        if let Ok(editor) = std::env::var(var)
            && !editor.trim().is_empty()
        {
            return Some(editor);
        }
    }
    let nvim_on_path = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("nvim").is_file()));
    nvim_on_path.then(|| "nvim".to_string())
}

/// The command that opens `editor` on `path`.
///
/// Through the platform shell so editors configured with flags ("code --wait",
/// "emacsclient -t") work: the setting is a command *line*, not a program
/// name, so `Command::new(editor)` would look for a binary called
/// "code --wait". Its own function so that property is testable without
/// suspending a terminal.
fn editor_command(editor: &str, path: &std::path::Path) -> std::process::Command {
    crate::platform::shell::command(&format!("{editor} \"{}\"", path.display()))
}

/// Suspend the TUI, run `editor` on `path`, then restore the TUI. Returns the
/// editor's exit status, or `None` when the TUI could not be suspended or
/// restored (a notice is posted either way; an unrestored terminal is fatal
/// to the session, so the caller must not continue).
fn run_editor_suspended(
    app: &mut App,
    terminal: &mut Tui,
    editor: &str,
    path: &std::path::Path,
) -> Option<std::io::Result<std::process::ExitStatus>> {
    // Leave the alternate screen so the editor draws on the real terminal.
    if let Err(err) = restore_terminal() {
        app.notice(format!("could not suspend the TUI: {err:#}"));
        return None;
    }
    let status = editor_command(editor, path).status();

    // Re-enter the TUI regardless of how the editor exited.
    match setup_terminal() {
        Ok(new_terminal) => {
            *terminal = new_terminal;
            let _ = terminal.clear();
            Some(status)
        }
        Err(err) => {
            app.notice(format!(
                "could not restore the TUI: {err:#} — /quit and relaunch"
            ));
            None
        }
    }
}

/// Suspend the TUI, open the external editor on `~/.wizard/config.toml`, then
/// restore the TUI and reload the edited config. Driven by the `/settings`
/// "Open config file" row; runs from the main loop because it owns `terminal`.
/// Falls back to a path notice when no editor is configured.
pub(super) fn edit_config_file(app: &mut App, terminal: &mut Tui) {
    let path = match Config::path() {
        Ok(path) => path,
        Err(err) => {
            app.notice(format!("could not locate config: {err:#}"));
            return;
        }
    };
    let Some(editor) = resolve_editor() else {
        app.notice(format!(
            "no $EDITOR set — edit {} by hand, then /reload",
            path.display()
        ));
        return;
    };

    let Some(status) = run_editor_suspended(app, terminal, &editor, &path) else {
        return;
    };
    match status {
        Ok(status) if status.success() => match Config::load() {
            Ok(config) => {
                app.config = config;
                app.status.mode = app.config.mode;
                app.status.model = app.config.active().model;
                app.notice("config reloaded — restart for provider/model changes to take effect");
            }
            Err(err) => app.notice(format!("config not reloaded (parse error): {err:#}")),
        },
        Ok(_) => app.notice("editor exited without success — config not reloaded"),
        Err(err) => app.notice(format!("could not launch editor: {err:#}")),
    }
}

/// The name every composer draft starts with. The pid keeps two live sessions
/// off each other's file; the prefix is what the sweep matches on.
const DRAFT_PREFIX: &str = "wizard-prompt-";

/// How long an abandoned draft survives before the next Ctrl-G removes it.
///
/// The editor is modal (it owns the terminal while it runs), so a draft this
/// old belongs to a session that died between staging the file and reading it
/// back, and nothing is going to come for it. Generous anyway, because the
/// cost of sweeping too eagerly is deleting text a user still wants and the
/// cost of sweeping too late is a few kilobytes.
const DRAFT_TTL: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);

/// Where the composer draft is staged for the external editor.
///
/// Wizard's own private scratch directory rather than the shared temp dir: the
/// draft is an unsent prompt (often the most sensitive text in the session, and
/// frequently pasted credentials or logs), the name is predictable from the
/// pid, and `/tmp` is world-writable, so another local user could read it or
/// pre-plant the name and have the editor follow their symlink.
///
/// Hardened best-effort, not strictly, which is why this does not go through
/// [`crate::platform::paths::staging_dir`]: that helper's policy is set by its
/// other caller, where the staged file becomes the argument to `sudo install`
/// and a loose mode is worse than no directory at all. Here a `chmod` that the
/// filesystem cannot express (exFAT, WSL DrvFs, a CIFS mount with `WIZARD_HOME`
/// on it) would take Ctrl-G away entirely, in exchange for a protection that
/// filesystem was never going to provide. That is the same trade, and the same
/// reasoning, as the state tree's own directories.
fn prompt_scratch_path() -> Result<std::path::PathBuf> {
    let dir = crate::platform::paths::state_dir()?.join("scratch");
    crate::platform::secrets::create_private_dir(&dir)?;
    // The draft outlives the process whenever the TUI could not be restored
    // around the editor, and `/tmp`'s reaper is not here to collect it any
    // more. Sweeping on the way in keeps that from accumulating unsent prompts
    // for the life of the install.
    sweep_stale_drafts(&dir, DRAFT_TTL);
    Ok(dir.join(format!("{DRAFT_PREFIX}{}.md", std::process::id())))
}

/// Remove drafts in `dir` that have not been touched for `ttl`. Best effort
/// throughout: a scratch sweep must never be the reason the editor does not
/// open.
fn sweep_stale_drafts(dir: &std::path::Path, ttl: std::time::Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let is_draft = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(DRAFT_PREFIX));
        if !is_draft {
            continue;
        }
        // An unreadable mtime (or one in the future, which a clock change or a
        // restored backup can produce) reads as "young": the sweep's job is to
        // stop unbounded growth, not to guess at a file it cannot date.
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= ttl);
        if stale {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Write the draft the editor is about to open.
///
/// Owner-only where the filesystem can express it, and best effort where it
/// cannot, for the same reason [`prompt_scratch_path`] hardens its directory
/// that way. Deliberately not [`crate::platform::secrets::write_private_atomic`]:
/// that primitive hardens the parent strictly, which would put the hard
/// failure straight back. The 0700 directory is the lock that actually holds
/// here; the file mode is a second one on the same door.
fn stage_draft(path: &std::path::Path, text: &str) -> Result<()> {
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    if let Err(err) = crate::platform::secrets::harden_file(path) {
        tracing::warn!(
            "could not restrict permissions on {} ({err:#}); \
             the draft is readable by other users on this filesystem",
            path.display()
        );
    }
    Ok(())
}

/// Suspend the TUI and open the composer draft in the external editor
/// (Ctrl-G); on a clean exit the edited file replaces the input with the
/// cursor at the end. A nonzero exit leaves the composer untouched. Runs from
/// the main loop because it owns `terminal`.
pub(super) fn edit_prompt_in_editor(app: &mut App, terminal: &mut Tui) {
    let Some(editor) = resolve_editor() else {
        app.notice("no $VISUAL/$EDITOR set and nvim not on PATH — cannot edit the prompt");
        return;
    };

    let path = match prompt_scratch_path() {
        Ok(path) => path,
        Err(err) => {
            app.notice(format!("could not stage the prompt: {err:#}"));
            return;
        }
    };
    if let Err(err) = stage_draft(&path, &app.input) {
        app.notice(format!("could not stage the prompt: {err:#}"));
        return;
    }

    let Some(status) = run_editor_suspended(app, terminal, &editor, &path) else {
        // The draft stays behind here on purpose: the editor may already have
        // run, in which case this file holds the only copy of what the user
        // just wrote, and the session is over either way. `sweep_stale_drafts`
        // is what keeps it from outliving its usefulness.
        return;
    };
    match status {
        Ok(status) if status.success() => match std::fs::read_to_string(&path) {
            Ok(text) => app.set_input_from_editor(text),
            Err(err) => app.notice(format!("could not read the edited prompt: {err:#}")),
        },
        Ok(_) => app.notice("editor exited without success — prompt unchanged"),
        Err(err) => app.notice(format!("could not launch editor: {err:#}")),
    }
    let _ = std::fs::remove_file(&path);
}

// ---- Clipboard ---------------------------------------------------------
//
// There is no one mechanism that copies out of a full-screen TUI everywhere,
// so this section runs several and reports what actually landed. Three
// channels matter, and they are not alternatives to each other:
//
//   * A native clipboard tool (`wl-copy`, `xclip`, `xsel`, `pbcopy`,
//     `clip.exe`) sets the clipboard of the machine the *tool* runs on. That
//     is the right machine when Wizard is local and the wrong one over SSH.
//   * OSC 52 is an escape the terminal emulator interprets, so it is the only
//     channel that can reach the clipboard of the machine the *user* is
//     sitting at while Wizard runs on a server.
//   * tmux's paste buffer is where `prefix ]` pastes from, and is therefore
//     what a tmux user means by "copy" regardless of what the outer terminal
//     did with the escape. Zellij has no equivalent command; it intercepts
//     OSC 52 from the pane and forwards the copy itself.
//
// Both of the interesting failures are silent by construction. A native tool
// on a remote host exits zero after writing a clipboard nobody can see, and a
// bare OSC 52 inside tmux is swallowed before the outer terminal ever sees it
// (tmux only forwards the sequence it emits itself, and its DCS passthrough is
// off by default since 3.3a). So both used to report success while copying
// nothing. See `copy_to_clipboard` for the resulting order.

/// The parts of the environment that decide where a copy can land.
///
/// Captured as data rather than read at each decision point so the ordering
/// and the framing are testable without setting process-wide environment
/// variables from a parallel test run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct CopyEnv {
    /// Running inside a tmux client (`$TMUX`).
    tmux: bool,
    /// Running inside GNU screen, which frames escapes its own way.
    screen: bool,
    /// Running inside Zellij (`$ZELLIJ`). Zellij intercepts OSC 52 itself
    /// and forwards a copy to the outer terminal; it has no DCS wrapper.
    zellij: bool,
    /// This process is on the far end of an SSH session.
    ssh: bool,
    /// A display the native clipboard tools could plausibly write to.
    display: bool,
}

impl CopyEnv {
    fn detect() -> Self {
        let set = |key: &str| std::env::var_os(key).is_some_and(|value| !value.is_empty());
        Self::from_parts(
            set("TMUX"),
            set("STY"),
            set("ZELLIJ"),
            &std::env::var("TERM").unwrap_or_default(),
            set("SSH_CONNECTION") || set("SSH_TTY") || set("SSH_CLIENT"),
            set("DISPLAY") || set("WAYLAND_DISPLAY") || set("WAYLAND_SOCKET") || cfg!(not(unix)),
        )
    }

    /// Split out from [`CopyEnv::detect`] because the multiplexer question is
    /// the one with a trap in it: tmux sets `TERM=screen-256color` on plenty
    /// of installs, so a `$TERM`-first reading of "am I under screen" says yes
    /// inside tmux and frames every escape the wrong way. `$TMUX` decides
    /// first, `$ZELLIJ` next, and `$TERM` only gets a say when neither is set.
    /// Nested `tmux` inside Zellij is possible; tmux still owns the pane then,
    /// so its wrapper wins.
    fn from_parts(
        tmux: bool,
        sty: bool,
        zellij: bool,
        term: &str,
        ssh: bool,
        display: bool,
    ) -> Self {
        Self {
            tmux,
            screen: !tmux && !zellij && (sty || term.starts_with("screen")),
            zellij: zellij && !tmux,
            ssh,
            display,
        }
    }

    /// Whether the escape and the multiplexer buffer are tried before the
    /// native tools.
    ///
    /// Over SSH they must be: `xclip` on the server writes a clipboard on the
    /// server, and exits zero doing it, so letting it answer first is how a
    /// copy comes to report success while the user's own clipboard never
    /// changes. Inside tmux or Zellij the same reordering is merely correct
    /// rather than critical, since the paste about to be attempted is probably
    /// the multiplexer's own.
    ///
    /// Order only. No leg suppresses another any more, which is the other half
    /// of the same bug: the previous code returned the moment a native tool
    /// exited zero and never sent the escape at all.
    fn prefer_escape(self) -> bool {
        self.ssh || self.tmux || self.zellij
    }

    /// Whether a native clipboard tool is worth spawning at all.
    ///
    /// Remote with no display is the one case where it is not: every candidate
    /// fails, and all the attempt buys is five process spawns on every drag.
    /// A forwarded X11 session still has a display, and still gets the run.
    fn native_worth_trying(self) -> bool {
        !self.ssh || self.display
    }
}

/// Largest selection Wizard will push through OSC 52, in bytes of text before
/// encoding.
///
/// Terminals cap the sequence, and the ones that cap it mostly discard an
/// oversized one rather than truncating it, which is the failure worth
/// designing against: a copy that reports success and pastes nothing. The real
/// ceilings are all far higher than this and they disagree with each other
/// (xterm parses 600_000 bytes of sequence, tmux and iTerm2 buffer 1 MiB,
/// Ghostty 8 MiB, several others impose nothing at all), so there is no
/// number that is exactly right.
///
/// This one is deliberately conservative and deliberately conventional: it is
/// the budget hterm's `osc52.sh` picked (100_000 bytes for the whole sequence,
/// less the framing, times three quarters for base64), which spread through
/// enough shell helpers and editor plugins to have become the de-facto
/// interoperable size. A selection past it is a transcript dump, and the
/// routes that have no cap at all still take it.
const OSC52_MAX_TEXT: usize = 74_994;

/// How much of a DCS string sequence to hand GNU screen at once, wrapper
/// included.
///
/// screen buffers a string sequence in a fixed `MAXSTR` array and drops
/// whatever runs past it. That bound has moved with the version (256, then 512
/// in 4.2.0, then 768 in 4.2.1, and 2560 on current builds), so the safe chunk
/// is the oldest one rather than the newest: overshooting silently loses the
/// tail of the payload, and undershooting costs a few extra four-byte
/// wrappers. 256 is what hterm's reference implementation settled on for the
/// same reason.
const SCREEN_CHUNK: usize = 256 - 4;

/// Copy `text` to the clipboard by every route this terminal stack offers.
///
/// Every applicable route runs. That is the correction: the previous version
/// treated them as a fallback chain and returned as soon as a native tool
/// exited zero, which over SSH is exactly the case where the native tool wrote
/// a clipboard nobody was looking at. Which route the user actually pastes
/// from is not knowable from in here, so guessing wrong has to stay cheap.
///
/// The routes, in the order they are tried:
///
/// 1. **A native clipboard tool** (`wl-copy` / `xclip` / `pbcopy` / …), first
///    in a local session, because there it is the one channel certain to reach
///    the clipboard the user will paste from. Over SSH it moves behind the
///    other two and is skipped entirely with no display to write to.
/// 2. **tmux's own paste buffer**, whenever `$TMUX` says there is one. This is
///    what a tmux user means by copy, and it holds regardless of what the
///    outer terminal did with any escape. `load-buffer -w` also has tmux push
///    the buffer outward with an OSC 52 of its own, which is the one route
///    that survives tmux's default settings.
/// 3. **The clipboard escape**, framed for the multiplexer that has to carry
///    it, skipped when the selection is over [`OSC52_MAX_TEXT`].
///
/// Returns the notice worth putting on screen, or `None` when the copy was
/// unremarkable. An error means nothing at all accepted the text.
pub(super) fn copy_to_clipboard(text: &str) -> Result<Option<String>> {
    copy_with_env(text, CopyEnv::detect())
}

/// [`copy_to_clipboard`] with the environment passed in, so the ordering is
/// reachable from a test.
fn copy_with_env(text: &str, env: CopyEnv) -> Result<Option<String>> {
    let mut landed: Vec<&'static str> = Vec::new();
    let mut failures: Vec<String> = Vec::new();

    let mut record = |result: Result<()>, label: &'static str| match result {
        Ok(()) => landed.push(label),
        Err(err) => failures.push(format!("{err:#}")),
    };

    let native = env.native_worth_trying();
    if native && !env.prefer_escape() {
        record(copy_via_native(text), "the system clipboard");
    }

    if env.tmux {
        record(copy_via_tmux_buffer(text), "tmux's paste buffer");
    }

    let oversize = text.len() > OSC52_MAX_TEXT;
    if !oversize {
        record(write_escape(text, env), "the terminal (OSC 52)");
    }

    if native && env.prefer_escape() {
        record(copy_via_native(text), "this host's clipboard");
    }

    if landed.is_empty() {
        let detail = if failures.is_empty() {
            "no clipboard channel is available here".to_string()
        } else {
            failures.join("; ")
        };
        if oversize {
            anyhow::bail!(
                "selection is {} bytes, past the {OSC52_MAX_TEXT}-byte cap on the clipboard \
                 escape, and nothing else took it: {detail}",
                text.len()
            );
        }
        anyhow::bail!("could not copy: {detail}");
    }

    if oversize {
        return Ok(Some(format!(
            "selection is {} bytes, past the {OSC52_MAX_TEXT}-byte cap on the clipboard escape. \
             It went to {}, but the terminal's own clipboard was left alone.",
            text.len(),
            landed.join(" and ")
        )));
    }
    Ok(None)
}

/// Frame the OSC 52 clipboard escape for `text` the way this terminal stack
/// needs it, or `None` when the selection is too big to send honestly.
///
/// Pure, and separate from the write, because everything interesting about
/// this code is in the bytes: a terminal is not something a test can hold.
fn clipboard_escape(text: &str, env: CopyEnv) -> Option<String> {
    if text.len() > OSC52_MAX_TEXT {
        return None;
    }
    let sequence = osc52(text);
    Some(if env.tmux {
        wrap_tmux(&sequence)
    } else if env.screen {
        wrap_screen(&sequence)
    } else {
        sequence
    })
}

/// `ESC ] 52 ; c ; <base64> BEL`. The `c` selects the clipboard (as against
/// the primary selection), and the payload is base64 so it can carry newlines
/// and anything else a transcript row holds without terminating the sequence
/// early.
///
/// BEL rather than the equally legal `ESC \` terminator, because of what
/// [`wrap_tmux`] then has to do to this string: BEL leaves exactly one ESC in
/// the whole sequence, so the doubling rule has one place to apply instead of
/// two, and the tmux form ends `…BEL ESC \` instead of a run of three escapes
/// whose reading depends on where the wrapper stops. Every implementation that
/// wraps for tmux uses BEL for this reason.
fn osc52(text: &str) -> String {
    use base64::Engine;

    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    format!("\x1b]52;c;{encoded}\x07")
}

/// tmux's DCS passthrough: `ESC P tmux ; <payload> ESC \`, with every ESC in
/// the payload doubled.
///
/// The doubling is not decoration. tmux scans the passthrough body for the
/// string terminator, so an undoubled ESC inside it ends the sequence there
/// and the rest of the base64 is printed into the pane as text. Doubling makes
/// tmux emit one ESC per pair and keeps looking for the real terminator.
///
/// Worth knowing where this does and does not help. tmux has two independent
/// doors to the outer terminal's clipboard and ships with both of them shut:
/// `allow-passthrough` (off by default since 3.3a) is what carries this
/// sequence, and `set-clipboard` (`external` by default, which means "ignore
/// what applications send me") is what would carry a bare, unwrapped OSC 52.
/// So neither escape works on a stock install, which is the whole reported
/// bug. This one is still sent because it costs nothing and it is the entire
/// answer for anyone who has turned the option on; [`copy_via_tmux_buffer`] is
/// what carries the copy for everyone else, through a third door that is open
/// by default.
fn wrap_tmux(payload: &str) -> String {
    let mut out = String::with_capacity(payload.len() + 16);
    out.push_str("\x1bPtmux;");
    for ch in payload.chars() {
        if ch == '\x1b' {
            out.push('\x1b');
        }
        out.push(ch);
    }
    out.push_str("\x1b\\");
    out
}

/// GNU screen's DCS wrapper, in pieces screen will not truncate.
///
/// screen passes the body of a string sequence (`ESC P … ESC \`) through to
/// the terminal it is attached to, unaltered and with no doubling rule of its
/// own, but it drops whatever runs past [`SCREEN_CHUNK`], and a base64'd
/// selection passes that in a couple of lines. Splitting the escape across
/// consecutive string sequences works because the outer terminal sees the
/// concatenated bodies as one stream: the same bytes, delivered in
/// instalments.
///
/// A chunk never ends on a lone ESC. Splitting an escape from the byte it
/// introduces would hand the outer terminal an introducer with no argument
/// followed by an argument with no introducer.
fn wrap_screen(payload: &str) -> String {
    let mut out = String::with_capacity(payload.len() + payload.len() / SCREEN_CHUNK * 4 + 8);
    let mut chunk = String::with_capacity(SCREEN_CHUNK);
    for ch in payload.chars() {
        if chunk.len() + ch.len_utf8() > SCREEN_CHUNK && !chunk.ends_with('\x1b') {
            out.push_str("\x1bP");
            out.push_str(&chunk);
            out.push_str("\x1b\\");
            chunk.clear();
        }
        chunk.push(ch);
    }
    if !chunk.is_empty() {
        out.push_str("\x1bP");
        out.push_str(&chunk);
        out.push_str("\x1b\\");
    }
    out
}

/// Load `text` into tmux's paste buffer, which is what `prefix ]` pastes.
///
/// `-w` (tmux 3.2 and later) additionally has tmux set the outer terminal's
/// clipboard with an OSC 52 of its own. That flag is the practical answer to
/// the whole problem: tmux's default `set-clipboard external` ignores an
/// application's OSC 52 outright, and its DCS passthrough is off by default,
/// so a sequence Wizard writes itself is discarded on a stock install, while
/// one tmux emits on its own behalf is not. Older tmux rejects the flag (it
/// arrived in 3.2) and exits nonzero straight away rather than hanging, so the
/// retry without it costs one failed spawn and still fills the paste buffer.
///
/// Runs on the event loop, like every other leg of a copy, which is fine
/// because tmux drains its stdin as fast as it is written and answers in
/// milliseconds. A tmux server wedged badly enough to stop reading would stall
/// the frame instead, at the pipe buffer.
fn copy_via_tmux_buffer(text: &str) -> Result<()> {
    if pipe_to("tmux", &["load-buffer", "-w", "-"], text.as_bytes()).is_ok() {
        return Ok(());
    }
    pipe_to("tmux", &["load-buffer", "-"], text.as_bytes()).context("loading tmux's paste buffer")
}

/// Write the clipboard escape where each layer of the terminal stack will
/// actually read it.
///
/// The pane (stdout, then `/dev/tty`) gets the sequence framed for the mux
/// in the way: tmux's DCS passthrough, screen's chunked DCS, or the bare
/// OSC 52 Zellij intercepts itself. `$SSH_TTY` is the sshd pty *outside*
/// that mux, so a wrapped sequence there is bytes the laptop's emulator
/// does not understand. Over SSH the bare OSC 52 is written there too.
/// That is the route that reaches the clipboard of the machine the user
/// is sitting at when tmux's `set-clipboard` is `external` and
/// `allow-passthrough` is off, and when Zellij is between Wizard and sshd.
fn write_escape(text: &str, env: CopyEnv) -> Result<()> {
    use std::io::{IsTerminal, Write};

    let Some(framed) = clipboard_escape(text, env) else {
        anyhow::bail!("selection is past the clipboard-escape cap");
    };

    let mut last: Option<anyhow::Error> = None;
    let mut wrote = false;
    let mut stdout = std::io::stdout();
    if stdout.is_terminal() {
        match stdout
            .write_all(framed.as_bytes())
            .and_then(|_| stdout.flush())
        {
            Ok(()) => {
                wrote = true;
                if !env.ssh && !env.tmux && !env.zellij && !env.screen {
                    return Ok(());
                }
            }
            Err(err) => {
                last = Some(anyhow::Error::new(err).context("writing clipboard escape to stdout"));
            }
        }
    }

    match emit_to_path("/dev/tty", &framed) {
        Ok(()) => wrote = true,
        Err(err) => last = Some(err),
    }

    // `$SSH_TTY` is this session's pty as sshd sees it. Inside tmux or
    // Zellij that is still the mux pane (same device as `/dev/tty`), so
    // a second write of the same framed sequence is a no-op, and a bare
    // OSC 52 would be the one tmux's default `set-clipboard external`
    // discards. Only write it when there is no mux in the way, where it
    // is the outer sshd pty and the framed sequence on stdout may have
    // gone to a redirected handle the laptop never sees.
    if env.ssh
        && !env.tmux
        && !env.zellij
        && !env.screen
        && let Some(ssh_tty) = std::env::var_os("SSH_TTY")
        && !ssh_tty.is_empty()
        && ssh_tty != "/dev/tty"
    {
        match emit_to_path(&ssh_tty, &framed) {
            Ok(()) => wrote = true,
            Err(err) => last = Some(err),
        }
    }

    if wrote {
        return Ok(());
    }
    Err(last.unwrap_or_else(|| {
        anyhow::anyhow!("stdout is not a terminal and no tty is available for the clipboard escape")
    }))
}

fn emit_to_path(path: impl AsRef<std::path::Path>, sequence: &str) -> Result<()> {
    use std::io::Write;

    let path = path.as_ref();
    let mut tty = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("opening {path:?}"))?;
    tty.write_all(sequence.as_bytes())
        .with_context(|| format!("writing clipboard escape to {path:?}"))?;
    tty.flush()
        .with_context(|| format!("flushing clipboard escape to {path:?}"))
}

/// Pipe `text` into the first available OS clipboard writer.
fn copy_via_native(text: &str) -> Result<()> {
    // (program, args). Order: Wayland first when a Wayland session is visible,
    // then X11, then the macOS / Windows builtins. `pipe_to` only returns Ok
    // when the process exits zero, so a missing binary or a dead compositor
    // just falls through to the next candidate.
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var_os("WAYLAND_SOCKET").is_some();

    let mut candidates: Vec<(&str, &[&str])> = Vec::new();
    if wayland {
        candidates.push(("wl-copy", &[]));
    }
    candidates.extend_from_slice(&[
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
        ("pbcopy", &[]),   // macOS
        ("clip.exe", &[]), // Windows (WSL / native)
    ]);
    if !wayland {
        // wl-copy can still work under XWayland-ish setups; try it last if we
        // didn't already lead with it.
        candidates.push(("wl-copy", &[]));
    }

    let mut last_err: Option<anyhow::Error> = None;
    for (cmd, args) in candidates {
        match pipe_to(cmd, args, text.as_bytes()) {
            Ok(()) => return Ok(()),
            Err(err) => last_err = Some(err),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no clipboard tool available")))
}

/// Spawn `cmd args`, write `bytes` to its stdin, and require a zero exit.
fn pipe_to(cmd: &str, args: &[&str], bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawning {cmd}"))?;
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("{cmd} stdin missing"))?;
        stdin
            .write_all(bytes)
            .with_context(|| format!("writing to {cmd} stdin"))?;
        // Drop stdin to close the pipe so tools that read to EOF (wl-copy,
        // pbcopy) finish rather than hanging.
    }
    let status = child.wait().with_context(|| format!("waiting for {cmd}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow::anyhow!("{cmd} exited with {status}"))
    }
}

fn restore_terminal() -> Result<()> {
    // Pop the keyboard-enhancement flags pushed in `setup_terminal`. Done
    // unconditionally (and ignoring errors): popping an empty/absent stack is a
    // no-op on supporting terminals and an ignored escape elsewhere, which is
    // safer than re-querying support from a panic/teardown path.
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::PopKeyboardEnhancementFlags,
    );
    crossterm::execute!(
        std::io::stdout(),
        crossterm::event::DisableMouseCapture,
        crossterm::event::DisableBracketedPaste,
        crossterm::terminal::LeaveAlternateScreen,
    )
    .context("leaving alternate screen")?;
    crossterm::terminal::disable_raw_mode().context("disabling raw mode")?;
    Ok(())
}

/// Restore the terminal if (and only if) raw mode is active. Safe to call
/// from a panic hook or after a headless run — it does nothing when the TUI
/// never started.
pub fn restore_terminal_best_effort() {
    if is_terminal_armed() {
        let _ = restore_terminal();
    }
}

/// Whether the terminal is still in the state [`setup_terminal`] put it in.
///
/// Raw mode is the marker for the whole arrangement: it goes on first and
/// comes off last, and every teardown path in this module (and the panic hook
/// in `main`) moves it in lockstep with the alternate screen, mouse capture,
/// and bracketed paste. The event loop polls this every frame because the
/// panic hook can strip all of it while the process keeps running — see
/// [`crate::app::recover::TerminalWatchdog`] for why that happens and what the
/// loop does about it.
pub(super) fn is_terminal_armed() -> bool {
    crossterm::terminal::is_raw_mode_enabled().unwrap_or(false)
}

/// Restores the terminal when the main loop unwinds or errors out.
pub(super) struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal_best_effort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompt_scratch_file_is_private_and_not_in_the_shared_temp_dir() {
        // The draft is unsent user text and the file name is derivable from the
        // pid, so a world-writable directory is both a disclosure and a
        // symlink-planting opportunity. Pin the directory, not just the name.
        let path = prompt_scratch_path().expect("scratch path");
        let dir = path.parent().expect("scratch parent");
        assert_eq!(dir.file_name().and_then(|n| n.to_str()), Some("scratch"));
        assert_eq!(
            dir.parent(),
            Some(
                crate::platform::paths::state_dir()
                    .expect("state dir")
                    .as_path()
            ),
            "the scratch file must live under Wizard's own state tree"
        );
        assert_ne!(
            dir,
            crate::platform::paths::temp_dir(),
            "the shared temp dir is world-writable"
        );
        assert!(
            crate::platform::secrets::is_protected(dir).expect("stat the scratch dir"),
            "{} must be owner-only",
            dir.display()
        );

        // And the file itself, which is what actually holds the text: a 0700
        // parent is the lock that matters, but a draft written at the process
        // umask is one `chmod` on the directory away from being world-readable.
        stage_draft(&path, "an unsent prompt with a pasted key in it").expect("stage the draft");
        assert!(
            crate::platform::secrets::is_protected(&path).expect("stat the draft"),
            "{} must be owner-only",
            path.display()
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read the draft"),
            "an unsent prompt with a pasted key in it"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn abandoned_drafts_are_swept_and_live_ones_are_not() {
        // `/tmp` had a reaper; `~/.wizard/scratch` does not, so a session that
        // died between staging the draft and reading it back used to leave an
        // unsent prompt on disk for the life of the install, one file per pid.
        let dir = tempfile::tempdir().expect("tempdir");
        let old = dir.path().join(format!("{DRAFT_PREFIX}101.md"));
        let fresh = dir.path().join(format!("{DRAFT_PREFIX}102.md"));
        let unrelated = dir.path().join("notes.md");
        for path in [&old, &fresh, &unrelated] {
            std::fs::write(path, "draft").expect("write");
        }
        let ancient = std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 86_400);
        for path in [&old, &unrelated] {
            std::fs::File::options()
                .write(true)
                .open(path)
                .expect("open")
                .set_modified(ancient)
                .expect("backdate");
        }

        sweep_stale_drafts(dir.path(), DRAFT_TTL);

        assert!(!old.exists(), "an abandoned draft must be swept");
        assert!(fresh.exists(), "a draft in use must survive");
        assert!(
            unrelated.exists(),
            "the sweep must only touch its own file names"
        );

        // A sweep that ran with the shipped TTL against files this new would
        // delete a draft the user is editing right now.
        sweep_stale_drafts(dir.path(), std::time::Duration::from_secs(0));
        assert!(!fresh.exists(), "the age threshold is what decides");
        assert!(unrelated.exists());
    }

    #[test]
    fn the_editor_runs_as_a_command_line_not_a_program_name() {
        // `$EDITOR` is routinely "code --wait" or "emacsclient -t", so the
        // whole string has to reach a shell as one argument; splitting it (or
        // passing it to `Command::new`) would look for a binary with a space in
        // its name. Built through the same function `run_editor_suspended`
        // calls, so reverting that call to `Command::new(editor).arg(path)`
        // fails here.
        let command = editor_command("code --wait", std::path::Path::new("/some/draft.md"));
        let program = command.get_program().to_string_lossy().into_owned();
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_ne!(
            program, "code --wait",
            "the editor setting is not a program name"
        );
        assert_eq!(program, crate::platform::shell::name());
        assert_eq!(args.len(), 2, "expected <flag> <line>, got {args:?}");
        assert_eq!(args[1], "code --wait \"/some/draft.md\"");
    }

    /// A plain terminal takes the sequence as it stands.
    const PLAIN: CopyEnv = CopyEnv {
        tmux: false,
        screen: false,
        zellij: false,
        ssh: false,
        display: true,
    };

    #[test]
    fn a_plain_terminal_gets_the_bare_osc_52_escape() {
        let escape = clipboard_escape("hi", PLAIN).expect("a two-byte selection fits");
        // `aGk=` is base64("hi"); `c` is the clipboard selection, and BEL ends
        // the sequence.
        assert_eq!(escape, "\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn the_payload_is_base64_so_newlines_cannot_end_the_sequence_early() {
        // A transcript selection is multi-line by definition, and a raw
        // payload would end at the first control byte in it.
        let escape = clipboard_escape("one\ntwo", PLAIN).expect("fits");
        assert_eq!(escape, "\x1b]52;c;b25lCnR3bw==\x07");
        assert_eq!(
            escape.matches('\x07').count(),
            1,
            "exactly one terminator: {escape:?}"
        );
    }

    #[test]
    fn under_tmux_the_escape_is_wrapped_in_the_dcs_passthrough() {
        let env = CopyEnv {
            tmux: true,
            ..PLAIN
        };
        let escape = clipboard_escape("hi", env).expect("fits");
        assert_eq!(escape, "\x1bPtmux;\x1b\x1b]52;c;aGk=\x07\x1b\\");
    }

    #[test]
    fn tmux_passthrough_doubles_every_escape_in_the_payload() {
        // The one that bites: tmux reads the passthrough body looking for the
        // string terminator, so a single ESC inside it ends the sequence there
        // and the rest of the base64 is printed into the pane as text. This is
        // asserted on a payload with several escapes, not on the one the OSC
        // 52 builder happens to produce, so the rule stays the rule.
        let wrapped = wrap_tmux("a\x1bb\x1b\x1bc");
        assert_eq!(wrapped, "\x1bPtmux;a\x1b\x1bb\x1b\x1b\x1b\x1bc\x1b\\");
        assert!(wrapped.starts_with("\x1bPtmux;"));
        assert!(wrapped.ends_with("\x1b\\"));

        // Every ESC between the introducer and the terminator is part of a
        // pair.
        let body = &wrapped["\x1bPtmux;".len()..wrapped.len() - 2];
        assert_eq!(body.matches('\x1b').count() % 2, 0, "body: {body:?}");
    }

    #[test]
    fn under_screen_the_escape_is_split_into_sequences_screen_will_not_truncate() {
        // Long enough to need several chunks: screen drops whatever runs past
        // its string-sequence limit, so one long sequence loses the tail of
        // the payload and pastes garbage.
        let text = "x".repeat(4_000);
        let env = CopyEnv {
            screen: true,
            ..PLAIN
        };
        let escape = clipboard_escape(&text, env).expect("fits under the cap");

        let chunks: Vec<&str> = escape
            .split("\x1b\\")
            .filter(|part| !part.is_empty())
            .collect();
        assert!(
            chunks.len() > 1,
            "expected several chunks, got {}",
            chunks.len()
        );
        for chunk in &chunks {
            assert!(chunk.starts_with("\x1bP"), "chunk without a DCS introducer");
            // The introducer and the terminator count against screen's buffer
            // too, so what has to fit is the whole string sequence, not its
            // body. 256 is the oldest MAXSTR any screen shipped with.
            assert!(
                chunk.len() + "\x1b\\".len() <= 256,
                "string sequence of {} bytes is past the oldest screen's MAXSTR",
                chunk.len() + 2
            );
        }

        // Concatenating the bodies has to reproduce the escape exactly: the
        // point of chunking is that the outer terminal sees one stream.
        let rebuilt: String = chunks.iter().map(|chunk| &chunk[2..]).collect();
        assert_eq!(rebuilt, osc52(&text));
    }

    #[test]
    fn a_screen_chunk_never_ends_on_a_lone_escape() {
        // Splitting an ESC from the byte it introduces hands the outer
        // terminal an introducer with no argument followed by an argument with
        // no introducer. Forced here by a payload that is all escapes, so a
        // boundary lands on one no matter where it falls.
        let wrapped = wrap_screen(&"\x1b".repeat(SCREEN_CHUNK * 3));
        for chunk in wrapped.split("\x1b\\").filter(|part| !part.is_empty()) {
            let body = &chunk[2..];
            assert!(!body.is_empty(), "an empty chunk carries nothing");
        }
        let rebuilt: String = wrapped
            .split("\x1b\\")
            .filter(|part| !part.is_empty())
            .map(|chunk| &chunk[2..])
            .collect();
        assert_eq!(rebuilt, "\x1b".repeat(SCREEN_CHUNK * 3));
    }

    #[test]
    fn an_oversize_selection_gets_no_escape_rather_than_a_truncated_one() {
        // Terminals drop an oversized OSC 52 in silence, so sending one is a
        // copy that reports success and pastes nothing. `None` here is what
        // makes `copy_with_env` say so out loud.
        assert!(clipboard_escape(&"x".repeat(OSC52_MAX_TEXT), PLAIN).is_some());
        assert!(clipboard_escape(&"x".repeat(OSC52_MAX_TEXT + 1), PLAIN).is_none());
        // And the decision is on the text, not on the framing: the same
        // selection is over the cap under every multiplexer.
        for env in [
            CopyEnv {
                tmux: true,
                ..PLAIN
            },
            CopyEnv {
                screen: true,
                ..PLAIN
            },
            CopyEnv {
                zellij: true,
                ..PLAIN
            },
        ] {
            assert!(clipboard_escape(&"x".repeat(OSC52_MAX_TEXT + 1), env).is_none());
        }
    }

    #[test]
    fn ssh_and_tmux_put_the_escape_ahead_of_the_native_clipboard_tools() {
        // The bug this ordering exists for: `xclip` on the far end of an SSH
        // session writes the *server's* clipboard, which nobody can see, and
        // exits zero doing it. Running it first means every copy over SSH
        // reports success and changes nothing the user can paste.
        assert!(CopyEnv { ssh: true, ..PLAIN }.prefer_escape());
        assert!(
            CopyEnv {
                tmux: true,
                ..PLAIN
            }
            .prefer_escape()
        );
        assert!(
            CopyEnv {
                zellij: true,
                ..PLAIN
            }
            .prefer_escape()
        );
        assert!(
            !PLAIN.prefer_escape(),
            "a local session should still lead with the native tool"
        );
    }

    #[test]
    fn the_native_tools_are_skipped_only_when_there_is_no_display_to_write_to() {
        // Five process spawns per drag, every one of them certain to fail, is
        // what a headless server used to pay before the escape got its turn.
        assert!(
            !CopyEnv {
                ssh: true,
                display: false,
                ..PLAIN
            }
            .native_worth_trying()
        );
        // A forwarded X11 session is remote and still has somewhere to write.
        assert!(
            CopyEnv {
                ssh: true,
                display: true,
                ..PLAIN
            }
            .native_worth_trying()
        );
        // Local sessions always try, display or not: macOS sets no $DISPLAY
        // and `pbcopy` works fine there.
        assert!(
            CopyEnv {
                ssh: false,
                display: false,
                ..PLAIN
            }
            .native_worth_trying()
        );
    }

    #[test]
    fn tmux_is_not_mistaken_for_screen_by_the_term_string_it_sets() {
        // tmux ships `TERM=screen-256color` on plenty of installs, so reading
        // $TERM first says "screen" inside tmux and frames every escape with
        // the wrong wrapper.
        let inside_tmux = CopyEnv::from_parts(true, false, false, "screen-256color", true, false);
        assert!(inside_tmux.tmux);
        assert!(!inside_tmux.screen);
        assert!(!inside_tmux.zellij);

        let inside_screen =
            CopyEnv::from_parts(false, true, false, "screen.xterm-256color", false, true);
        assert!(inside_screen.screen);
        assert!(!inside_screen.tmux);

        // $STY is the reliable marker, but a screen session that lost it is
        // still recognisable from $TERM.
        assert!(CopyEnv::from_parts(false, false, false, "screen", false, false).screen);
        assert!(!CopyEnv::from_parts(false, false, false, "xterm-256color", false, false).screen);
    }

    #[test]
    fn zellij_gets_the_bare_osc_52_and_prefers_the_escape() {
        // Zellij intercepts OSC 52 itself and forwards a copy. Wrapping it in
        // tmux's DCS would print the sequence into the pane. Nested tmux
        // inside Zellij still belongs to tmux: that pane's $TMUX is set.
        let env = CopyEnv::from_parts(false, false, true, "xterm-256color", true, false);
        assert!(env.zellij);
        assert!(!env.tmux);
        assert!(!env.screen);
        assert!(env.prefer_escape());
        let escape = clipboard_escape("hi", env).expect("fits");
        assert_eq!(escape, "\x1b]52;c;aGk=\x07");

        let nested = CopyEnv::from_parts(true, false, true, "tmux-256color", true, false);
        assert!(nested.tmux);
        assert!(!nested.zellij);
        assert_eq!(
            clipboard_escape("hi", nested).expect("fits"),
            "\x1bPtmux;\x1b\x1b]52;c;aGk=\x07\x1b\\"
        );
    }

    #[test]
    fn ssh_client_alone_is_enough_to_prefer_the_escape() {
        // Some sshds set SSH_CLIENT and not SSH_CONNECTION / SSH_TTY. Missing
        // that used to lead with xclip on the server.
        let env = CopyEnv {
            ssh: true,
            display: false,
            ..PLAIN
        };
        assert!(env.prefer_escape());
        assert!(!env.native_worth_trying());
    }
}
