// OWNER: transcript
//! Copy to the system clipboard and say honestly how it went.
//!
//! OSC 52 is write-only and gives no answer, so the only way to avoid a silent failure is to
//! know beforehand which terminals take it. [`judge`] sorts the environment into `Yes`
//! (known to accept it, or tmux reports a clipboard-capable client), `No` (known not to, or
//! tmux will not forward it) and `Maybe`. The status line then says `copied`, says the text was
//! sent but unconfirmed, or says nothing was copied. Local helpers (`wl-copy`, `xclip`,
//! `pbcopy`) are tried only when there is no SSH connection, as the design asks, because over
//! SSH they would fill the remote machine's clipboard.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use crate::term::Caps;

/// OSC 52 payloads past this are refused by some terminals (xterm's default limit is 1 MB of
/// base64 and several stop at 100 KB), so a bigger copy is cut and the toast says so.
const MAX_BYTES: usize = 100_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Support {
    Yes,
    Maybe,
    No,
}

/// What the environment says about the terminal, gathered once.
#[derive(Clone, Debug, Default)]
pub struct Env {
    pub term: String,
    pub term_program: String,
    pub xtversion: String,
    pub vte: bool,
    pub konsole: bool,
    pub windows_terminal: bool,
    pub ssh: bool,
    /// Inside tmux: `Some(forwards)` where `forwards` is whether tmux will pass a copy on.
    pub tmux: Option<bool>,
}

fn var(k: &str) -> String {
    std::env::var(k).unwrap_or_default()
}

fn tmux_forwards() -> bool {
    let run = |args: &[&str]| -> Option<String> {
        let o = Command::new("tmux").args(args).output().ok()?;
        o.status
            .success()
            .then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    // `external` and `on` both let `load-buffer -w` reach the outer terminal; the client has to
    // advertise the clipboard feature for tmux to send anything.
    let setting = run(&["show-options", "-gv", "set-clipboard"]).unwrap_or_default();
    if setting == "off" {
        return false;
    }
    run(&["display-message", "-p", "#{client_termfeatures}"])
        .is_some_and(|f| f.split(',').any(|x| x == "clipboard"))
}

impl Env {
    pub fn from_env(caps: &Caps) -> Env {
        Env {
            term: var("TERM"),
            term_program: var("TERM_PROGRAM"),
            xtversion: caps.xtversion.clone().unwrap_or_default(),
            vte: !var("VTE_VERSION").is_empty(),
            konsole: !var("KONSOLE_VERSION").is_empty(),
            windows_terminal: !var("WT_SESSION").is_empty(),
            ssh: !var("SSH_CONNECTION").is_empty(),
            tmux: (!var("TMUX").is_empty()).then(tmux_forwards),
        }
    }
}

/// Does this terminal accept OSC 52?
pub fn judge(e: &Env) -> Support {
    if let Some(forwards) = e.tmux {
        return if forwards { Support::Yes } else { Support::No };
    }
    let xt = e.xtversion.to_ascii_lowercase();
    let term = e.term.to_ascii_lowercase();
    let prog = e.term_program.to_ascii_lowercase();
    let good = [
        "kitty",
        "foot",
        "wezterm",
        "ghostty",
        "alacritty",
        "contour",
        "iterm",
    ];
    if good
        .iter()
        .any(|g| xt.contains(g) || term.contains(g) || prog.contains(g))
        || e.windows_terminal
    {
        return Support::Yes;
    }
    if term == "linux" || term == "dumb" || prog == "apple_terminal" || e.vte {
        return Support::No;
    }
    if term.starts_with("screen") {
        // GNU screen drops it unless tmux is behind it, and tmux was handled above.
        return Support::No;
    }
    Support::Maybe
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub msg: String,
    pub warn: bool,
}

fn pipe_wait(cmd: &str, args: &[&str], text: &str) -> bool {
    let Ok(mut c) = Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    if let Some(mut si) = c.stdin.take() {
        if si.write_all(text.as_bytes()).is_err() {
            let _ = c.kill();
            return false;
        }
    }
    // wl-copy and xclip fork a server and exit at once; a helper that has not exited after a
    // second is stuck and is reported as a failure rather than waited on.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(c.wait().map(|s| s.success()).unwrap_or(false));
    });
    rx.recv_timeout(Duration::from_secs(1)).unwrap_or(false)
}

fn local_copy(text: &str) -> bool {
    if cfg!(target_os = "macos") {
        return pipe_wait("pbcopy", &[], text);
    }
    if !var("WAYLAND_DISPLAY").is_empty() && pipe_wait("wl-copy", &[], text) {
        return true;
    }
    !var("DISPLAY").is_empty() && pipe_wait("xclip", &["-selection", "clipboard"], text)
}

/// Cut `text` at a char boundary so the OSC 52 payload stays under [`MAX_BYTES`] of base64.
fn clip_len(text: &str) -> &str {
    let raw = MAX_BYTES / 4 * 3;
    if text.len() <= raw {
        return text;
    }
    let mut end = raw;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Pure part of [`copy`]: what to say given what happened.
pub fn report(
    support: Support,
    sent: bool,
    local_ok: bool,
    tmux_only: bool,
    chars: usize,
    cut: Option<usize>,
) -> Report {
    let n = format!("{chars} chars");
    let tail = cut.map_or(String::new(), |c| format!(", cut from {c}"));
    if local_ok || (sent && support == Support::Yes) {
        return Report {
            msg: format!("copied {n}{tail}"),
            warn: cut.is_some(),
        };
    }
    if tmux_only {
        return Report {
            msg: format!("{n} in the tmux buffer only, the outer terminal lacks OSC 52"),
            warn: true,
        };
    }
    match (sent, support) {
        (true, Support::Maybe) => Report {
            msg: format!("sent {n} by OSC 52, not confirmed{tail}"),
            warn: true,
        },
        _ => Report {
            msg: "this terminal blocks OSC 52, nothing copied; shift-drag selects natively"
                .to_string(),
            warn: true,
        },
    }
}

thread_local! {
    static CAPTURE: std::cell::RefCell<Option<Vec<String>>> = const { std::cell::RefCell::new(None) };
}

/// Route this thread's copies into a list instead of the terminal and the system clipboard.
/// For tests: nothing is written to stdout and no helper process is started.
pub fn capture() {
    CAPTURE.with(|c| *c.borrow_mut() = Some(Vec::new()));
}

/// Everything copied on this thread since [`capture`], oldest first.
pub fn captured() -> Vec<String> {
    CAPTURE.with(|c| c.borrow().clone().unwrap_or_default())
}

/// Is this thread routing copies into a list (see [`capture`])?
pub fn capturing() -> bool {
    CAPTURE.with(|c| c.borrow().is_some())
}

/// What the helpers did for one copy. Everything that can wait on another process is in here, so
/// it can be gathered off the UI thread; [`finish`] then writes the terminal escape and builds
/// the report.
#[derive(Clone, Debug)]
pub struct Copied {
    support: Support,
    local_ok: bool,
    tmux_only: bool,
    chars: usize,
    cut: Option<usize>,
    /// The OSC 52 to write, when the terminal may take it.
    osc: Option<String>,
}

/// Run the tmux and local helpers for `text`. Blocks on child processes (`tmux` twice on the
/// first call, then a helper that may sit for a second), so call it from a worker.
pub fn gather(text: &str, caps: &Caps) -> Copied {
    static ENV: OnceLock<Env> = OnceLock::new();
    let env = ENV.get_or_init(|| Env::from_env(caps));
    let support = judge(env);
    let body = clip_len(text);
    let cut = (body.len() < text.len()).then(|| text.chars().count());
    let chars = body.chars().count();
    let osc = (support != Support::No).then(|| crate::app::osc52(body));
    let mut local_ok = false;
    let mut tmux_only = false;
    if env.tmux.is_some() {
        let loaded = pipe_wait("tmux", &["load-buffer", "-w", "-"], body);
        local_ok = loaded && support == Support::Yes;
        tmux_only = loaded && !local_ok;
    }
    if !local_ok && !env.ssh && local_copy(body) {
        local_ok = true;
        tmux_only = false;
    }
    Copied {
        support,
        local_ok,
        tmux_only,
        chars,
        cut,
        osc,
    }
}

/// Write the OSC 52 (on the thread that draws, so it cannot land in the middle of a frame) and
/// say how the copy went.
pub fn finish(c: Copied) -> Report {
    let mut sent = false;
    if let Some(osc) = &c.osc {
        let mut out = std::io::stdout();
        sent = out
            .write_all(osc.as_bytes())
            .and_then(|()| out.flush())
            .is_ok();
    }
    report(c.support, sent, c.local_ok, c.tmux_only, c.chars, c.cut)
}

/// Copy `text` and report how, on this thread. Writes OSC 52 to the terminal unless it is known
/// not to work. The app uses [`gather`] and [`finish`] instead, so a slow helper never stops
/// the interface.
pub fn copy(text: &str, caps: &Caps) -> Report {
    let taken = CAPTURE.with(|c| match c.borrow_mut().as_mut() {
        Some(v) => {
            v.push(text.to_string());
            true
        }
        None => false,
    });
    if taken {
        return report(Support::Yes, true, false, false, text.chars().count(), None);
    }
    finish(gather(text, caps))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(term: &str) -> Env {
        Env {
            term: term.into(),
            ..Env::default()
        }
    }

    #[test]
    fn known_terminals_are_judged_from_the_environment() {
        assert_eq!(judge(&env("xterm-kitty")), Support::Yes);
        assert_eq!(judge(&env("foot")), Support::Yes);
        assert_eq!(judge(&env("alacritty")), Support::Yes);
        assert_eq!(judge(&env("linux")), Support::No);
        assert_eq!(judge(&env("xterm-256color")), Support::Maybe);
        let mut e = env("xterm-256color");
        e.vte = true;
        assert_eq!(judge(&e), Support::No);
        let mut e = env("xterm-256color");
        e.xtversion = "WezTerm 2024".into();
        assert_eq!(judge(&e), Support::Yes);
        let mut e = env("tmux-256color");
        e.tmux = Some(true);
        assert_eq!(judge(&e), Support::Yes);
        e.tmux = Some(false);
        assert_eq!(judge(&e), Support::No);
    }

    #[test]
    fn the_toast_never_claims_a_copy_that_did_not_happen() {
        let r = report(Support::Yes, true, false, false, 214, None);
        assert_eq!(
            r,
            Report {
                msg: "copied 214 chars".into(),
                warn: false
            }
        );
        let r = report(Support::Maybe, true, false, false, 9, None);
        assert!(r.warn && r.msg.contains("not confirmed"), "{r:?}");
        let r = report(Support::No, false, false, false, 9, None);
        assert!(r.warn && r.msg.contains("blocks OSC 52"), "{r:?}");
        // A local helper makes it a real copy even where OSC 52 is not accepted.
        let r = report(Support::No, false, true, false, 9, None);
        assert_eq!(r.msg, "copied 9 chars");
        let r = report(Support::No, false, false, true, 9, None);
        assert!(r.warn && r.msg.contains("tmux buffer only"), "{r:?}");
        let r = report(Support::Yes, true, false, false, 75_000, Some(90_000));
        assert!(r.warn && r.msg.contains("cut from 90000"), "{r:?}");
    }

    #[test]
    fn long_copies_are_cut_on_a_char_boundary() {
        let s = "é".repeat(60_000);
        let c = clip_len(&s);
        assert!(c.len() <= MAX_BYTES / 4 * 3 && s.starts_with(c));
        assert_eq!(clip_len("short"), "short");
    }
}
