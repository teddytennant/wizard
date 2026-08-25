//! The agent core the native window is built on: session ownership, the
//! config store, git, and OAuth sign-in.
//!
//! This module used to be a second *surface* — an axum server on
//! `127.0.0.1:<port>` serving a JavaScript page over a WebSocket. That surface
//! is gone (see `docs/native-gui.md`); what is left is everything under it that
//! was never about HTTP:
//!
//! - [`tasks`]: the multi-session manager. One lazily-built
//!   [`Agent`](crate::agent::Agent) per chat, each on its own worker, with the
//!   plan/interview gates and the registry heartbeat.
//! - [`settings`]: a config store that re-reads `~/.wizard/config.toml` on
//!   every read, so an edit made in the window and an edit made in the TUI both
//!   land without a restart.
//! - [`git`]: status, diffs and branch switching for the window's git rail.
//! - [`oauth`]: the subscription sign-in flow.
//! - [`command`]: the one place a slash command is applied to a chat's live
//!   agent.
//!
//! It is compiled only with `--features native`, because the window is the
//! only thing that reaches it, and it moved under `src/plugins/` with the
//! window when the window became a plugin. It stays a *sibling* of
//! [`native`](crate::plugins::native) rather than a module inside it, and the
//! name is kept, because none of it draws anything: it is the half of the GUI
//! that would survive another front end being written against it, and nesting
//! it would say the window owns it. It registers nothing with the kernel —
//! [`crate::plugins::native::plugin`] is the one registration the pair makes.

pub(crate) mod command;
pub(crate) mod git;
pub(crate) mod oauth;
pub(crate) mod settings;
pub(crate) mod tasks;

/// Open `url` in the user's browser, best-effort: a missing opener must
/// never fail the caller, so errors are logged and dropped.
///
/// The one thing here that still involves a browser: an OAuth authorize URL
/// has to open in one, because that is where the provider's consent screen
/// lives.
pub(crate) fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    if let Err(err) = std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        tracing::warn!("could not open the browser via {opener}: {err}");
    }
}
