//! The pane: what takes the conversation's place when you open something.
//!
//! Three things can be opened this way — a file's diff, an image, a subagent's
//! run — and there is exactly one slot for all three. That was the browser
//! GUI's shape and it is worth keeping for a reason that is not habit: each of them
//! wants the *whole* centre column (a diff is wide, an image is large, a run is
//! a conversation), and a fourth column would leave the chat at a width where
//! neither is readable. The transcript is not destroyed while a pane is open,
//! so it keeps streaming behind it.
//!
//! # The image pane is where a window beat a browser outright
//!
//! The browser GUI could not read a file. Every image in it went through
//! `GET /api/image?path=…`, a route that had to re-derive whether a path was one
//! wizard wrote, because a page can ask for any string. In process there is no
//! such question: [`iced::widget::image::Handle::from_path`] opens the file the
//! agent already recorded in an [`ImageRef`](crate::images::ImageRef). That
//! route, its path validation and its failure modes are deleted.
//!
//! # The diff pane draws `gui::git`'s types directly
//!
//! [`FileDiff`], [`Hunk`], [`DiffLine`] and [`LineKind`] were already the right
//! shape — they were built for a JSON response and are a plain tree of owned
//! data. Nothing is re-parsed here; this is a renderer over them.

use std::path::PathBuf;

use iced::widget::{column, container, image, row, scrollable, text};
use iced::{Element, Length, Padding};

use crate::plugins::gui::git::{DiffLine, FileDiff, LineKind};
use crate::plugins::native::theme::Palette;
use crate::plugins::native::widget::chrome;
use crate::theme::Token;

/// What is in the slot.
pub enum Pane {
    /// The conversation.
    Chat,
    /// A file's diff. `None` while the fetch is in flight.
    Diff {
        path: String,
        diff: Option<Box<Result<FileDiff, String>>>,
    },
    Image(PathBuf),
    /// A subagent run, by id. Its content lives in
    /// [`crate::plugins::native::subagent::Rail`], because the run keeps streaming
    /// whether or not its pane is open.
    Run(u64),
}

impl Pane {
    pub fn is_chat(&self) -> bool {
        matches!(self, Pane::Chat)
    }
}

/// The pane's header: a back button, and what is being shown.
pub fn header<'a, M: Clone + 'a>(
    title: Element<'a, M>,
    right: Option<Element<'a, M>>,
    back: M,
    palette: &Palette,
) -> Element<'a, M> {
    let mut bar = row![chrome::action("‹ chat", back, palette), title]
        .spacing(10)
        .align_y(iced::Alignment::Center);
    if let Some(right) = right {
        bar = bar
            .push(iced::widget::space().width(Length::Fill))
            .push(right);
    }
    column![
        container(bar).padding(Padding::new(8.0).left(12.0).right(12.0)),
        chrome::hairline(palette),
    ]
    .into()
}

/// A file's diff, or why there is not one to show.
pub fn diff<'a, M: Clone + 'a>(
    path: &'a str,
    loaded: Option<&'a Result<FileDiff, String>>,
    back: M,
    palette: &Palette,
) -> Element<'a, M> {
    let (body, stat): (Element<'a, M>, Option<Element<'a, M>>) = match loaded {
        None => (chrome::muted("Reading the diff…", palette).into(), None),
        Some(Err(why)) => (
            text(why.clone())
                .size(chrome::UI)
                .color(palette.color(Token::Error))
                .into(),
            None,
        ),
        Some(Ok(file)) => (
            diff_body(file, palette),
            Some(
                row![
                    text(format!("+{}", file.additions))
                        .size(chrome::SMALL)
                        .font(crate::plugins::native::font::MONO)
                        .color(palette.color(Token::Success)),
                    text(format!("−{}", file.deletions))
                        .size(chrome::SMALL)
                        .font(crate::plugins::native::font::MONO)
                        .color(palette.color(Token::Error)),
                ]
                .spacing(6)
                .into(),
            ),
        ),
    };
    column![
        header(chrome::literal(path, palette).into(), stat, back, palette),
        // The diff scrolls **inside** the pane, in both directions: a long line
        // must not make the whole window scroll sideways.
        scrollable(container(body).padding(Padding::new(12.0)))
            // Both bars embedded rather than floating. `Scrollable::spacing`
            // is a no-op for `Direction::Both` — it matches only the
            // single-axis variants — so the inset has to be set on each
            // `Scrollbar` instead, which is why `chrome::scroll` cannot be
            // used here. Without it the vertical bar sits on top of the
            // rightmost column of a diff, which for a monospace line is a
            // whole character of code hidden behind a widget.
            .direction(scrollable::Direction::Both {
                horizontal: scrollable::Scrollbar::default().spacing(6.0),
                vertical: scrollable::Scrollbar::default().spacing(6.0),
            })
            .width(Length::Fill)
            .height(Length::Fill),
    ]
    // The horizontal scrollbar is the last thing in this pane, and `spacing`
    // insets a bar from the *content* only — not from the window. Without this
    // it sat on the frame's final row of pixels while every other pane keeps
    // 12 px, so the diff read as cropped rather than laid out.
    .padding(Padding::new(0.0).bottom(12.0))
    .into()
}

/// The hunks, or the prose that explains why there are none.
///
/// Three of the four cases below are *not* errors — a binary file, a mode-only
/// change and a truncation are all things git legitimately reports — and saying
/// so in a sentence is the difference between a pane that looks broken and one
/// that answered the question.
fn diff_body<'a, M: 'a>(file: &'a FileDiff, palette: &Palette) -> Element<'a, M> {
    if file.binary {
        return chrome::muted("Binary file — no line diff to show.", palette).into();
    }
    if file.hunks.is_empty() {
        return chrome::muted(
            "No line changes — git records this file as changed in its mode or name only.",
            palette,
        )
        .into();
    }
    // The column is as wide as its widest line, in pixels, computed rather
    // than asked for.
    //
    // `Fill` does not work here and the previous comment claiming it did was
    // wrong. This column lives inside a `Direction::Both` scrollable, which
    // hands its content an infinite, *compressed* limit — and under
    // compression a `Fill` child resolves against the widest `Shrink` sibling,
    // which in this column is the `@@ … @@` header. So every row came out
    // header-width: the washes stopped a couple of inches in, and, worse, the
    // scrollable's content was that narrow too, leaving the horizontal
    // scrollbar nothing to scroll. A long line was drawn past the pane edge
    // with no way to reach the rest of it, which is the exact thing
    // `Direction::Both` and `Wrapping::None` were put here to allow.
    //
    // A width in pixels fixes both at once: the rows are a continuous band and
    // the scroll region knows how far right the content really goes. It is
    // computable because the font is ours and monospaced — JetBrains Mono is
    // bundled in `assets/fonts/`, and its advance is 0.6 em, so a line is
    // `chars × size × 0.6` wide plus the row's own padding.
    //
    // Degrades rather than breaks if that estimate is ever short: a line wider
    // than the column is still drawn in full and still clipped by the
    // scrollable, exactly as it is today. What is lost is a few pixels of wash
    // at the end of the longest line, not a line.
    let widest = file
        .hunks
        .iter()
        .flat_map(|hunk| {
            std::iter::once(hunk.header.chars().count())
                .chain(hunk.lines.iter().map(|line| line.text.chars().count()))
        })
        .max()
        .unwrap_or(0);
    let mut body = column![]
        .spacing(0)
        .width(Length::Fixed(diff_line_width(widest)));
    for hunk in &file.hunks {
        body = body.push(
            container(chrome::literal(hunk.header.clone(), palette))
                .padding(Padding::new(4.0).top(10.0)),
        );
        for line in &hunk.lines {
            body = body.push(diff_line(line, palette));
        }
    }
    if file.truncated {
        body = body.push(chrome::muted("Truncated — the rest is not shown.", palette));
    }
    body.into()
}

/// Point size of a diff line. Named because [`diff_line_width`] has to agree
/// with it, and two literals that must match are one edit away from not.
const DIFF_SIZE: f32 = 12.0;

/// Horizontal advance of JetBrains Mono, as a fraction of the point size.
///
/// A property of the bundled font (`assets/fonts/jetbrains-mono.ttf`), not a
/// guess about whatever the system has: `crate::plugins::native::font` exists precisely
/// so that no monospace text here resolves through a fallback.
const MONO_ADVANCE: f32 = 0.6;

/// Width of a diff row holding `chars` monospace characters, including the
/// padding [`diff_line`] puts either side of it.
fn diff_line_width(chars: usize) -> f32 {
    chars as f32 * DIFF_SIZE * MONO_ADVANCE + DIFF_PADDING * 2.0
}

/// Padding either side of a diff line's text.
const DIFF_PADDING: f32 = 8.0;

/// `Token::DiffAdd`/`Token::DiffDel`, not `Success`/`Error`.
///
/// They are not synonyms, and the shipped theme is where that shows. `minimal`
/// is monochrome and sets `error = "white"` deliberately, then defines
/// `diff.add = "green"` and `diff.del = "red"` under the comment: "the `/diff`
/// sidebar keeps the conventional colors: this is the one place a hue carries
/// the meaning, and inverting it would be actively confusing."
///
/// Reading `Error` here took the white. A deleted line rendered with a neutral
/// grey wash next to a green added one, which reads as *highlighted*, not as
/// removed — and the rail's diffstat showed a green `+30` beside a white `−14`.
/// The TUI has always used the diff tokens (`src/ui/mod.rs`); the window did not,
/// and the tokens existed the whole time.
fn diff_line<'a, M: 'a>(line: &'a DiffLine, palette: &Palette) -> Element<'a, M> {
    let (color, wash) = match line.kind {
        LineKind::Add => (
            palette.color(Token::DiffAdd),
            Some(tint(palette.color(Token::DiffAdd))),
        ),
        LineKind::Del => (
            palette.color(Token::DiffDel),
            Some(tint(palette.color(Token::DiffDel))),
        ),
        LineKind::Meta => (palette.color(Token::DiffMeta), None),
        LineKind::Ctx => (palette.color(Token::Muted), None),
    };
    container(
        text(line.text.clone())
            .size(DIFF_SIZE)
            .font(crate::plugins::native::font::MONO)
            // A diff line is a line. The scroll region around this is
            // `Direction::Both` precisely so a long one can be scrolled to
            // rather than reflowed, but `text` word-wraps by default, so it
            // wrapped instead — a two-word-wide ribbon of green running down
            // the pane, with the horizontal scrollbar it was given nothing to
            // scroll. Wrapping off is what makes the surrounding decision mean
            // anything, and it is also the only honest way to show a diff:
            // reflowed source is not the text that changed.
            .wrapping(iced::widget::text::Wrapping::None)
            .color(color),
    )
    .width(Length::Fill)
    .padding(Padding::new(0.0).left(DIFF_PADDING).right(DIFF_PADDING))
    .style(move |_theme| container::Style {
        background: wash.map(iced::Background::Color),
        ..container::Style::default()
    })
    .into()
}

/// An add/delete wash: the token colour at low alpha, so the tint reads as a
/// band behind the line rather than as a second foreground.
fn tint(color: iced::Color) -> iced::Color {
    iced::Color { a: 0.11, ..color }
}

/// One image, drawn.
///
/// `from_path` rather than bytes: the file is on disk, the agent put it there,
/// and iced's image cache keys on the path — so a transcript that names the same
/// image twice decodes it once.
pub fn image_pane<'a, M: Clone + 'a>(
    path: &'a PathBuf,
    back: M,
    palette: &Palette,
) -> Element<'a, M> {
    let body: Element<'a, M> = match path.exists() {
        true => image(image::Handle::from_path(path))
            .content_fit(iced::ContentFit::ScaleDown)
            .into(),
        // A file the agent wrote and something else removed. Said plainly,
        // with the path, because the path is the actionable half.
        false => text(format!("The file is gone: {}", path.display()))
            .size(chrome::UI)
            .color(palette.color(Token::Error))
            .into(),
    };
    column![
        header(
            chrome::literal(path.display().to_string(), palette).into(),
            None,
            back,
            palette
        ),
        scrollable(container(body).padding(Padding::new(16.0)))
            .width(Length::Fill)
            .height(Length::Fill),
    ]
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::gui::git::Hunk;

    fn palette() -> Palette {
        Palette::from_theme(&crate::theme::minimal())
    }

    #[derive(Debug, Clone, PartialEq)]
    enum Probe {
        Back,
    }

    fn file(hunks: Vec<Hunk>, binary: bool, truncated: bool) -> FileDiff {
        FileDiff {
            path: "src/lib.rs".to_string(),
            status: 'M',
            additions: 3,
            deletions: 1,
            binary,
            truncated,
            hunks,
        }
    }

    /// Three not-an-error cases that all render as an empty hunk list. Each one
    /// needs its own sentence, or the pane looks broken in three different
    /// ways that all read the same.
    #[test]
    fn a_diff_with_no_hunks_says_which_kind_of_nothing_it_is() -> Result<(), iced_test::Error> {
        let palette = palette();

        let binary = file(Vec::new(), true, false);
        let shown = Ok(binary);
        let mut ui = iced_test::simulator(diff("a.png", Some(&shown), Probe::Back, &palette));
        assert!(ui.find("Binary file — no line diff to show.").is_ok());

        let mode_only = Ok(file(Vec::new(), false, false));
        let mut ui = iced_test::simulator(diff("a.sh", Some(&mode_only), Probe::Back, &palette));
        assert!(
            ui.find(crate::plugins::native::probe::contains("mode or name only"))
                .is_ok()
        );

        let failed: Result<FileDiff, String> = Err("not a changed file".to_string());
        let mut ui = iced_test::simulator(diff("x", Some(&failed), Probe::Back, &palette));
        assert!(ui.find("not a changed file").is_ok());
        Ok(())
    }

    /// A truncated diff says so. Silently stopping at twenty thousand lines
    /// would make "the rest is unchanged" and "we stopped showing you"
    /// identical, which is the same mistake the transcript's output clipping
    /// exists to avoid.
    #[test]
    fn a_truncated_diff_admits_it() -> Result<(), iced_test::Error> {
        let hunk = Hunk {
            header: "@@ -1,2 +1,3 @@".to_string(),
            lines: vec![DiffLine {
                kind: LineKind::Add,
                text: "+one".to_string(),
            }],
        };
        let shown = Ok(file(vec![hunk], false, true));
        let mut ui =
            iced_test::simulator(diff("src/lib.rs", Some(&shown), Probe::Back, &palette()));
        assert!(ui.find("+one").is_ok());
        assert!(
            ui.find(crate::plugins::native::probe::contains("Truncated"))
                .is_ok()
        );
        Ok(())
    }

    /// The back control is the only way out besides Escape, so it has to be
    /// wired. A pane you cannot leave is a window you have to restart.
    #[test]
    fn the_header_carries_a_way_back() -> Result<(), iced_test::Error> {
        let shown = Ok(file(Vec::new(), true, false));
        let mut ui = iced_test::simulator(diff("a.png", Some(&shown), Probe::Back, &palette()));
        ui.click("‹ chat")?;
        assert_eq!(ui.into_messages().next(), Some(Probe::Back));
        Ok(())
    }

    /// An image whose file has gone says the path. It is the only part of the
    /// message that can be acted on.
    #[test]
    fn a_missing_image_names_the_file() -> Result<(), iced_test::Error> {
        let path = PathBuf::from("/nowhere/gone.png");
        let mut ui = iced_test::simulator(image_pane(&path, Probe::Back, &palette()));
        assert!(
            ui.find(crate::plugins::native::probe::contains("/nowhere/gone.png"))
                .is_ok()
        );
        Ok(())
    }
}
