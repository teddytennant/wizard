//! The inspector: one node, in words, from the same snapshot the canvas drew.
//!
//! Everything here comes out of [`Inspection`], which borrows from the
//! [`MeshGraph`](crate::plugins::graph::MeshGraph) rather than copying out of it. That is
//! the whole reason the panel and the canvas cannot disagree about whether a
//! peer is live: there is one snapshot, taken at one clock, and both surfaces
//! read it.
//!
//! # What is said out loud, and why each line is here
//!
//! - **The name, with its discriminator.** Always both halves, because a peer
//!   picks its own label and the fingerprint prefix is the part it cannot
//!   choose. When the model has flagged the label as confusable with another
//!   node's, the discriminator is emphasised — advisory only, since
//!   [`DisplayName::is_ambiguous`](crate::plugins::graph::DisplayName::is_ambiguous) is
//!   an approximation and the discriminator is present either way.
//! - **Liveness and staleness together.** "stale · 4h" rather than a colour: a
//!   coloured dot with no number cannot distinguish a peer that went quiet four
//!   minutes ago from one that went quiet four months ago, and the plan's bar is
//!   correct staleness indication rather than a plausible one.
//! - **Trust, and what it permits.** The recorded decision, spelled out, plus
//!   `accepts_work` marked as the *claim* it is: a peer saying it will run work
//!   is a peer's statement about itself and this machine has not verified it.
//! - **The address and the full fingerprint, copyable.** The address is how
//!   somebody else adds this peer; the fingerprint is what two operators compare
//!   out of band. Both are useless if they cannot leave the window.
//! - **Revoke, only when there is trust to take away.** `Inspection::revocable`
//!   already answers that (a peer, and trusted), and a button that did nothing
//!   would be worse than no button.

use iced::widget::{button, column, container, row, space, text};
use iced::{Border, Element, Length, Padding};

use crate::plugins::graph::{GraphNode, Inspection, NodeKey, NodeKind};
use crate::plugins::mesh::{CapabilityKind, NodeId};
use crate::plugins::native::theme::Palette;
use crate::plugins::native::widget::chrome;
use crate::theme::Token;

use super::view::CapabilityFilter;
use super::{BODY, MONO, Message};

/// The panel for whatever is selected, or the empty-state prompt.
pub fn inspector<'a>(
    inspection: Option<Inspection<'a>>,
    pinned: bool,
    palette: &'a Palette,
) -> Element<'a, Message> {
    // A panel describing a node wants all the height it can get; a three-word
    // prompt saying nothing is selected does not. Filling in both cases made
    // the empty state — which is what the screen opens on, every time — a
    // 300-pixel column of nothing with one grey line at the top of it.
    let describing = inspection.is_some();
    let body: Element<'_, Message> = match inspection {
        None => text("select a node")
            .size(MONO)
            .color(palette.color(Token::Faint))
            .into(),
        Some(inspection) => panel(inspection, pinned, palette),
    };
    let height = match describing {
        true => Length::Fill,
        false => Length::Shrink,
    };
    container(chrome::scroll(body).height(height))
        .width(Length::Fixed(300.0))
        .height(height)
        // Everything in this panel that a *peer* chose — its name, and the
        // capability names it advertises — is text this machine did not write.
        // The wrapping above keeps it inside on the widths we know about; this
        // keeps it inside on the ones we do not.
        .clip(true)
        .padding(Padding::new(14.0))
        .style({
            let surface = palette.surface;
            let hairline = palette.hairline;
            move |_theme| container::Style {
                background: Some(iced::Background::Color(surface)),
                border: Border {
                    color: hairline,
                    width: 1.0,
                    radius: 12.0.into(),
                },
                ..container::Style::default()
            }
        })
        .into()
}

fn panel<'a>(
    inspection: Inspection<'a>,
    pinned: bool,
    palette: &'a Palette,
) -> Element<'a, Message> {
    let node = inspection.node;
    let mut body = column![name(node, palette)].spacing(10);

    // Liveness and staleness on one line, because either alone is a half
    // answer. `is_live()` is what decides the colour, as everywhere else.
    body = body.push(field(
        "state",
        format!("{} · seen {}", node.liveness.label(), node.seen_label()),
        if node.liveness.is_live() {
            Token::Success
        } else {
            Token::Muted
        },
        palette,
    ));
    body = body.push(field(
        "trust",
        node.trust.label().to_string(),
        Token::Text,
        palette,
    ));
    body = body.push(field(
        "accepts work",
        // Marked as a claim. Nothing here has verified it, and a peer that
        // says yes is still refused unless this machine trusts it.
        if node.accepts_work {
            "claims yes".to_string()
        } else {
            "no".to_string()
        },
        Token::Muted,
        palette,
    ));
    if node.delegations > 0 {
        body = body.push(field(
            "delegations",
            node.delegations.to_string(),
            Token::Text,
            palette,
        ));
    }

    if let Some(address) = &node.address {
        body = body.push(copyable("address", address, palette));
    }
    if let Some(fingerprint) = &node.fingerprint {
        body = body.push(copyable("fingerprint", fingerprint, palette));
    }

    body = body.push(capabilities(node, palette));

    if !inspection.sessions.is_empty() {
        body = body.push(links("sessions", &inspection.sessions, palette));
    }
    if !inspection.introduced.is_empty() {
        body = body.push(links("introduced", &inspection.introduced, palette));
    }
    if let Some(via) = inspection.introduced_by {
        body = body.push(links("introduced by", &[via], palette));
    }

    // The pin is the operator's arrangement, so the way out of it is beside the
    // node it applies to rather than in a menu.
    if pinned {
        body = body.push(
            button(text("release pin").size(MONO))
                .on_press(Message::Unpin(node.key.clone()))
                .style(quiet(palette)),
        );
    }

    if inspection.revocable
        && let NodeKey::Node(id) = node.key
    {
        body = body.push(revoke(id, palette));
    }

    body.into()
}

/// The name, with the fingerprint prefix the peer cannot choose.
fn name<'a>(node: &'a GraphNode, palette: &'a Palette) -> Element<'a, Message> {
    let heading = text(node.name.label().to_string())
        .size(BODY)
        // `WordOrGlyph`, not the default `Word`. This string comes from the
        // *peer*: `PeerText::sanitize` caps it at 64 characters, but nothing
        // makes it contain a space, and word wrapping cannot break a word. A
        // 64-character name with no spaces is wider than this 300-pixel panel
        // and would be drawn straight out of it, across the canvas. Breaking
        // mid-glyph is the right trade for a name — the whole thing stays
        // readable, which is the point of showing a peer's chosen label at
        // all — and the panel's clip below is what makes it a guarantee
        // rather than a hope about font metrics.
        .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
        .color(palette.color(Token::Heading));
    let discriminator = text(format!(
        "{} {}",
        crate::plugins::graph::model::DISCRIMINATOR_SEPARATOR,
        node.name.discriminator()
    ))
    .size(MONO)
    .font(iced::Font::MONOSPACE)
    .color(palette.color(if node.name.is_ambiguous() {
        // Another node's label could be mistaken for this one's. Advisory
        // emphasis: the discriminator is drawn either way, and nothing about
        // safety depends on the flag being right.
        Token::Warning
    } else {
        Token::Faint
    }));
    let mut rows = column![heading, discriminator].spacing(2);
    if node.name.is_ambiguous() {
        rows = rows.push(
            text("another node has a look-alike name")
                .size(MONO)
                .color(palette.color(Token::Warning)),
        );
    }
    rows = rows.push(
        text(match node.kind {
            NodeKind::Local => "this machine".to_string(),
            NodeKind::Peer => "peer".to_string(),
            NodeKind::Session { .. } => "session".to_string(),
        })
        .size(MONO)
        .color(palette.color(Token::Faint)),
    );
    rows.into()
}

fn field<'a>(
    label: &'a str,
    value: String,
    token: Token,
    palette: &'a Palette,
) -> Element<'a, Message> {
    row![
        text(label).size(MONO).color(palette.color(Token::Faint)),
        space().width(Length::Fill),
        text(value).size(MONO).color(palette.color(token)),
    ]
    .spacing(8)
    .into()
}

/// A value with a copy button. Both of these leave the window or they are
/// decoration.
fn copyable<'a>(label: &'a str, value: &str, palette: &'a Palette) -> Element<'a, Message> {
    column![
        text(label).size(MONO).color(palette.color(Token::Faint)),
        row![
            // 22, not 30. At `MONO` in this 300 px panel, thirty monospace
            // characters use the whole inner width, so the copy button beside
            // them was laid out past the panel's edge and rendered as a lone
            // `c`. A copy button you cannot see is the same as not having one,
            // and these two values — the address somebody adds you by, the
            // fingerprint two operators read to each other — are the entire
            // reason this panel is copyable at all.
            //
            // The `Fill` container is the guard rather than the fix: it hands
            // the button its width first and clips the value if a font ever
            // measures wider than this budget assumes. The budget is what
            // keeps the value *readable*, since eliding is only worth doing
            // while both ends still show.
            container(
                text(elide(value, 22))
                    .size(MONO)
                    .font(iced::Font::MONOSPACE)
                    .wrapping(iced::widget::text::Wrapping::None)
                    .color(palette.color(Token::Code)),
            )
            .width(Length::Fill)
            .clip(true),
            button(text("copy").size(MONO))
                .on_press(Message::Copy(value.to_string()))
                .style(quiet(palette)),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center),
    ]
    .spacing(2)
    .into()
}

/// What the node advertises, grouped by kind, each entry a filter.
///
/// Clickable because "who else has this model" is the question a capability
/// list provokes, and the filter is the answer already built.
fn capabilities<'a>(node: &'a GraphNode, palette: &'a Palette) -> Element<'a, Message> {
    let mut body = column![
        text("capabilities")
            .size(MONO)
            .color(palette.color(Token::Faint))
    ]
    .spacing(4);
    let mut any = false;
    for kind in CapabilityKind::ALL {
        let entries: Vec<_> = node.caps.iter().filter(|cap| cap.kind == kind).collect();
        if entries.is_empty() {
            continue;
        }
        any = true;
        let mut group = column![
            text(kind.label())
                .size(MONO)
                .color(palette.color(Token::Muted))
        ]
        .spacing(2);
        for cap in entries {
            group = group.push(
                button(
                    text(cap.display.clone())
                        .size(MONO)
                        .font(iced::Font::MONOSPACE)
                        // Peer-chosen, like the name above, and sanitized to a
                        // character cap rather than to a width.
                        .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
                )
                .on_press(Message::Filter(Some(CapabilityFilter {
                    kind,
                    name: cap.name.clone(),
                })))
                .style(quiet(palette)),
            );
        }
        body = body.push(container(group).padding(Padding::new(0.0).left(8.0)));
    }
    if !any {
        body = body.push(
            text("advertises nothing")
                .size(MONO)
                .color(palette.color(Token::Faint)),
        );
    }
    body.into()
}

/// A group of other nodes, each one a way to move the selection there.
fn links<'a>(
    label: &'a str,
    nodes: &[&'a GraphNode],
    palette: &'a Palette,
) -> Element<'a, Message> {
    let mut body = column![text(label).size(MONO).color(palette.color(Token::Faint))].spacing(2);
    for node in nodes {
        body = body.push(
            button(
                text(node.name.rendered())
                    .size(MONO)
                    .font(iced::Font::MONOSPACE),
            )
            .on_press(Message::Select(node.key.clone()))
            .style(quiet(palette)),
        );
    }
    body.into()
}

/// The one destructive control on the screen.
///
/// Says what it does in the sentence under it, because "revoke" alone does not
/// convey that a live stream ends the moment it is pressed.
fn revoke<'a>(id: NodeId, palette: &'a Palette) -> Element<'a, Message> {
    column![
        button(text("revoke trust").size(MONO))
            .on_press(Message::Revoke(id))
            .style({
                let error = palette.color(Token::Error);
                let canvas = palette.canvas;
                move |_theme, status| button::Style {
                    background: Some(iced::Background::Color(match status {
                        button::Status::Hovered | button::Status::Pressed => error,
                        _ => iced::Color::TRANSPARENT,
                    })),
                    text_color: match status {
                        button::Status::Hovered | button::Status::Pressed => canvas,
                        _ => error,
                    },
                    border: Border {
                        color: error,
                        width: 1.0,
                        radius: 6.0.into(),
                    },
                    ..button::Style::default()
                }
            }),
        text("blocks the peer and drops its live streams")
            .size(MONO)
            .color(palette.color(Token::Faint)),
    ]
    .spacing(4)
    .into()
}

/// The unobtrusive button style the panel uses for everything that is not
/// destructive: text, no fill, an accent on hover.
fn quiet(palette: &Palette) -> impl Fn(&iced::Theme, button::Status) -> button::Style + use<> {
    let text_color = palette.color(Token::Text);
    let accent = palette.color(Token::Accent);
    move |_theme, status| button::Style {
        background: None,
        text_color: match status {
            button::Status::Hovered | button::Status::Pressed => accent,
            _ => text_color,
        },
        border: Border::default(),
        ..button::Style::default()
    }
}

/// A long value, shortened in the middle so both ends stay readable.
///
/// The middle rather than the tail: an address and a fingerprint are compared
/// by their ends, and a tail-truncated fingerprint is one whose distinguishing
/// half has been thrown away.
fn elide(value: &str, budget: usize) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= budget {
        return value.to_string();
    }
    let head = budget.saturating_sub(1) / 2;
    let tail = budget.saturating_sub(1) - head;
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(&chars[chars.len() - tail..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both ends survive: a fingerprint compared out of band is compared by its
    /// ends, and a tail-truncated one has thrown its distinguishing half away.
    #[test]
    fn eliding_keeps_both_ends() {
        let full = "sha256:AbCdEfGhIjKlMnOpQrStUvWxYz0123456789";
        let short = elide(full, 20);
        assert_eq!(short.chars().count(), 20);
        assert!(short.starts_with("sha256:"), "{short}");
        assert!(short.ends_with("6789"), "{short}");
        assert!(short.contains('…'));
        assert_eq!(elide("short", 20), "short");
        assert_eq!(elide("", 20), "");
    }
}
