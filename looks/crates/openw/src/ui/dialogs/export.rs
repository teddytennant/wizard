// OWNER: dialogs
//! `/export`: filename plus four options, then the transcript is written as Markdown.

use std::path::{Path, PathBuf};

use agent_core::transcript::{Part, Role, Transcript};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use tuikit::dialog::MEDIUM;
use tuikit::editor::{Editor, EditorStyle};
use tuikit::paint::{fill, put_str};
use tuikit::Theme;

use super::panel::frame;
use super::{Ctx, Dialog, Effect, Outcome};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    pub thinking: bool,
    pub tool_details: bool,
    pub assistant_metadata: bool,
    pub open_without_saving: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            thinking: true,
            tool_details: true,
            assistant_metadata: true,
            open_without_saving: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Filename,
    Thinking,
    ToolDetails,
    Metadata,
    Open,
}

const ORDER: [Field; 5] = [
    Field::Filename,
    Field::Thinking,
    Field::ToolDetails,
    Field::Metadata,
    Field::Open,
];

pub struct ExportDialog {
    editor: Editor,
    opts: Options,
    active: Field,
}

/// opencode names the file after the first eight characters of its `ses_` ids, which differ
/// from the first character on. Other backends' ids (wizard's are timestamps) share their
/// first eight characters for a whole month, so those are used whole.
pub fn default_filename(session_id: &str) -> String {
    let short: String = if session_id.starts_with("ses_") {
        session_id.chars().take(8).collect()
    } else {
        session_id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            .collect()
    };
    if short.is_empty() {
        "session.md".into()
    } else {
        format!("session-{short}.md")
    }
}

/// Write `text` to `name` (relative names are taken against `cwd`) without replacing a file
/// that is already there: a taken name gets `-2`, `-3` and so on before the extension.
/// Returns the path written.
pub fn write_new(cwd: &Path, name: &str, text: &str) -> std::io::Result<PathBuf> {
    use std::io::Write;
    let first = cwd.join(name);
    let (stem, ext) = match first.extension() {
        Some(e) => (
            first.with_extension("").to_string_lossy().into_owned(),
            format!(".{}", e.to_string_lossy()),
        ),
        None => (first.to_string_lossy().into_owned(), String::new()),
    };
    let mut path = first;
    for n in 2..1000 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                f.write_all(text.as_bytes())?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                path = PathBuf::from(format!("{stem}-{n}{ext}"));
            }
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "too many files with that name",
    ))
}

pub fn open(ctx: &Ctx) -> Box<dyn Dialog> {
    let mut editor = Editor::with_text(&default_filename(&ctx.session_id));
    editor.move_doc_end();
    Box::new(ExportDialog {
        editor,
        opts: ctx.export,
        active: Field::Filename,
    })
}

impl Dialog for ExportDialog {
    fn title(&self) -> &str {
        "Export Options"
    }

    fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => Outcome::close(),
            KeyCode::Char('c') if ctrl => Outcome::close(),
            KeyCode::Tab => {
                let i = ORDER.iter().position(|f| *f == self.active).unwrap_or(0);
                self.active = ORDER[(i + 1) % ORDER.len()];
                Outcome::stay()
            }
            KeyCode::Enter => {
                let path = self.editor.text().trim().to_string();
                if path.is_empty() {
                    return Outcome::stay();
                }
                Outcome::close_with(vec![Effect::Export {
                    path,
                    options: self.opts,
                }])
            }
            KeyCode::Char(' ') if self.active != Field::Filename => {
                match self.active {
                    Field::Thinking => self.opts.thinking = !self.opts.thinking,
                    Field::ToolDetails => self.opts.tool_details = !self.opts.tool_details,
                    Field::Metadata => self.opts.assistant_metadata = !self.opts.assistant_metadata,
                    Field::Open => self.opts.open_without_saving = !self.opts.open_without_saving,
                    Field::Filename => {}
                }
                Outcome::stay()
            }
            _ => {
                // The text area keeps focus whichever option is highlighted, as opencode's does.
                self.editor.apply_key(key);
                Outcome::stay()
            }
        }
    }

    fn handle_paste(&mut self, text: &str) -> Outcome {
        self.editor.paste(&text.replace(['\n', '\r'], " "));
        Outcome::stay()
    }

    fn draw(&mut self, buf: &mut Buffer, screen: Rect, theme: &Theme) -> Option<(u16, u16)> {
        // header, gap, [label, gap, 3-row area], gap, 4 options, gap, hint, bottom padding
        let content_h = 1 + 1 + 5 + 1 + 4 + 1 + 1 + 1;
        let (_, body) = frame(
            buf,
            screen,
            theme,
            MEDIUM,
            content_h,
            "Export Options",
            "esc",
        );
        let panel = theme.background_panel;
        put_str(
            buf,
            body.x,
            body.y,
            "Filename:",
            Style::new().fg(theme.text).bg(panel),
            body,
        );
        let area = Rect::new(body.x, body.y + 2, body.width, 3);
        let style = EditorStyle {
            text: Style::new().fg(theme.text).bg(panel),
            placeholder: Some((
                "Enter filename".into(),
                Style::new().fg(theme.text_muted).bg(panel),
            )),
        };
        let info = self.editor.render(area, buf, &style);
        let rows = [
            (Field::Thinking, self.opts.thinking, "Include thinking"),
            (
                Field::ToolDetails,
                self.opts.tool_details,
                "Include tool details",
            ),
            (
                Field::Metadata,
                self.opts.assistant_metadata,
                "Include assistant metadata",
            ),
            (
                Field::Open,
                self.opts.open_without_saving,
                "Open without saving",
            ),
        ];
        let mut y = body.y + 6;
        for (f, on, label) in rows {
            let active = self.active == f;
            let row = Rect::new(body.x, y, body.width, 1);
            if active {
                fill(buf, row, Style::new().bg(theme.background_element));
            }
            let bg = if active {
                theme.background_element
            } else {
                panel
            };
            put_str(
                buf,
                body.x + 1,
                y,
                if on { "[x]" } else { "[ ]" },
                Style::new()
                    .fg(if active {
                        theme.primary
                    } else {
                        theme.text_muted
                    })
                    .bg(bg),
                body,
            );
            put_str(
                buf,
                body.x + 6,
                y,
                label,
                Style::new()
                    .fg(if active { theme.primary } else { theme.text })
                    .bg(bg),
                body,
            );
            y += 1;
        }
        y += 1;
        let m = Style::new().fg(theme.text_muted).bg(panel);
        let t = Style::new().fg(theme.text).bg(panel);
        let parts: [(&str, Style); 5] = if self.active == Field::Filename {
            [
                ("Press ", m),
                ("return", t),
                (" to confirm, ", m),
                ("tab", t),
                (" for options", m),
            ]
        } else {
            [
                ("Press ", m),
                ("space", t),
                (" to toggle, ", m),
                ("return", t),
                (" to confirm", m),
            ]
        };
        let mut x = body.x;
        for (s, st) in parts {
            x = put_str(buf, x, y, s, st, body);
        }
        if self.active == Field::Filename {
            info.cursor
        } else {
            None
        }
    }
}

/// The transcript as Markdown, honoring the export options.
pub fn markdown(
    t: &Transcript,
    title: &str,
    opts: &Options,
    display_name: &dyn Fn(&str) -> String,
) -> String {
    let mut out = format!("# {title}\n\n");
    for m in &t.messages {
        match m.role {
            Role::User => out.push_str("## User\n\n"),
            Role::Assistant => {
                if opts.assistant_metadata {
                    out.push_str(&format!("## Assistant ({})\n\n", display_name(&m.model)));
                } else {
                    out.push_str("## Assistant\n\n");
                }
            }
            Role::Notice(_) => out.push_str("## Notice\n\n"),
        }
        for p in &m.parts {
            match p {
                Part::Text(x) => {
                    out.push_str(x.trim());
                    out.push_str("\n\n");
                }
                Part::Thought { text, .. } => {
                    if opts.thinking && !text.trim().is_empty() {
                        out.push_str("_Thinking:_\n\n");
                        out.push_str(text.trim());
                        out.push_str("\n\n");
                    }
                }
                Part::Tool(c) => {
                    if opts.tool_details {
                        out.push_str(&format!("**Tool: {}**", c.name));
                        if !c.title.is_empty() {
                            out.push_str(&format!(" `{}`", c.title));
                        }
                        out.push_str("\n\n");
                        if let Some(o) = c.output.as_deref().filter(|o| !o.trim().is_empty()) {
                            out.push_str("```\n");
                            out.push_str(o.trim_end());
                            out.push_str("\n```\n\n");
                        }
                    }
                }
            }
        }
    }
    // a file someone will `cat`, or text that goes to the clipboard: no escape sequences from
    // tool output or model text
    tuikit::width::plain_text(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{Event, ToolCall};

    #[test]
    fn options_gate_thinking_and_tool_output() {
        let mut t = Transcript::new();
        t.push_user("hi");
        t.apply(&Event::ThoughtDelta("pondering".into()));
        t.apply(&Event::Tool(ToolCall {
            id: "1".into(),
            name: "execute".into(),
            title: "ls".into(),
            output: Some("a\nb".into()),
            ..Default::default()
        }));
        t.apply(&Event::TextDelta("done".into()));
        let name = |s: &str| s.to_string();
        let all = markdown(&t, "T", &Options::default(), &name);
        assert!(all.contains("pondering") && all.contains("```\na\nb\n```"));
        let none = markdown(
            &t,
            "T",
            &Options {
                thinking: false,
                tool_details: false,
                assistant_metadata: false,
                open_without_saving: false,
            },
            &name,
        );
        assert!(!none.contains("pondering") && !none.contains("execute"));
        assert!(none.contains("## Assistant\n"));
    }

    #[test]
    fn default_name_uses_eight_chars_of_the_id() {
        assert_eq!(default_filename("ses_1234567890"), "session-ses_1234.md");
        assert_eq!(default_filename(""), "session.md");
        // timestamp ids share their first eight characters for a month
        assert_ne!(
            default_filename("2026-07-13T11-30-22"),
            default_filename("2026-07-14T09-00-00")
        );
    }

    /// Finding 20: tool output and thoughts went into the file with their control bytes.
    #[test]
    fn the_export_carries_no_escape_sequences() {
        let mut t = Transcript::new();
        t.push_user("hi");
        t.apply(&Event::Tool(ToolCall {
            id: "1".into(),
            name: "execute".into(),
            title: "ls".into(),
            output: Some("a\x1b]0;PWNED\x07b\x1b[2Jc".into()),
            ..Default::default()
        }));
        t.apply(&Event::TextDelta("done\x1b[31m".into()));
        let name = |s: &str| s.to_string();
        let md = markdown(&t, "T\x07", &Options::default(), &name);
        assert!(!md.chars().any(|c| c.is_control() && c != '\n'), "{md:?}");
        assert!(md.contains("ab") && !md.contains("PWNED"));
    }

    #[test]
    fn export_never_replaces_a_file_and_resolves_against_the_cwd() {
        let d = std::env::temp_dir().join(format!("openw-export-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let a = write_new(&d, "s.md", "one").unwrap();
        let b = write_new(&d, "s.md", "two").unwrap();
        let c = write_new(&d, "noext", "x").unwrap();
        let c2 = write_new(&d, "noext", "y").unwrap();
        assert_eq!(a, d.join("s.md"));
        assert_eq!(b, d.join("s-2.md"));
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "one");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "two");
        assert_eq!((c, c2), (d.join("noext"), d.join("noext-2")));
        let _ = std::fs::remove_dir_all(&d);
    }
}
