//! The rewind panel: what going back to a user message would undo, and the choice of how far.
//! It docks where the composer is. The backend answers a preview with the files that would be
//! put back; a backend that cannot rewind never answers, and then the only choice left is to
//! take the message back into the composer.

use std::time::{Duration, Instant};

use agent_core::RewindPreview;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use tuikit::paint::{fill, put_str};
use tuikit::width::spans_width;

use crate::ui::row::{cut, sp, spaces, Cx};

/// How long to wait for the backend's preview before saying it cannot rewind.
const WAIT: Duration = Duration::from_millis(1500);

#[derive(Debug, PartialEq, Eq)]
pub enum RwOut {
    None,
    Cancel,
    /// Do it. `edit` puts the message back in the composer.
    Do {
        conversation: bool,
        files: bool,
        edit: bool,
    },
}

#[derive(Clone, Debug)]
struct Opt {
    label: &'static str,
    detail: String,
    enabled: bool,
    conversation: bool,
    files: bool,
    edit: bool,
}

pub struct RewindPanel {
    pub turn: usize,
    pub text: String,
    pub preview: Option<RewindPreview>,
    pub opened: Instant,
    cwd: String,
    cursor: usize,
}

fn files_text(p: &RewindPreview, cwd: &str) -> String {
    let n = p.files.len();
    let names: Vec<String> = p
        .files
        .iter()
        .take(2)
        .map(|f| {
            // Inside the project a path is relative to it; outside, just the file name.
            let rel = f
                .strip_prefix(cwd)
                .map(|r| r.trim_start_matches('/'))
                .filter(|r| !cwd.is_empty() && !r.is_empty())
                .unwrap_or_else(|| f.rsplit('/').next().unwrap_or(f));
            tuikit::width::truncate_left(rel, 28)
        })
        .collect();
    let mut s = format!("{n} {}", if n == 1 { "file" } else { "files" });
    if !names.is_empty() {
        s.push_str(&format!(" ({}", names.join(", ")));
        if n > 2 {
            s.push_str(&format!(", +{}", n - 2));
        }
        s.push(')');
    }
    if p.insertions + p.deletions > 0 {
        s.push_str(&format!(", +{} -{}", p.insertions, p.deletions));
    }
    s
}

impl RewindPanel {
    pub fn new(turn: usize, text: String, now: Instant) -> RewindPanel {
        RewindPanel {
            turn,
            text,
            preview: None,
            opened: now,
            cwd: String::new(),
            cursor: 0,
        }
    }

    pub fn with_cwd(mut self, cwd: &str) -> RewindPanel {
        self.cwd = cwd.to_string();
        self
    }

    pub fn set_preview(&mut self, p: RewindPreview) {
        self.preview = Some(p);
        self.cursor = 0;
        // Land on the first thing that can be chosen.
        if let Some(i) = self.options().iter().position(|o| o.enabled) {
            self.cursor = i;
        }
    }

    fn options(&self) -> Vec<Opt> {
        let edit = Opt {
            label: "just edit this message",
            detail: "nothing is undone; the text goes to the composer".into(),
            enabled: true,
            conversation: false,
            files: false,
            edit: true,
        };
        let Some(p) = &self.preview else {
            return vec![edit];
        };
        let files = p.can_rewind_files;
        let conv = p.can_rewind_conversation;
        let ft = if files {
            format!("put back {}", files_text(p, &self.cwd))
        } else {
            "no file snapshot for this message".to_string()
        };
        vec![
            Opt {
                label: "conversation and files",
                detail: if files && conv {
                    format!("drop what came after, {ft}")
                } else if conv {
                    "drop what came after; files have no snapshot".to_string()
                } else {
                    "not possible here".to_string()
                },
                enabled: conv,
                conversation: true,
                files,
                edit: true,
            },
            Opt {
                label: "conversation only",
                detail: if conv {
                    "drop what came after, keep the files as they are".into()
                } else {
                    "the conversation cannot be cut at this message".into()
                },
                enabled: conv,
                conversation: true,
                files: false,
                edit: true,
            },
            Opt {
                label: "files only",
                detail: ft,
                enabled: files,
                conversation: false,
                files: true,
                edit: false,
            },
            edit,
        ]
    }

    fn waiting(&self, now: Instant) -> bool {
        self.preview.is_none() && now.saturating_duration_since(self.opened) < WAIT
    }

    pub fn height(&self) -> u16 {
        let n = self.options().len();
        (1 + 1 + n + 1) as u16
    }

    pub fn on_key(&mut self, key: KeyEvent) -> RwOut {
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return RwOut::None;
        }
        let opts = self.options();
        let n = opts.len();
        let pick = |i: usize| -> RwOut {
            match opts.get(i) {
                Some(o) if o.enabled => RwOut::Do {
                    conversation: o.conversation,
                    files: o.files,
                    edit: o.edit,
                },
                _ => RwOut::None,
            }
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('n' | 'q') => RwOut::Cancel,
            KeyCode::Up | KeyCode::Char('k') => {
                self.cursor = (self.cursor + n - 1) % n;
                RwOut::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.cursor = (self.cursor + 1) % n;
                RwOut::None
            }
            KeyCode::Enter => pick(self.cursor),
            // The number keys count the full list; with no preview only the last exists.
            KeyCode::Char(c @ '1'..='4') => {
                let i = c as usize - '1' as usize;
                if n == 1 {
                    if i == 3 {
                        pick(0)
                    } else {
                        RwOut::None
                    }
                } else {
                    pick(i)
                }
            }
            _ => RwOut::None,
        }
    }

    /// Paint into `rect`.
    pub fn draw(&self, buf: &mut Buffer, rect: Rect, cx: &Cx) {
        let p = cx.p;
        if rect.height < 4 {
            return;
        }
        let w = rect.width as usize;
        let put = |buf: &mut Buffer,
                   y: u16,
                   spans: &[Span<'static>],
                   bg: Option<ratatui::style::Color>| {
            if let Some(bg) = bg {
                fill(
                    buf,
                    Rect::new(rect.x, y, rect.width, 1),
                    Style::new().bg(bg),
                );
            }
            let mut x = rect.x;
            for s in spans {
                let st = match bg {
                    Some(b) => Style::new().bg(b).patch(s.style),
                    None => s.style,
                };
                x = put_str(buf, x, y, &s.content, st, rect);
            }
        };
        let mut spans = vec![
            sp(format!("{} ", cx.g.rule), p.s_accent()),
            sp("Rewind", p.s_accent().add_modifier(Modifier::BOLD)),
            sp(" ", p.s_accent()),
        ];
        let used = spans_width(&spans);
        spans.push(sp(cx.g.rule.repeat(w.saturating_sub(used)), p.s_accent()));
        put(buf, rect.y, &spans, None);
        let one: String = self.text.split_whitespace().collect::<Vec<_>>().join(" ");
        put(
            buf,
            rect.y + 1,
            &[
                spaces(2),
                sp("to just before  ", p.s_faint()),
                sp(
                    format!("\"{}\"", cut(&one, w.saturating_sub(24))),
                    p.s_text(),
                ),
            ],
            None,
        );
        let opts = self.options();
        let label_w = opts.iter().map(|o| o.label.len()).max().unwrap_or(0);
        let numbered = opts.len() > 1;
        for (i, o) in opts.iter().enumerate() {
            let y = rect.y + 2 + i as u16;
            if y >= rect.bottom() - 1 {
                break;
            }
            let on = i == self.cursor;
            let key = if numbered { i + 1 } else { 4 };
            let (kst, lst, dst) = if o.enabled {
                (
                    p.s_text().add_modifier(Modifier::BOLD),
                    if on {
                        p.s_text().add_modifier(Modifier::BOLD)
                    } else {
                        p.s_text()
                    },
                    p.s_dim(),
                )
            } else {
                (p.s_faint(), p.s_faint(), p.s_faint())
            };
            let mut line = vec![
                spaces(1),
                sp(
                    if on { cx.g.select } else { " " },
                    p.s_accent().add_modifier(Modifier::BOLD),
                ),
                spaces(1),
                sp(format!("{key}"), kst),
                spaces(2),
                sp(format!("{:<label_w$}", o.label), lst),
                spaces(2),
            ];
            let left = spans_width(&line);
            line.push(sp(cut(&o.detail, w.saturating_sub(left + 1)), dst));
            put(buf, y, &line, on.then_some(p.raised));
        }
        let hint_y = rect.bottom() - 1;
        let mut hint = vec![spaces(2)];
        let arrows = format!("{}{}", cx.g.up, cx.g.down);
        for (k, l) in [
            (arrows.as_str(), "move"),
            ("enter", "choose"),
            ("esc", "cancel"),
        ] {
            hint.push(sp(k.to_string(), p.s_text().add_modifier(Modifier::BOLD)));
            hint.push(sp(format!(" {l}"), p.s_dim()));
            hint.push(spaces(3));
        }
        if self.preview.is_none() {
            hint.push(sp(
                format!("asking claude what would change{}", cx.g.ellipsis),
                p.s_faint(),
            ));
        }
        put(buf, hint_y, &hint, None);
    }

    /// When the "no answer" state starts, so the loop can wake and redraw.
    pub fn wake_at(&self, now: Instant) -> Option<Instant> {
        self.waiting(now).then_some(self.opened + WAIT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::{Depth, Kind, Palette, UNICODE};
    use tuikit::testing::TestTerminal;

    fn preview() -> RewindPreview {
        RewindPreview {
            turn: 1,
            can_rewind_files: true,
            files: vec![
                "/w/src/main.rs".into(),
                "/w/notes.txt".into(),
                "/w/c.rs".into(),
            ],
            insertions: 4,
            deletions: 1,
            can_rewind_conversation: true,
            note: String::new(),
        }
    }

    fn k(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn without_a_preview_only_editing_the_message_is_offered() {
        let mut r = RewindPanel::new(1, "fix the test".into(), Instant::now());
        assert_eq!(r.options().len(), 1);
        assert_eq!(
            r.on_key(k('4')),
            RwOut::Do {
                conversation: false,
                files: false,
                edit: true
            }
        );
        assert_eq!(r.on_key(k('1')), RwOut::None);
        assert_eq!(
            r.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            RwOut::Cancel
        );
    }

    #[test]
    fn the_four_choices_map_to_what_gets_undone() {
        let mut r = RewindPanel::new(1, "fix the test".into(), Instant::now());
        r.set_preview(preview());
        assert_eq!(
            r.on_key(k('1')),
            RwOut::Do {
                conversation: true,
                files: true,
                edit: true
            }
        );
        assert_eq!(
            r.on_key(k('2')),
            RwOut::Do {
                conversation: true,
                files: false,
                edit: true
            }
        );
        assert_eq!(
            r.on_key(k('3')),
            RwOut::Do {
                conversation: false,
                files: true,
                edit: false
            }
        );
    }

    #[test]
    fn a_missing_snapshot_greys_out_the_file_choices() {
        let mut r = RewindPanel::new(0, "first".into(), Instant::now());
        let mut p = preview();
        p.can_rewind_files = false;
        p.files.clear();
        r.set_preview(p);
        assert_eq!(r.on_key(k('3')), RwOut::None, "files only is not possible");
        assert!(matches!(r.on_key(k('2')), RwOut::Do { files: false, .. }));
    }

    #[test]
    fn panel_names_the_files_it_would_put_back() {
        let pal = Palette::new(Kind::Hearth, Depth::True);
        let th = pal.theme();
        let cx = Cx {
            p: &pal,
            theme: &th,
            g: &UNICODE,
            width: 100,
            detail: false,
            now: Instant::now(),
            spin: 0,
        };
        let mut r = RewindPanel::new(1, "fix the wrap test".into(), Instant::now()).with_cwd("/w");
        r.set_preview(preview());
        let mut t = TestTerminal::new(100, r.height());
        t.draw(|b, a| r.draw(b, a, &cx));
        let s = t.plain();
        assert!(
            s.contains("Rewind") && s.contains("fix the wrap test"),
            "{s}"
        );
        assert!(
            s.contains("3 files (src/main.rs, notes.txt, +1), +4 -1"),
            "{s}"
        );
    }
}
