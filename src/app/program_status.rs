//! What the terminal shows for this session outside the screen itself: the
//! window title, and a Program Status Protocol report (OSC 7501).
//!
//! A terminal that reads the protocol, Rex being the one that defined it,
//! draws the report on the tab: a moving indicator while a turn runs, a mark
//! when it finishes, another when a permission prompt is waiting. Without it
//! a Wizard tab is indistinguishable from a shell sitting at its prompt.
//!
//! The title goes to every terminal. The report goes only where the
//! environment says it will be read. Asking the terminal would be the sure
//! way, but a query blocks on a reply that may never come, and
//! [`setup_terminal`](super::term::setup_terminal) says what that costs.
//!
//! A report is a set of `key=value` fields joined by colons. Free text is
//! base64, so nothing a user typed can end the sequence early.

use std::io::Write;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine;

use crate::session_registry::SessionState;
use crate::transcript::TranscriptItem;

use super::App;

/// The name reports are filed under. A terminal picks the tab's icon from it.
const APP_NAME: &str = "wizard";

/// Title of a session nobody has said anything in yet.
const UNTITLED: &str = "Wizard";

/// Longest title, in characters. Tabs are narrow; this matches the session
/// name the dashboard shows.
const TITLE_CHARS: usize = 48;

/// Longest status message, in characters. The protocol allows 2048 bytes of
/// it, but it is drawn on one line.
const MSG_CHARS: usize = 200;

/// How many times the terminal has been set up. Bumped by [`armed`], so a
/// [`Reporter`] can tell that what it last sent was wiped by a teardown (an
/// editor suspension, the panic hook) and has to be sent again.
static ARMED: AtomicU64 = AtomicU64::new(0);

/// Whether this terminal reads status reports. The environment does not
/// change under a running process, so it is read once.
fn supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        let set = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
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

/// The terminal was just set up: save the title it had, so [`disarmed`] can
/// put it back.
pub(super) fn armed(out: &mut impl Write) {
    ARMED.fetch_add(1, Ordering::Relaxed);
    let _ = out.write_all(b"\x1b[22;2t");
}

/// The terminal is being handed back: withdraw the report and restore the
/// title. Whatever runs next (the shell, `$EDITOR`) is not Wizard working.
///
/// The title is emptied before the old one is popped back. A terminal with no
/// title stack ignores the pop, and no title beats a stale one.
pub(super) fn disarmed(out: &mut impl Write) {
    if supported() {
        let _ = out.write_all(b"\x1b]7501;state=clear\x1b\\");
    }
    let _ = out.write_all(b"\x1b]2;\x1b\\\x1b[23;2t");
}

/// What a program can say it is doing. The names are the protocol's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Nothing has run yet.
    Idle,
    Working,
    /// Waiting on the person at the keyboard.
    Blocked(Kind),
    /// A turn ran and has ended.
    Done,
    Error,
}

/// What a blocked program is waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Permission,
    Question,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Report {
    state: State,
    msg: String,
}

impl Report {
    fn sequence(&self) -> String {
        let (state, kind) = match self.state {
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
        if !self.msg.is_empty() {
            body.push_str(":msg=");
            body.push_str(&base64::engine::general_purpose::STANDARD.encode(&self.msg));
        }
        format!("\x1b]7501;{body}\x1b\\")
    }
}

/// One line of at most `max` characters, with every control character turned
/// into a space. A title is sent raw, and a BEL or ESC inside it would end the
/// sequence and leave the rest to be read as commands.
fn plain(text: &str, max: usize) -> String {
    text.lines()
        .next()
        .unwrap_or("")
        .trim()
        .chars()
        .take(max)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// The title for this session: the first thing the user asked for.
fn title(app: &App) -> String {
    app.transcript
        .iter()
        .find_map(|item| match item {
            TranscriptItem::User { text, .. } => Some(text),
            TranscriptItem::TurnMarker { prompt, .. } => Some(prompt),
            _ => None,
        })
        .map(|text| plain(text, TITLE_CHARS))
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| UNTITLED.to_string())
}

/// Sends the title and the status report when either changes.
pub(super) struct Reporter {
    supported: bool,
    /// Whether a turn has run. Tells a finished session from a fresh one;
    /// both are idle as far as [`App::session_state`] goes.
    ran: bool,
    /// What the terminal was last told, and under which [`ARMED`] count.
    sent: Option<(u64, String, Report)>,
}

impl Reporter {
    pub(super) fn new() -> Self {
        Self {
            supported: supported(),
            ran: false,
            sent: None,
        }
    }

    fn report(&mut self, app: &App) -> Report {
        let state = match app.session_state() {
            SessionState::NeedsInput if !app.permission_prompts.is_empty() => {
                State::Blocked(Kind::Permission)
            }
            SessionState::NeedsInput => State::Blocked(Kind::Question),
            SessionState::Working => State::Working,
            SessionState::Idle if self.ran => State::Done,
            SessionState::Idle => State::Idle,
            // Only a background run ends in these, and the TUI is not one.
            SessionState::Completed => State::Done,
            SessionState::Failed => State::Error,
        };
        self.ran |= !matches!(state, State::Idle);
        let msg = match state {
            State::Working | State::Blocked(_) => plain(&app.session_activity(), MSG_CHARS),
            State::Idle | State::Done | State::Error => String::new(),
        };
        Report { state, msg }
    }

    /// Bring the terminal up to date with `app`. Writes nothing when nothing
    /// changed, which is every frame but a handful.
    pub(super) fn sync(&mut self, app: &App, out: &mut impl Write) {
        let now = (ARMED.load(Ordering::Relaxed), title(app), self.report(app));
        if self.sent.as_ref() == Some(&now) {
            return;
        }
        let _ = write!(out, "\x1b]2;{}\x1b\\", now.1);
        if self.supported {
            let _ = out.write_all(now.2.sequence().as_bytes());
        }
        let _ = out.flush();
        self.sent = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reporter() -> Reporter {
        Reporter {
            supported: true,
            ran: false,
            sent: None,
        }
    }

    fn synced(reporter: &mut Reporter, app: &App) -> String {
        let mut out = Vec::new();
        reporter.sync(app, &mut out);
        String::from_utf8(out).unwrap()
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
        let report = Report {
            state: State::Blocked(Kind::Permission),
            msg: "waiting for permission".to_string(),
        };
        assert_eq!(
            report.sequence(),
            "\x1b]7501;state=blocked:app=wizard:kind=permission:\
             msg=d2FpdGluZyBmb3IgcGVybWlzc2lvbg==\x1b\\"
        );
        let done = Report {
            state: State::Done,
            msg: String::new(),
        };
        assert_eq!(done.sequence(), "\x1b]7501;state=done:app=wizard\x1b\\");
    }

    #[test]
    fn a_title_cannot_end_its_own_sequence() {
        assert_eq!(
            plain("fix\x07 the\x1b]2;x parser\nsecond line", 48),
            "fix  the ]2;x parser"
        );
        assert_eq!(plain("  padded  ", 3), "pad");
    }

    #[test]
    fn a_session_is_idle_then_working_then_done() {
        let mut app = App::new(crate::config::Config::default());
        let mut reporter = reporter();

        let first = synced(&mut reporter, &app);
        assert!(first.starts_with("\x1b]2;Wizard\x1b\\"), "{first:?}");
        assert!(first.contains("state=idle:app=wizard\x1b\\"), "{first:?}");
        assert_eq!(
            synced(&mut reporter, &app),
            "",
            "an unchanged frame writes nothing"
        );

        app.status.busy = true;
        assert!(synced(&mut reporter, &app).contains("state=working:app=wizard:msg="));

        app.status.busy = false;
        assert!(synced(&mut reporter, &app).contains("state=done:app=wizard\x1b\\"));
    }

    #[test]
    fn a_torn_down_terminal_is_told_again() {
        let app = App::new(crate::config::Config::default());
        let mut reporter = reporter();
        assert!(!synced(&mut reporter, &app).is_empty());

        // An editor suspension: the report is cleared and the title popped,
        // then the terminal is set up again.
        let mut out = Vec::new();
        disarmed(&mut out);
        armed(&mut out);
        assert!(synced(&mut reporter, &app).contains("state=idle"));
    }

    #[test]
    fn a_terminal_that_does_not_read_reports_still_gets_the_title() {
        let app = App::new(crate::config::Config::default());
        let mut reporter = Reporter {
            supported: false,
            ..reporter()
        };
        assert_eq!(synced(&mut reporter, &app), "\x1b]2;Wizard\x1b\\");
    }
}
