// OWNER: welcome (the New Worktree dialog, spec 2.15 and the real `new_worktree_dialog.rs`)
//! `Ctrl+W` on the welcome screen: a small rounded dialog asking for an optional name. `Enter`
//! runs `git worktree add --detach` into grokw's own worktree directory and starts a new session
//! there; wizard has no worktree support, so the checkout and the backend restart are grokw's.

use std::path::{Path, PathBuf};

use agent_core::Request;
use ratatui::buffer::Buffer;
use ratatui::style::Style;
use unicode_width::UnicodeWidthStr;

use super::welcome::{Severity, StartupWarning};
use super::{bold, put, st};
use crate::app::App;

const MIN_WIDTH: u16 = 50;
const HEIGHT: u16 = 5;
const INNER_PAD: u16 = 4;
const LABEL: &str = "Name (optional): ";

#[derive(Clone, Debug, Default)]
pub struct Dialog {
    pub label: String,
}

/// Width that fits the typed name, clamped to the screen less a margin of two on each side.
pub fn dialog_width(screen_w: u16, label: &str) -> u16 {
    let max = screen_w.saturating_sub(4);
    let needed = (LABEL.width() + label.width() + 1 + INNER_PAD as usize) as u16;
    needed.max(MIN_WIDTH).min(max)
}

pub fn draw(buf: &mut Buffer, app: &App) {
    let Some(d) = app.home.worktree.as_ref() else {
        return;
    };
    let th = &app.theme;
    let (w, h) = (buf.area.width, buf.area.height);
    if h < HEIGHT || w < 20 {
        // too small for the box: say that it is there, and how to leave
        if h >= 1 && w >= 16 {
            put(buf, 0, 0, "[Esc] to close", st(th.gray_dim));
        }
        return;
    }
    let dw = dialog_width(w, &d.label);
    let x = w.saturating_sub(dw) / 2;
    let y = h.saturating_sub(HEIGHT) / 2;
    let bg = Style::new().bg(th.bg_dark);
    for yy in y..y + HEIGHT {
        for xx in x..x + dw {
            buf[(xx, yy)].set_symbol(" ").set_style(bg);
        }
    }
    let b = st(th.gray_dim).bg(th.bg_dark);
    put(buf, x, y, "╭", b);
    put(buf, x + dw - 1, y, "╮", b);
    put(buf, x, y + HEIGHT - 1, "╰", b);
    put(buf, x + dw - 1, y + HEIGHT - 1, "╯", b);
    for xx in x + 1..x + dw - 1 {
        put(buf, xx, y, "─", b);
        put(buf, xx, y + HEIGHT - 1, "─", b);
    }
    for yy in y + 1..y + HEIGHT - 1 {
        put(buf, x, yy, "│", b);
        put(buf, x + dw - 1, yy, "│", b);
    }
    let ix = x + 2;
    let iw = dw.saturating_sub(INNER_PAD) as usize;
    put(
        buf,
        ix,
        y + 1,
        "New Worktree",
        bold(th.text_primary).bg(th.bg_dark),
    );
    let x2 = put(buf, ix, y + 2, LABEL, st(th.gray_bright).bg(th.bg_dark));
    // the input scrolls to keep its end in view
    let room = iw.saturating_sub(LABEL.width());
    let shown: String = if d.label.width() > room {
        let mut cut = String::new();
        let mut used = 0;
        for c in d.label.chars().rev() {
            let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            if used + cw > room.saturating_sub(1) {
                break;
            }
            cut.insert(0, c);
            used += cw;
        }
        cut
    } else {
        d.label.clone()
    };
    let end = put(buf, x2, y + 2, &shown, st(th.text_primary).bg(th.bg_dark));
    if room > 0 {
        // the caret is one inverted cell
        put(
            buf,
            end,
            y + 2,
            " ",
            Style::new().fg(th.bg_dark).bg(th.text_primary),
        );
    }
    let hx = put(
        buf,
        ix,
        y + 3,
        "enter",
        bold(th.text_secondary).bg(th.bg_dark),
    );
    let hx = put(buf, hx, y + 3, " = create   ", st(th.gray).bg(th.bg_dark));
    let hx = put(
        buf,
        hx,
        y + 3,
        "esc",
        bold(th.text_secondary).bg(th.bg_dark),
    );
    put(buf, hx, y + 3, " = cancel", st(th.gray).bg(th.bg_dark));
}

/// The root of the git repository `dir` is in, if it is in one.
pub fn repo_root(dir: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(dir)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
}

/// Where the worktrees of `repo` live.
pub fn worktree_home(state_dir: Option<&Path>, repo: &Path) -> PathBuf {
    let name = repo
        .file_name()
        .map_or_else(|| "repo".to_string(), |n| n.to_string_lossy().to_string());
    state_dir
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state/grokw")))
        .unwrap_or_else(std::env::temp_dir)
        .join("worktrees")
        .join(name)
}

/// A directory name from what was typed, or a time-based one.
pub fn dir_name(label: &str) -> String {
    let clean: String = label
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let clean = clean.trim_matches('-').to_string();
    if clean.is_empty() {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        format!("wt-{secs:x}")
    } else {
        clean
    }
}

/// `git worktree add --detach <home>/<name> HEAD` from inside the repository.
pub fn create(repo: &Path, home: &Path, label: &str) -> Result<PathBuf, String> {
    let dir = home.join(dir_name(label));
    if dir.exists() {
        return Err(format!("{} already exists", dir.display()));
    }
    std::fs::create_dir_all(home).map_err(|e| e.to_string())?;
    let out = std::process::Command::new("git")
        .args(["worktree", "add", "--detach"])
        .arg(&dir)
        .arg("HEAD")
        .current_dir(repo)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(dir)
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(err
            .lines()
            .next()
            .unwrap_or("git failed")
            .trim()
            .to_string())
    }
}

fn banner(app: &mut App, message: String) {
    app.home
        .warnings
        .retain(|w| !w.message.starts_with("Cannot create worktree"));
    app.home.warnings.push(StartupWarning {
        severity: Severity::Warning,
        message,
        action: None,
    });
}

impl App {
    /// `Ctrl+W` and the `New worktree` row: the dialog inside a repository, a banner outside.
    pub fn open_worktree(&mut self) {
        if repo_root(&self.opts.cwd).is_none() {
            banner(
                self,
                "Not inside a git repository. Navigate to a git repo or run 'git init' first."
                    .into(),
            );
            return;
        }
        self.home
            .warnings
            .retain(|w| !w.message.starts_with("Not inside a git"));
        self.home.worktree = Some(Dialog::default());
    }

    /// `Enter` in the dialog: make the checkout and move the session into it.
    pub fn create_worktree(&mut self) {
        let Some(d) = self.home.worktree.take() else {
            return;
        };
        let Some(repo) = repo_root(&self.opts.cwd) else {
            banner(self, "Cannot create worktree: not a git repository".into());
            return;
        };
        let home = worktree_home(self.opts.state_dir.as_deref(), &repo);
        match create(&repo, &home, &d.label) {
            Ok(dir) => self.switch_to_dir(dir),
            Err(e) => banner(self, format!("Cannot create worktree: {e}")),
        }
    }

    /// Start a new backend in `dir` and make it the session. Wizard fixes its directory when it
    /// starts, so the old one is asked to shut down and a new one takes its place.
    pub fn switch_to_dir(&mut self, dir: PathBuf) {
        // a test, or a process without a runtime, has no backend to start
        if tokio::runtime::Handle::try_current().is_ok() {
            match crate::app::spawn_backend(dir.clone(), None, self.opts.mock, self.msg_tx.clone())
            {
                Ok((tx, _fwd)) => {
                    let old = std::mem::replace(&mut self.tx, tx);
                    let _ = old.send(Request::Shutdown);
                }
                Err(e) => {
                    banner(self, format!("Cannot create worktree: {e}"));
                    return;
                }
            }
        }
        self.opts.cwd = dir.clone();
        self.config.cwd = dir.display().to_string();
        self.branch = crate::app::git_branch(&dir);
        self.files.clear();
        self.tr = agent_core::transcript::Transcript::new();
        self.view.reset();
        self.stamps.clear();
        self.queue.clear();
        self.ed.clear();
        self.screen = crate::app::Screen::Session;
        self.focus = crate::app::Focus::Prompt;
        self.home.warnings.clear();
        self.toast(format!("✓ Worktree: {}", dir.display()));
    }
}
