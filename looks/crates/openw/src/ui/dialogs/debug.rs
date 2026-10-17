// OWNER: dialogs
//! `/debug`: version, clock, OS, terminal, session and model, with `enter` copying them.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use tuikit::dialog::LARGE;
use tuikit::paint::put_str;
use tuikit::width::{display_width, pad_right, wrap};
use tuikit::Theme;

use super::panel::frame;
use super::{Ctx, Dialog, Effect, Outcome};

pub struct DebugDialog {
    entries: Vec<(String, String)>,
    copied: bool,
}

/// `2026-10-05T17:24:25.280Z`.
pub fn iso(now_ms: i64) -> String {
    let secs = now_ms.div_euclid(1000);
    let ms = now_ms.rem_euclid(1000);
    let date = super::clock::date_string(secs, 0); // `Mon Oct 05 2026`
    let p: Vec<&str> = date.split(' ').collect();
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|m| *m == p[1])
    .map_or(1, |i| i + 1);
    let s = secs.rem_euclid(86_400);
    format!(
        "{}-{:02}-{}T{:02}:{:02}:{:02}.{:03}Z",
        p[3],
        month,
        p[2],
        s / 3600,
        (s % 3600) / 60,
        s % 60,
        ms
    )
}

fn describe_os() -> String {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        a => a,
    };
    let os = match std::env::consts::OS {
        "linux" => "Linux",
        "macos" => "Darwin",
        o => o,
    };
    #[cfg(unix)]
    {
        // SAFETY: `uname` fills the struct we hand it; the strings are NUL-terminated.
        unsafe {
            let mut u: libc::utsname = std::mem::zeroed();
            if libc::uname(&mut u) == 0 {
                let rel = std::ffi::CStr::from_ptr(u.release.as_ptr()).to_string_lossy();
                let rel = rel.split('-').next().unwrap_or("");
                return format!("{os} {rel} ({arch})");
            }
        }
    }
    format!("{os} ({arch})")
}

fn describe_terminal() -> String {
    let term = std::env::var("TERM_PROGRAM")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("TERM").ok())
        .unwrap_or_else(|| "unknown".into());
    if std::env::var_os("TMUX").is_some() && term != "tmux" {
        format!("{term} in tmux")
    } else {
        term
    }
}

pub fn entries(ctx: &Ctx, now_ms: i64) -> Vec<(String, String)> {
    vec![
        ("Version".into(), format!("{} (dev)", ctx.version)),
        ("Date".into(), iso(now_ms)),
        ("OS".into(), describe_os()),
        ("Terminal".into(), describe_terminal()),
        (
            "Session ID".into(),
            if ctx.in_session && !ctx.session_id.is_empty() {
                ctx.session_id.clone()
            } else {
                "n/a".into()
            },
        ),
        (
            "Model".into(),
            if ctx.config.model.is_empty() {
                "n/a".into()
            } else {
                ctx.config.model.clone()
            },
        ),
    ]
}

pub fn open(ctx: &Ctx) -> Box<dyn Dialog> {
    Box::new(DebugDialog {
        entries: entries(ctx, ctx.now * 1000),
        copied: false,
    })
}

impl Dialog for DebugDialog {
    fn title(&self) -> &str {
        "Debug"
    }

    fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Esc => Outcome::close(),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Outcome::close(),
            KeyCode::Enter => {
                self.copied = true;
                let text = self
                    .entries
                    .iter()
                    .map(|(k, v)| format!("{k}: {v}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                Outcome::stay().with(Effect::Copy(text)).with(Effect::Toast(
                    tuikit::theme::Variant::Info,
                    "Debug info copied to clipboard".into(),
                ))
            }
            _ => Outcome::stay(),
        }
    }

    fn draw(&mut self, buf: &mut Buffer, screen: Rect, theme: &Theme) -> Option<(u16, u16)> {
        let w = LARGE.min(screen.width.saturating_sub(2));
        let value_w = (w.saturating_sub(4) as usize).saturating_sub(11).max(1);
        let laid: Vec<(String, Vec<String>)> = self
            .entries
            .iter()
            .map(|(k, v)| (pad_right(k, 10), wrap(v, value_w)))
            .collect();
        let rows: u16 = laid.iter().map(|(_, l)| l.len().max(1) as u16).sum();
        // header, gap, entries, gap, footer, bottom padding
        let content_h = 1 + 1 + rows + 1 + 1 + 1;
        let (_, body) = frame(buf, screen, theme, LARGE, content_h, "Debug", "esc");
        let panel = theme.background_panel;
        let mut y = body.y;
        for (k, lines) in &laid {
            put_str(
                buf,
                body.x,
                y,
                k,
                Style::new().fg(theme.text_muted).bg(panel),
                body,
            );
            for l in lines {
                put_str(
                    buf,
                    body.x + 11,
                    y,
                    l,
                    Style::new().fg(theme.text).bg(panel),
                    body,
                );
                y += 1;
            }
            if lines.is_empty() {
                y += 1;
            }
        }
        y += 1;
        put_str(
            buf,
            body.x,
            y,
            "Share this when reporting an issue.",
            Style::new().fg(theme.text_muted).bg(panel),
            body,
        );
        let (word, color) = if self.copied {
            ("✓ copied", theme.success)
        } else {
            ("copy", theme.text)
        };
        let total = (display_width(word) + 1 + 5) as u16;
        let x = body.right().saturating_sub(total);
        let x = put_str(
            buf,
            x,
            y,
            word,
            Style::new()
                .fg(color)
                .bg(panel)
                .add_modifier(Modifier::BOLD),
            body,
        );
        put_str(
            buf,
            x,
            y,
            " enter",
            Style::new().fg(theme.text_muted).bg(panel),
            body,
        );
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_matches_js() {
        assert_eq!(iso(1_791_158_400_123), "2026-10-05T00:00:00.123Z");
    }
}
