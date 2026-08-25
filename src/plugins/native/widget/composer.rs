//! The composer, and the one control that both starts and stops a turn.
//!
//! `docs/gui-design-spec.md` is specific about this and the reason is good, so
//! it is restated here rather than looked up: **the send button is the stop
//! button.** Idle it is an upward arrow; while the agent is working it becomes a
//! square, and pressing it cancels the turn. One control, in the place the hand
//! already is, and it doubles as the "something is running" indicator — so no
//! idle spinner sits in a corner reading as "loading forever".
//!
//! Two controls would also be a lie about the state machine underneath:
//! [`TaskManager`](crate::plugins::gui::tasks::TaskManager) runs at most one turn per
//! task, so "send" while working and "stop" while idle are both unreachable, and
//! a UI with two buttons has to grey one of them out on every frame to say what
//! one button says by being what it is.
//!
//! This is the one part of the window built out of stock iced widgets rather
//! than out of [`super::super::select`]. That is deliberate and it is the line
//! the whole design draws: the transcript is *read*, so it must be selectable
//! and therefore ours; the composer is *typed into*, so it wants a real
//! `text_input` with its own cursor, its own selection and its own IME, none of
//! which the selection layer has or should grow.

use iced::widget::{button, column, container, row, text, text_input};
use iced::{Border, Element, Length, Padding};

use crate::plugins::native::Message;
use crate::plugins::native::theme::Palette;
use crate::theme::Token;

use super::markdown::{BODY, MONO};

/// Placeholder copy, from the design spec.
const PLACEHOLDER: &str = "Ask wizard to change something, or / for a command";

/// And what it says while a command is waiting on an answer.
const CONSOLE_PLACEHOLDER: &str = "Answer the command, then press Enter";

/// The composer card: the draft, the status line, and the send/stop control.
///
/// `console` binds it to a running command's stdin instead of to the agent. The
/// placeholder changes, and so does what Enter means — a line goes to the child
/// process rather than into the conversation. Saying which of the two you are
/// typing into is the whole of the affordance: a line sent to the wrong end is
/// either a message the agent never asked for or an answer the command never
/// gets.
pub fn composer<'a>(
    draft: &'a str,
    model: &'a str,
    working: bool,
    console: bool,
    palette: &'a Palette,
) -> Element<'a, Message> {
    let placeholder = match console {
        true => CONSOLE_PLACEHOLDER,
        false => PLACEHOLDER,
    };
    let field = text_input(placeholder, draft)
        .id(FIELD)
        .on_input(Message::DraftChanged)
        // Enter sends. It is wired even while a turn is running, because
        // `TaskManager::submit_turn` queues a mid-turn message rather than
        // refusing it, and silently swallowing the keystroke here would make
        // the queueing the browser GUI already does invisible.
        .on_submit(Message::Send)
        .size(BODY)
        .padding(Padding::new(10.0).left(4.0).right(4.0))
        .style(move |_theme, _status| text_input::Style {
            background: iced::Background::Color(iced::Color::TRANSPARENT),
            border: Border::default(),
            icon: palette.color(Token::Faint),
            placeholder: palette.color(Token::Faint),
            value: palette.color(Token::Text),
            selection: palette.selection,
        });

    // A bound console keeps the send arrow even while the turn is working: the
    // control has to submit a line, and stopping the whole turn is not what a
    // person pressing Enter at a prompt means.
    let (glyph, message, hint) = match (console, working) {
        (true, _) => ("↑", Message::Send, "answer"),
        (false, true) => ("■", Message::Stop, "stop"),
        (false, false) => ("↑", Message::Send, "send"),
    };
    // The one filled control in the view, inverted light-on-dark, per the
    // spec's "one primary button per view".
    let send = button(text(glyph).size(MONO).center())
        .on_press(message)
        .padding(Padding::new(6.0).left(12.0).right(12.0))
        .style({
            let canvas = palette.canvas;
            let accent = palette.color(Token::Accent);
            let muted = palette.color(Token::Muted);
            move |_theme, status| button::Style {
                background: Some(iced::Background::Color(match status {
                    button::Status::Hovered | button::Status::Pressed => accent,
                    _ => muted,
                })),
                text_color: canvas,
                border: Border::default().rounded(6.0),
                ..button::Style::default()
            }
        });

    let status = row![
        text(model)
            .size(MONO)
            .font(crate::plugins::native::font::MONO)
            .color(palette.color(Token::Muted)),
        iced::widget::space().width(Length::Fill),
        text(hint).size(MONO).color(palette.color(Token::Faint)),
        send,
    ]
    .spacing(10)
    .align_y(iced::Alignment::Center);

    container(column![field, status].spacing(6))
        .width(Length::Fill)
        .padding(Padding::new(10.0))
        .style({
            let surface = palette.surface;
            let hairline = palette.hairline;
            move |_theme| container::Style {
                background: Some(iced::Background::Color(surface)),
                border: Border {
                    color: hairline,
                    width: 1.0,
                    radius: 14.0.into(),
                },
                ..container::Style::default()
            }
        })
        .into()
}

/// The text field's id, so the app can focus it on start and after a send
/// without the user having to click into it.
pub const FIELD: &str = "composer";

#[cfg(test)]
mod tests {
    use super::*;
    use iced_test::selector::id;
    use iced_test::{Error, simulator};

    fn palette() -> Palette {
        Palette::from_theme(&crate::theme::minimal())
    }

    /// The design spec's rule, as a test: one control, two meanings. If a
    /// second button ever appears, or the glyph stops changing, this fails.
    #[test]
    fn the_send_control_becomes_the_stop_control() -> Result<(), Error> {
        let palette = palette();
        let idle = composer("", "grok-4.5", false, false, &palette);
        let mut ui = simulator(idle);
        assert!(ui.find("↑").is_ok(), "idle shows send");
        assert!(ui.find("■").is_err(), "and only send");

        let busy = composer("", "grok-4.5", true, false, &palette);
        let mut ui = simulator(busy);
        assert!(ui.find("■").is_ok(), "working shows stop");
        assert!(ui.find("↑").is_err(), "and only stop");
        Ok(())
    }

    /// Pressing it means different things in the two states, which is the part
    /// that would still be broken if only the glyph had changed.
    #[test]
    fn pressing_it_sends_when_idle_and_stops_when_working() -> Result<(), Error> {
        let palette = palette();
        let mut ui = simulator(composer("do it", "grok-4.5", false, false, &palette));
        ui.click("↑")?;
        assert!(matches!(ui.into_messages().next(), Some(Message::Send)));

        let mut ui = simulator(composer("do it", "grok-4.5", true, false, &palette));
        ui.click("■")?;
        assert!(matches!(ui.into_messages().next(), Some(Message::Stop)));
        Ok(())
    }

    /// Typing reaches the app as a draft change. Without this the field looks
    /// alive and nothing it holds ever leaves it.
    #[test]
    fn typing_reports_the_draft() -> Result<(), Error> {
        let palette = palette();
        let mut ui = simulator(composer("", "grok-4.5", false, false, &palette));
        let _ = ui.click(id(FIELD))?;
        ui.typewrite("hi");
        let drafts: Vec<String> = ui
            .into_messages()
            .filter_map(|message| match message {
                Message::DraftChanged(text) => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(drafts.last().map(String::as_str), Some("hi"), "{drafts:?}");
        Ok(())
    }

    /// The active model is on the composer, in mono, because it is a thing you
    /// could paste into a config file.
    #[test]
    fn the_active_model_is_shown() -> Result<(), Error> {
        let palette = palette();
        let mut ui = simulator(composer("", "grok-4.5-fast", false, false, &palette));
        assert!(ui.find("grok-4.5-fast").is_ok());
        Ok(())
    }
}
