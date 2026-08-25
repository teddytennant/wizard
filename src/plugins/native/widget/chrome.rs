//! The small vocabulary every panel in the window is built out of.
//!
//! `docs/gui-design-spec.md` names the failure mode it wants avoided in so many
//! words: "a settings screen made of eight cards, each with a tagline under it
//! and a blue button at the bottom … reads as filler, because it is." What it
//! asks for instead is one list shape, hairlines instead of boxes, and section
//! labels that read the same in the sidebar (`CHATS`), the rail (`GIT TOOLS`)
//! and Settings (`PROVIDERS`).
//!
//! That is only true if there is one implementation of each. So the label, the
//! row, the quiet action and the one filled button live here, and every panel
//! calls them. A panel that draws its own header has already forked the design.
//!
//! Everything in this module is a plain function over stock iced widgets, on
//! purpose: this is chrome, it is never selected across, and it is exactly the
//! part of the window where `container`, `row` and `text` are the right answer.
//! The transcript is the other half — see [`super`] on why *that* one cannot be.

use iced::widget::{button, column, container, row, text};
use iced::{Border, Element, Length, Padding};

use crate::plugins::native::font;
use crate::plugins::native::theme::Palette;
use crate::theme::Token;

/// Section label size. 10.5px uppercase and letterspaced, per the spec.
pub const LABEL: f32 = 10.5;
/// A literal: a path, a model tag, a branch name. Mono.
pub const LITERAL: f32 = 11.5;
/// Ages, hints, help text.
pub const SMALL: f32 = 12.0;
/// The UI's default.
pub const UI: f32 = 13.0;

/// A section label: `PROVIDERS`, `GIT TOOLS`, `CHATS`.
///
/// Uppercased here rather than at every call site, so a label typed in lower
/// case cannot render differently from the one beside it. Letterspacing is
/// faked with interleaved thin spaces, because iced 0.14's `Text` has no
/// letter-spacing control and the alternative is a label that reads as a word
/// rather than as a rule.
pub fn label<'a, M: 'a>(text_: &str, palette: &Palette) -> Element<'a, M> {
    let spaced: String = text_
        .to_uppercase()
        .chars()
        .flat_map(|ch| [ch, '\u{2009}'])
        .collect();
    text(spaced)
        .size(LABEL)
        .color(palette.color(Token::Faint))
        .into()
}

/// A hairline: the divider between regions.
pub fn hairline<'a, M: 'a>(palette: &Palette) -> Element<'a, M> {
    let color = palette.hairline;
    container(iced::widget::space().height(1))
        .width(Length::Fill)
        .style(move |_theme| container::Style {
            background: Some(iced::Background::Color(color)),
            ..container::Style::default()
        })
        .into()
}

/// A divider *inside* a surface: felt, not seen.
pub fn separator<'a, M: 'a>(palette: &Palette) -> Element<'a, M> {
    let color = palette.separator;
    container(iced::widget::space().height(1))
        .width(Length::Fill)
        .style(move |_theme| container::Style {
            background: Some(iced::Background::Color(color)),
            ..container::Style::default()
        })
        .into()
}

/// A quiet text action: no chrome until it is hovered.
///
/// Every action in the window is one of these except the single filled
/// [`primary`] per view. That ratio is the spec's, and it is what keeps a panel
/// from reading as a form.
pub fn action<'a, M: Clone + 'a>(text_: &'a str, message: M, palette: &Palette) -> Element<'a, M> {
    let muted = palette.color(Token::Muted);
    let bright = palette.color(Token::Text);
    let hover = palette.raised;
    button(text(text_).size(SMALL))
        .on_press(message)
        .padding(Padding::new(4.0).left(7.0).right(7.0))
        .style(move |_theme, status| button::Style {
            background: match status {
                button::Status::Hovered | button::Status::Pressed => {
                    Some(iced::Background::Color(hover))
                }
                _ => None,
            },
            text_color: match status {
                button::Status::Hovered | button::Status::Pressed => bright,
                _ => muted,
            },
            border: Border::default().rounded(7.0),
            ..button::Style::default()
        })
        .into()
}

/// A quiet action that means *destroy something*: the same shape, red on hover.
///
/// Not red at rest, because a row of red words reads as a list of errors. The
/// colour arrives at the moment the pointer is on the thing it warns about.
pub fn danger<'a, M: Clone + 'a>(text_: &'a str, message: M, palette: &Palette) -> Element<'a, M> {
    let muted = palette.color(Token::Muted);
    let red = palette.color(Token::Error);
    let hover = palette.raised;
    button(text(text_).size(SMALL))
        .on_press(message)
        .padding(Padding::new(4.0).left(7.0).right(7.0))
        .style(move |_theme, status| button::Style {
            background: match status {
                button::Status::Hovered | button::Status::Pressed => {
                    Some(iced::Background::Color(hover))
                }
                _ => None,
            },
            text_color: match status {
                button::Status::Hovered | button::Status::Pressed => red,
                _ => muted,
            },
            border: Border::default().rounded(7.0),
            ..button::Style::default()
        })
        .into()
}

/// The one filled button in a view, inverted light-on-dark.
///
/// `None` for a message disables it, which is the only state that greys out
/// anywhere in this window.
pub fn primary<'a, M: Clone + 'a>(
    text_: &'a str,
    message: Option<M>,
    palette: &Palette,
) -> Element<'a, M> {
    let canvas = palette.canvas;
    let accent = palette.color(Token::Text);
    let muted = palette.color(Token::Muted);
    let mut control =
        button(text(text_).size(SMALL).center()).padding(Padding::new(6.0).left(14.0).right(14.0));
    if let Some(message) = message {
        control = control.on_press(message);
    }
    control
        .style(move |_theme, status| button::Style {
            background: Some(iced::Background::Color(match status {
                button::Status::Disabled => muted,
                _ => accent,
            })),
            text_color: canvas,
            border: Border::default().rounded(6.0),
            ..button::Style::default()
        })
        .into()
}

/// A whole row that is clickable: a chat in the sidebar, a provider, a changed
/// file. `selected` gives it the lighter background the spec uses for "you are
/// here" — brightness, never a hue.
pub fn pick<'a, M: Clone + 'a>(
    content: impl Into<Element<'a, M>>,
    message: M,
    selected: bool,
    palette: &Palette,
) -> Element<'a, M> {
    let hover = palette.raised;
    let chosen = palette.surface;
    button(content)
        .on_press(message)
        .width(Length::Fill)
        .padding(Padding::new(5.0).left(8.0).right(8.0))
        .style(move |_theme, status| button::Style {
            background: Some(iced::Background::Color(match (selected, status) {
                (true, _) => hover,
                (false, button::Status::Hovered | button::Status::Pressed) => chosen,
                _ => iced::Color::TRANSPARENT,
            })),
            text_color: iced::Color::WHITE,
            border: Border::default().rounded(7.0),
            ..button::Style::default()
        })
        .into()
}

/// A block: a label and its rows, separated from the next block by a hairline.
/// The one list shape, used by Settings, the rail and the sidebar alike.
pub fn block<'a, M: 'a>(
    heading: &str,
    rows: Vec<Element<'a, M>>,
    palette: &Palette,
) -> Element<'a, M> {
    let mut stack = column![label(heading, palette)].spacing(6);
    for row_ in rows {
        stack = stack.push(row_);
    }
    container(stack)
        .width(Length::Fill)
        .padding(Padding::new(0.0).top(4.0).bottom(12.0))
        .into()
}

/// Body text in the UI's sans face.
pub fn body<'a>(text_: impl text::IntoFragment<'a>, palette: &Palette) -> text::Text<'a> {
    text(text_).size(UI).color(palette.color(Token::Text))
}

/// Secondary text: a summary, a help line, a status.
pub fn muted<'a>(text_: impl text::IntoFragment<'a>, palette: &Palette) -> text::Text<'a> {
    text(text_).size(SMALL).color(palette.color(Token::Muted))
}

/// A literal — a path, a model tag, a base URL, a branch name — in mono.
///
/// "Sans for prose, mono for literals" is the spec's rule and the reason is
/// that a literal is a thing you could paste into a terminal. This function is
/// how that rule is kept: there is nothing else that sets mono in a panel.
pub fn literal<'a>(text_: impl text::IntoFragment<'a>, palette: &Palette) -> text::Text<'a> {
    text(text_)
        .size(LITERAL)
        .font(font::MONO)
        .color(palette.color(Token::Muted))
}

/// A row with its second element pushed to the right edge.
///
/// # Do not use this when the left side has no bound on its width
///
/// This helper produced most of a batch of eighteen layout bugs, and it is
/// worth writing down why so the next one is not the nineteenth.
///
/// In iced's flex layout, `Fill` children are laid out **last**, out of
/// whatever is left after every non-fluid sibling has taken its intrinsic
/// size. `left` here is usually `Shrink`, so it is measured *first*, against
/// the whole width the row was given — and `right` is then laid out in the
/// remainder, which can be **zero**. A control on the right does not shrink,
/// wrap or clip when that happens. It disappears.
///
/// That is not theoretical. It hid `remove` on a settings row behind a long
/// model id, `end input (Ctrl-D)` behind a long command, the diffstat behind a
/// long path, the unread badge behind a subagent's name, and the copy button
/// in the mesh inspector behind an address. Each one was the only control on
/// its row that did the thing you would want at that moment.
///
/// So: use `spread` when the left side is short and bounded — a heading, a
/// label, a fixed chip. When it is a path, a title, a model id, a command, an
/// error, or anything else a user or a peer supplies, write the row out and
/// make the *variable* side the elastic one:
///
/// ```ignore
/// row![
///     container(text(whatever).wrapping(Wrapping::None))
///         .width(Length::Fill)
///         .clip(true),
///     the_control,
/// ]
/// .spacing(8)
/// .align_y(iced::Alignment::Center)
/// ```
///
/// The `Fill` gives the control its width first, `Wrapping::None` keeps a
/// one-line row one line, and the clip turns the overflow into a truncation
/// rather than into ink on the neighbour.
///
/// And do not nest `spread` inside `spread`: each contributes a `Fill` space,
/// two of them split the slack between the halves, and the inner one spends
/// its share pushing its right element past the edge of whatever contains it.
pub fn spread<'a, M: 'a>(
    left: impl Into<Element<'a, M>>,
    right: impl Into<Element<'a, M>>,
) -> iced::widget::Row<'a, M> {
    row![
        left.into(),
        iced::widget::space().width(Length::Fill),
        right.into()
    ]
    .align_y(iced::Alignment::Center)
    .spacing(8)
}

/// A panel against a window edge: a hairline on the side facing the
/// conversation, and rows behind it. Not a card floating in space with dead air
/// beneath it, which is what the spec asks this *not* to be.
/// How much room an embedded scrollbar leaves between itself and the content.
///
/// iced floats a scrollbar *over* the content by default (`Scrollbar::spacing`
/// is `None`), which is invisible until something in the content reaches the
/// right edge — and then it sits on top of it. In the sidebar that was every
/// chat's timestamp: `18d` rendered as `18c`, `25d` as `25c`, because the last
/// glyph was under the bar. A clipped digit is worse than a narrower column,
/// since it is still perfectly readable as the wrong value.
const SCROLLBAR_SPACING: f32 = 6.0;

/// A vertical scroll region whose bar is embedded rather than floating.
///
/// Use this instead of `scrollable` wherever the content can reach the right
/// edge. Where content is inset well clear of it — a transcript with 12-16 px
/// of its own padding — a floating bar is fine and this is not needed.
pub fn scroll<'a, M: 'a>(content: impl Into<Element<'a, M>>) -> iced::widget::Scrollable<'a, M> {
    iced::widget::scrollable(content).spacing(SCROLLBAR_SPACING)
}

pub fn rail<'a, M: 'a>(
    content: impl Into<Element<'a, M>>,
    width: f32,
    border_left: bool,
    palette: &Palette,
) -> Element<'a, M> {
    let hairline = palette.hairline;
    let canvas = palette.canvas;
    let edge = container(iced::widget::space().width(1))
        .height(Length::Fill)
        .style(move |_theme| container::Style {
            background: Some(iced::Background::Color(hairline)),
            ..container::Style::default()
        });
    let panel = container(content)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(Padding::new(12.0));
    // `.width(Fill)` on the row, and it is load-bearing rather than tidy. A
    // `Row` defaults to `Shrink`, and a `Fill` child inside a `Shrink` parent
    // resolves against the parent's *intrinsic* width — so `panel` sized
    // itself to its content and the row laid out wider than the fixed width
    // wrapping it, instead of the content being given the fixed width to fit
    // inside. That is why the sidebar header spread itself over ~250 px in a
    // 240 px rail with a 100 px gap in the middle: the gap was a `Fill` space
    // expanding into room the rail did not have. Filling here makes the
    // fixed width the constraint the content is laid out against.
    let laid_out: Element<'a, M> = match border_left {
        true => row![edge, panel],
        false => row![panel, edge],
    }
    .width(Length::Fill)
    .into();
    container(laid_out)
        .width(Length::Fixed(width))
        .height(Length::Fill)
        // A fixed width is a promise about where this panel ends, and iced does
        // not keep it for free: a row whose intrinsic width exceeds its bounds
        // is laid out past them rather than compressed, so the overflow paints
        // over whatever is next. That is not hypothetical — the sidebar's own
        // header ("wizard  mesh  settings") was wider than the 240 it is given
        // and drew `settings` across the divider and into the chat pane's
        // title, two overlapping words on different baselines being the first
        // thing in the window. Clipping makes the fixed width mean what it
        // says for every future caller too, whatever they put in a rail.
        .clip(true)
        .style(move |_theme| container::Style {
            background: Some(iced::Background::Color(canvas)),
            ..container::Style::default()
        })
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced_test::{Error, simulator};

    fn palette() -> Palette {
        Palette::from_theme(&crate::theme::minimal())
    }

    #[derive(Debug, Clone, PartialEq)]
    enum Probe {
        Pressed,
    }

    /// Section labels are uppercased *here*, so a caller that types
    /// `"providers"` cannot render a label that reads differently from the one
    /// beside it.
    #[test]
    fn a_section_label_is_uppercased_wherever_it_was_typed() -> Result<(), Error> {
        let palette = palette();
        let spaced = |word: &str| {
            word.chars()
                .flat_map(|ch| [ch, '\u{2009}'])
                .collect::<String>()
        };
        let mut ui = simulator::<Probe, _, _>(label::<Probe>("providers", &palette));
        assert!(ui.find(spaced("PROVIDERS").as_str()).is_ok());
        assert!(ui.find(spaced("providers").as_str()).is_err());
        Ok(())
    }

    /// A disabled primary really is unpressable. `None` is the only way this
    /// window greys anything out, so if it stopped working the greyed button
    /// would still submit.
    #[test]
    fn a_primary_without_a_message_cannot_be_pressed() -> Result<(), Error> {
        let palette = palette();
        let mut ui = simulator(primary("Save", Some(Probe::Pressed), &palette));
        ui.click("Save")?;
        assert_eq!(ui.into_messages().next(), Some(Probe::Pressed));

        let mut ui = simulator(primary::<Probe>("Save", None, &palette));
        let _ = ui.click("Save");
        assert_eq!(ui.into_messages().next(), None);
        Ok(())
    }

    /// A whole row is the click target, not just the words in it. A sidebar
    /// where only the title is clickable has a dead gutter beside every chat.
    #[test]
    fn a_pick_row_is_clickable_across_its_whole_width() -> Result<(), Error> {
        let palette = palette();
        let mut ui = simulator(pick(
            spread(body("wizard", &palette), muted("2m", &palette)),
            Probe::Pressed,
            false,
            &palette,
        ));
        ui.click("2m")?;
        assert_eq!(ui.into_messages().next(), Some(Probe::Pressed));
        Ok(())
    }
}
