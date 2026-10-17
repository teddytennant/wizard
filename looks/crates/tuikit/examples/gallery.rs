//! Renders one frame of a widget to stdout as truecolor ANSI, for eyeballing against
//! `reference/*/*.png` (pipe through a private tmux socket and `tools/shot.py`).
//!
//! usage: gallery <name> [--size WxH] [--theme NAME] [--light]
//! names: home, palette, markdown, diff, diff-split, toast, scanner, editor, prompt

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::Widget;
use tuikit::border::PromptFrame;
use tuikit::dialog::{FooterHint, SelectDialog};
use tuikit::diff::{self, DiffOptions, DiffView};
use tuikit::editor::{Editor, EditorStyle};
use tuikit::markdown::{self, MarkdownOptions};
use tuikit::paint::{fill, put_line, put_str};
use tuikit::select::SelectItem;
use tuikit::spinner::Scanner;
use tuikit::testing::TestTerminal;
use tuikit::toast::{Toast, ToastView};
use tuikit::{Mode, Theme, Variant};

const MD: &str = r#"# Release notes

Plain paragraph with **bold**, *emphasis*, ~~strike~~, `inline code` and a [link](https://example.com/docs).
This line wraps because it is longer than the box it is drawn in, and the wrap must respect display width: 日本語のテキスト too.

## Lists

- first item with a long tail that wraps onto the next line to show the hanging indent
  - nested item
  - another nested item
- second item

1. ordered one
2. ordered two

> A quoted thought that spans
> more than one line.

```rust
fn main() {
    // highlighted from the theme's syntax tokens
    let n: u32 = 42;
    println!("{}", "hello");
}
```

| Tool | Result | Time |
|:-----|:------:|-----:|
| Read | ok | 12ms |
| Edit | applied | 340ms |

---

Done.
"#;

const OLD: &str = "fn greet(name: &str) {\n    println!(\"hello {}\", name);\n}\n\nfn main() {\n    greet(\"world\");\n    let unused = 1;\n}\n";
const NEW: &str = "fn greet(name: &str, loud: bool) {\n    if loud {\n        println!(\"HELLO {}\", name);\n    } else {\n        println!(\"hello {}\", name);\n    }\n}\n\nfn main() {\n    greet(\"world\", true);\n}\n";

fn palette_items() -> Vec<SelectItem<&'static str>> {
    let it = |t: &'static str| SelectItem::new(t, t);
    vec![
        it("Switch session").hint("ctrl+x l").group("Suggested"),
        it("Switch model").hint("ctrl+x m").group("Suggested"),
        it("Connect provider").group("Suggested"),
        it("Switch session").hint("ctrl+x l").group("Session"),
        it("New session").hint("ctrl+x n").group("Session"),
        it("Open editor").hint("ctrl+x e").group("Session"),
        it("Move session")
            .description("Move to another project dir")
            .group("Session"),
        it("Switch agent").hint("ctrl+x a").group("Agent"),
        it("Switch model").hint("ctrl+x m").group("Agent"),
        it("Toggle thinking").group("Agent"),
    ]
}

/// The opencode home screen: dark page, input box, key hints, status line.
fn home(buf: &mut Buffer, area: Rect, theme: &Theme, text: &str) {
    fill(buf, area, Style::new().bg(theme.background).fg(theme.text));
    let w = 75.min(area.width.saturating_sub(2));
    let x = area.x + area.width.saturating_sub(w).div_ceil(2);
    let y = area.y + (area.height * 17 / 36).min(area.height.saturating_sub(5));
    let frame = Rect::new(x, y, w, 5);
    PromptFrame {
        border: theme.secondary,
        bg: theme.background_element,
    }
    .render(frame, buf);
    let inner = PromptFrame::inner(frame);
    let mut ed = Editor::with_text(text);
    let style = EditorStyle {
        text: Style::new().fg(theme.text).bg(theme.background_element),
        placeholder: Some((
            "Ask anything...".into(),
            Style::new()
                .fg(theme.text_muted)
                .bg(theme.background_element),
        )),
    };
    ed.render(Rect { height: 1, ..inner }, buf, &style);
    let bg = theme.background_element;
    let line = Line::from(vec![
        ratatui::text::Span::styled("Build", Style::new().fg(theme.secondary).bg(bg)),
        ratatui::text::Span::styled(" · ", Style::new().fg(theme.text_muted).bg(bg)),
        ratatui::text::Span::styled("Big Pickle", Style::new().fg(theme.text).bg(bg)),
        ratatui::text::Span::styled(" OpenCode Zen", Style::new().fg(theme.text_muted).bg(bg)),
    ]);
    put_line(buf, inner.x, inner.y + 2, &line, inner);
    let hint = Line::from(vec![
        ratatui::text::Span::styled("tab ", Style::new().fg(theme.text)),
        ratatui::text::Span::styled("agents", Style::new().fg(theme.text_muted)),
        ratatui::text::Span::raw("  "),
        ratatui::text::Span::styled("ctrl+p ", Style::new().fg(theme.text)),
        ratatui::text::Span::styled("commands", Style::new().fg(theme.text_muted)),
    ]);
    let hx = (x + w).saturating_sub(27);
    put_line(buf, hx, y + 5, &hint, area);
    put_str(
        buf,
        area.x + 1,
        area.bottom().saturating_sub(1),
        "~/proj:main",
        Style::new().fg(theme.text_muted),
        area,
    );
}

fn markdown_screen(buf: &mut Buffer, area: Rect, theme: &Theme) {
    fill(buf, area, Style::new().bg(theme.background).fg(theme.text));
    let w = area.width.saturating_sub(6).min(100);
    let lines = markdown::render(MD, w, theme, &MarkdownOptions::default());
    for (i, l) in lines.iter().enumerate().take(area.height as usize) {
        put_line(buf, area.x + 3, area.y + i as u16, l, area);
    }
}

fn diff_screen(buf: &mut Buffer, area: Rect, theme: &Theme, view: DiffView) {
    fill(buf, area, Style::new().bg(theme.background).fg(theme.text));
    put_str(
        buf,
        area.x + 2,
        area.y,
        "← Edit src/main.rs",
        Style::new().fg(theme.text_muted),
        area,
    );
    let opts = DiffOptions {
        view,
        lang: Some("rust".into()),
        ..Default::default()
    };
    let w = area.width.saturating_sub(4);
    let lines = diff::from_texts(OLD, NEW, w, theme, &opts);
    for (i, l) in lines
        .iter()
        .enumerate()
        .take(area.height.saturating_sub(2) as usize)
    {
        put_line(buf, area.x + 2, area.y + 2 + i as u16, l, area);
    }
}

fn scanner_screen(buf: &mut Buffer, area: Rect, theme: &Theme) {
    fill(buf, area, Style::new().bg(theme.background).fg(theme.text));
    let sc = Scanner::for_theme(theme.secondary, theme);
    for (row, frame) in [0usize, 3, 7, 12, 17, 22, 40, 53].into_iter().enumerate() {
        let spans = sc.cells(frame);
        let mut x = area.x + 2;
        for (ch, c) in spans {
            x = put_str(
                buf,
                x,
                area.y + 1 + row as u16,
                &ch.to_string(),
                Style::new().fg(c).bg(theme.background_element),
                area,
            );
        }
        put_str(
            buf,
            area.x + 12,
            area.y + 1 + row as u16,
            &format!("frame {frame}"),
            Style::new().fg(theme.text_muted),
            area,
        );
    }
    let t = Theme::alpha_over(theme.text_muted, theme.background, theme.thinking_opacity);
    put_str(
        buf,
        area.x + 2,
        area.y + 11,
        "thinking text at theme opacity",
        Style::new().fg(t),
        area,
    );
}

fn editor_screen(buf: &mut Buffer, area: Rect, theme: &Theme) {
    fill(buf, area, Style::new().bg(theme.background).fg(theme.text));
    let frame = Rect::new(
        area.x + 4,
        area.y + 2,
        area.width.saturating_sub(8).min(60),
        8,
    );
    PromptFrame {
        border: theme.primary,
        bg: theme.background_element,
    }
    .render(frame, buf);
    let inner = PromptFrame::inner(frame);
    let mut ed = Editor::with_text(
        "A longer prompt that has to soft wrap across several rows, with 日本語 and a tab:\there.\n\nSecond paragraph after a blank line.",
    );
    let style = EditorStyle {
        text: Style::new().fg(theme.text).bg(theme.background_element),
        placeholder: None,
    };
    let info = ed.render(inner, buf, &style);
    if let Some((cx, cy)) = info.cursor {
        if let Some(c) = buf.cell_mut((cx, cy)) {
            c.set_style(Style::new().add_modifier(Modifier::REVERSED));
        }
    }
    put_str(
        buf,
        area.x + 4,
        frame.bottom() + 1,
        &format!("needs {} rows at width {}", info.needed_height, inner.width),
        Style::new().fg(theme.text_muted),
        area,
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let name = args.first().map_or("home", String::as_str).to_string();
    let mut size = (120u16, 36u16);
    let mut theme_name = "opencode".to_string();
    let mut mode = Mode::Dark;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--size" => {
                i += 1;
                if let Some((w, h)) = args.get(i).and_then(|s| s.split_once('x')) {
                    size = (w.parse().unwrap_or(120), h.parse().unwrap_or(36));
                }
            }
            "--theme" => {
                i += 1;
                if let Some(t) = args.get(i) {
                    theme_name = t.clone();
                }
            }
            "--light" => mode = Mode::Light,
            _ => {}
        }
        i += 1;
    }
    let theme = Theme::builtin_mode(&theme_name, mode).unwrap_or_else(|| {
        eprintln!(
            "unknown theme {theme_name:?}; known: {}",
            Theme::names().join(", ")
        );
        std::process::exit(2);
    });
    let mut t = TestTerminal::new(size.0, size.1);
    t.draw(|buf, area| match name.as_str() {
        "home" => home(buf, area, &theme, "hello"),
        "prompt" => home(buf, area, &theme, ""),
        "palette" => {
            home(buf, area, &theme, "");
            let mut d = SelectDialog::new("Commands", palette_items());
            d.render(buf, area, &theme);
        }
        "sessions" => {
            home(buf, area, &theme, "");
            let items = (0..30)
                .map(|n| {
                    SelectItem::new(n, format!("Session number {n} about something"))
                        .description("2h ago")
                        .current(n == 2)
                        .group(if n < 10 { "Today" } else { "Yesterday" })
                })
                .collect();
            let mut d = SelectDialog::new("Sessions", items).footer(vec![
                FooterHint::new("delete", "ctrl+d"),
                FooterHint::new("rename", "ctrl+r"),
            ]);
            d.state.move_by(3);
            d.render(buf, area, &theme);
        }
        "markdown" => markdown_screen(buf, area, &theme),
        "diff" => diff_screen(buf, area, &theme, DiffView::Unified),
        "diff-split" => diff_screen(buf, area, &theme, DiffView::Split),
        "toast" => {
            home(buf, area, &theme, "");
            let toast = Toast::new(
                Variant::Error,
                "Could not reach the provider. Check your network and try again in a moment.",
            )
            .titled("Request failed");
            ToastView {
                toast: &toast,
                theme: &theme,
            }
            .render(area, buf);
        }
        "scanner" => scanner_screen(buf, area, &theme),
        "editor" => editor_screen(buf, area, &theme),
        other => {
            eprintln!("unknown widget {other:?}");
            std::process::exit(2);
        }
    });
    // No newline after the last row: a full-height frame would scroll the screen by one.
    let dump = t.ansi();
    print!("{}", dump.strip_suffix('\n').unwrap_or(&dump));
}
