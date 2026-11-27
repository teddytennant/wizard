//! Tells the terminal what the session is doing: the window title, and a
//! Program Status Protocol report (OSC 7501).
//!
//! Rex, which defined the protocol, draws the report on the tab: a moving
//! indicator while a turn runs, a mark when it finishes, another when a
//! permission prompt is waiting. Without it the tab looks like a shell.
//!
//! [`agent_core::status`] works out what to say. This writes it, from
//! [`EventPump`](crate::term::EventPump), which every look polls on the thread
//! that draws and never in the middle of a frame.
//!
//! The title goes to every terminal. The report goes only where the
//! environment says it will be read; asking the terminal would mean waiting
//! on a reply that may never come.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use agent_core::status::{Kind, State, Status};
use base64::Engine;

/// The name reports are filed under. A terminal picks the tab's icon from it.
const APP_NAME: &str = "wizard";

/// Longest title, in columns. Tabs are narrow.
const TITLE_COLS: usize = 48;

/// Longest message, in columns. The protocol allows 2048 bytes of it, but it
/// is drawn on one line.
const MSG_COLS: usize = 200;

/// The look sets its own title and this module leaves it alone.
static OWN_TITLE: AtomicBool = AtomicBool::new(false);
/// The title that was there before is on the terminal's title stack.
static TITLE_PUSHED: AtomicBool = AtomicBool::new(false);
/// A report went out and has to be withdrawn on the way out.
static REPORTED: AtomicBool = AtomicBool::new(false);

/// For a look whose title is part of what it imitates. Call before the event
/// loop starts.
pub fn keep_own_title() {
    OWN_TITLE.store(true, Ordering::Relaxed);
}

/// Whether this terminal reads status reports. The environment does not
/// change under a running process, so it is read once.
fn supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        let set = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
        reads_status(
            std::env::var("TERM_PROGRAM").ok().as_deref(),
            set("REX_BLOCK"),
            set("TMUX") || set("STY"),
        )
    })
}

/// Rex names itself in `TERM_PROGRAM` and names the block in `REX_BLOCK`. A
/// multiplexer running inside the block drops sequences it does not know, so
/// a report sent through one never arrives.
fn reads_status(term_program: Option<&str>, rex_block: bool, nested_mux: bool) -> bool {
    !nested_mux && (rex_block || term_program == Some("rex"))
}

/// One line that cannot start or continue a control sequence.
fn plain(text: &str, cols: usize) -> String {
    let text = crate::width::plain_text(text).replace('\n', " ");
    crate::width::truncate(text.trim(), cols)
}

fn report(status: &Status) -> String {
    let (state, kind) = match status.state {
        State::Idle => ("idle", None),
        State::Working => ("working", None),
        State::Blocked(Kind::Permission) => ("blocked", Some("permission")),
        State::Blocked(Kind::Question) => ("blocked", Some("question")),
        State::Done => ("done", None),
        State::Error => ("error", None),
    };
    let mut body = format!("state={state}:app={APP_NAME}");
    if let Some(kind) = kind {
        body.push_str(":kind=");
        body.push_str(kind);
    }
    let msg = plain(&status.msg, MSG_COLS);
    if !msg.is_empty() {
        body.push_str(":msg=");
        body.push_str(&base64::engine::general_purpose::STANDARD.encode(msg));
    }
    format!("\x1b]7501;{body}\x1b\\")
}

/// Everything `status` needs written, as one string so it reaches the
/// terminal in one piece.
fn sequences(status: &Status, title: bool, reports: bool, pushed: bool) -> String {
    let mut out = String::new();
    let name = plain(&status.title, TITLE_COLS);
    if title && !name.is_empty() {
        if !pushed {
            out.push_str("\x1b[22;0t");
        }
        out.push_str(&format!("\x1b]2;{name}\x07"));
    }
    if reports {
        out.push_str(&report(status));
    }
    out
}

/// Write the newest status, if one was posted since the last call.
pub(crate) fn flush() {
    let Some(status) = agent_core::status::take() else {
        return;
    };
    let title = !OWN_TITLE.load(Ordering::Relaxed);
    let pushed = TITLE_PUSHED.load(Ordering::Relaxed);
    let out = sequences(&status, title, supported(), pushed);
    if out.is_empty() {
        return;
    }
    TITLE_PUSHED.fetch_or(out.starts_with("\x1b[22;0t"), Ordering::Relaxed);
    REPORTED.fetch_or(supported(), Ordering::Relaxed);
    let mut stdout = io::stdout();
    let _ = stdout.write_all(out.as_bytes());
    let _ = stdout.flush();
}

/// Withdraw the report and put the title back. Whatever has the terminal
/// next is not this session working.
pub(crate) fn clear() {
    let mut out = String::new();
    if REPORTED.swap(false, Ordering::Relaxed) {
        out.push_str("\x1b]7501;state=clear\x1b\\");
    }
    if TITLE_PUSHED.swap(false, Ordering::Relaxed) {
        // Emptied first: a terminal with no title stack ignores the pop, and no title beats
        // a stale one.
        out.push_str("\x1b]2;\x07\x1b[23;0t");
    }
    if out.is_empty() {
        return;
    }
    let mut stdout = io::stdout();
    let _ = stdout.write_all(out.as_bytes());
    let _ = stdout.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(state: State, msg: &str, title: &str) -> Status {
        Status {
            state,
            msg: msg.to_string(),
            title: title.to_string(),
        }
    }

    #[test]
    fn a_report_goes_only_where_rex_will_read_it() {
        assert!(reads_status(Some("rex"), false, false));
        assert!(reads_status(None, true, false));
        assert!(!reads_status(Some("ghostty"), false, false));
        assert!(!reads_status(None, false, false));
        // tmux inside a Rex block inherits REX_BLOCK and swallows the report.
        assert!(!reads_status(Some("tmux"), true, true));
    }

    #[test]
    fn a_report_names_the_app_and_encodes_its_message() {
        assert_eq!(
            report(&status(State::Blocked(Kind::Permission), "cargo test", "")),
            "\x1b]7501;state=blocked:app=wizard:kind=permission:msg=Y2FyZ28gdGVzdA==\x1b\\"
        );
        assert_eq!(
            report(&status(State::Done, "", "")),
            "\x1b]7501;state=done:app=wizard\x1b\\"
        );
    }

    #[test]
    fn the_old_title_is_saved_once_and_the_new_one_cannot_end_its_sequence() {
        let s = status(State::Working, "", "fix\x07 the\x1b]2;x parser");
        let first = sequences(&s, true, false, false);
        assert!(first.starts_with("\x1b[22;0t\x1b]2;fix the"), "{first:?}");
        assert_eq!(first.matches('\x07').count(), 1, "{first:?}");
        assert_eq!(first.matches('\x1b').count(), 2, "{first:?}");
        assert!(!sequences(&s, true, false, true).contains("\x1b[22;0t"));
    }

    #[test]
    fn a_session_with_no_title_yet_leaves_the_title_alone() {
        let s = status(State::Idle, "", "");
        assert_eq!(sequences(&s, true, false, false), "");
        assert_eq!(
            sequences(&s, true, true, false),
            "\x1b]7501;state=idle:app=wizard\x1b\\"
        );
    }

    #[test]
    fn a_look_with_its_own_title_gets_only_the_report() {
        let s = status(State::Done, "", "fix the parser");
        assert_eq!(
            sequences(&s, false, true, false),
            "\x1b]7501;state=done:app=wizard\x1b\\"
        );
    }
}
