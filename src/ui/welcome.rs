//! The four home screens: what fills the transcript before the first message.
//!
//! One per [`WelcomeStyle`], because a home screen is the loudest thing a skin
//! says and the four say it in genuinely different shapes — a centered mark, a
//! rounded card, a `>_` banner, a block behind an accent bar. They agree on
//! content: who you are talking to, which model and mode, how to begin, and
//! anything that went wrong at startup. A screen that dropped the last of
//! those to look tidier would be hiding the one thing on it that needs acting
//! on.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;

use super::{accent, dim, format_cwd, mode_span, model_span, muted, truncate_width, warning};
use crate::app::App;
use crate::skin::{self, WelcomeStyle};
use crate::theme::{self, Token};
use crate::transcript::TranscriptItem;

/// How many startup notices the welcome card shows. Enough for the handful a
/// broken config raises, few enough that the card stays a card.
const MAX_WELCOME_NOTICES: usize = 3;

/// Display columns one welcome notice may take before it is cut.
const WELCOME_NOTICE_WIDTH: usize = 68;

/// The wand-and-spark mark, as braille dots.
///
/// Braille because it is the only way a terminal draws at better than one dot
/// per cell: U+2800..U+28FF is a 2x4 grid per character, so 26 columns carry
/// the same shape a 52-pixel bitmap would. Generated from `assets/wizard-mark.png`
/// (trim, normalize, threshold at 50%) rather than drawn by hand, so it is the
/// same mark as the favicon and the README rather than an artist's impression
/// of it.
///
/// Every row is padded to the same width so the centering below has one number
/// to work with; the trailing cells are U+2800 BRAILLE PATTERN BLANK, which is
/// a blank that occupies a cell, not a space.
const MARK_ART: &[&str] = &[
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢳⡀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠸⣿⣦⣀⣤⣤⡴⠂",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣠⣿⣿⣿⡿⠋⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⡠⠾⠟⠛⠻⣿⣷⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢀⣤⣶⡄⠀⠀⠀⠈⠻⡄⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢀⣴⣿⡿⠋⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢀⣴⣿⡿⠋⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⢀⣴⣿⡿⠋⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⢀⣴⣿⡿⠋⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⢀⣴⣿⣿⠟⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⢀⣴⣿⣿⠟⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⢀⣴⣿⣿⠟⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⢿⣿⠟⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
];

/// Welcome screen shown before the first message. Which one depends on the
/// active skin; they all say the same four things (who you are talking to,
/// which model and mode, how to start, and anything that went wrong at
/// startup), because a home screen that omitted a startup warning to look
/// tidier would hide the one thing on it that needs acting on.
pub(crate) fn draw_welcome(frame: &mut Frame, app: &App, area: Rect) {
    match skin::chrome().welcome {
        WelcomeStyle::Mark => draw_welcome_mark(frame, app, area),
        WelcomeStyle::Banner => draw_welcome_banner(frame, app, area),
        WelcomeStyle::Bar => draw_welcome_bar(frame, app, area),
    }
}

/// Anything the user has to read before they start: a provider that did not
/// answer its health probe, then the startup notices.
///
/// Startup notices (a theme name that would not load, a config that did not
/// parse) go into the transcript, and the transcript is not drawn while a
/// welcome screen is up, and `has_conversation` ignores notices on purpose, so
/// the welcome screen stays until the user actually says something. Without
/// this they were invisible exactly when they mattered: `WIZARD_THEME=
/// solarised wizard` opened on the default theme with no hint that the name
/// was wrong, and the notice only appeared after the first submission.
///
/// Every line is truncated, because a provider error carries a URL and the
/// provider's own prose and is routinely wider than the screen it lands on.
fn welcome_notices(app: &App) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(err) = &app.provider_health_error {
        lines.push(Line::from(Span::styled(
            truncate_width(
                &format!("⚠ provider unreachable: {err}"),
                WELCOME_NOTICE_WIDTH,
            ),
            warning().bold(),
        )));
    }
    let notices: Vec<&String> = app
        .transcript
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::Notice(text) => Some(text),
            _ => None,
        })
        .collect();
    for text in notices.iter().rev().take(MAX_WELCOME_NOTICES) {
        let line = text.lines().next().unwrap_or_default();
        lines.push(Line::from(Span::styled(
            format!("⚠ {}", truncate_width(line, WELCOME_NOTICE_WIDTH)),
            warning(),
        )));
    }
    lines
}

/// `model · mode`, the one line every welcome screen carries.
fn welcome_status(app: &App) -> Line<'static> {
    Line::from(vec![
        model_span(app),
        Span::styled(" · ", dim()),
        mode_span(app.status.mode),
    ])
}

/// Draw `lines` down the left of `area`, one blank row of margin at the top and
/// one column at the left, clipped to what fits.
fn draw_welcome_lines(frame: &mut Frame, area: Rect, lines: Vec<Line<'static>>) {
    let body = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(1),
    };
    if body.width == 0 || body.height == 0 {
        return;
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), body);
}

/// The house welcome: the braille mark over a small centered card, drawn only
/// when the terminal has room to spare for it.
fn draw_welcome_mark(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line<'static>> = vec![
        Line::from(Span::styled(
            "w i z a r d",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled("your sovereign agent", dim().italic())),
        Line::raw(""),
        Line::from(vec![
            model_span(app),
            Span::styled(" · ", dim()),
            mode_span(app.status.mode),
        ]),
        Line::raw(""),
        Line::raw(""),
        Line::from(vec![
            Span::styled("type a message", muted()),
            Span::styled(" and press Enter to begin", dim()),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::styled("/", accent()),
            Span::styled("  commands — Tab completes, ↑/↓ select", dim()),
        ]),
        Line::from(vec![
            Span::styled("/model", accent()),
            Span::styled("  pick a model", dim()),
        ]),
        Line::from(vec![
            Span::styled("/help", accent()),
            Span::styled("  all commands & keys", dim()),
        ]),
    ];

    // Anything that went wrong at startup goes under the model line, so it is
    // visible at launch rather than only when a turn fails. Inserted at one
    // index so they keep the order they were raised in.
    for notice in welcome_notices(app).into_iter().rev() {
        lines.insert(4, notice);
    }

    // The mark, above the name, when the card can afford it.
    //
    // Conditional because the hints below are the useful part: on a short
    // terminal a thirteen-line drawing would push "type a message" off the
    // screen, and a splash that hides the instructions is worse than no splash.
    // The +2 is the blank line under the art and one row of slack, so the card
    // never sits flush against the composer.
    let art_width = MARK_ART.first().map_or(0, |row| row.chars().count()) as u16;
    if area.height as usize >= lines.len() + MARK_ART.len() + 2 && area.width >= art_width {
        // Dim: it is a watermark, not a headline. The name under it is what the
        // eye should land on.
        let mut art: Vec<Line<'static>> = MARK_ART
            .iter()
            .map(|row| Line::from(Span::styled(*row, dim())))
            .collect();
        art.push(Line::raw(""));
        art.append(&mut lines);
        lines = art;
    }

    let height = lines.len() as u16;
    let top = area.height.saturating_sub(height) / 2;
    let centered = Rect {
        x: area.x,
        y: area.y + top,
        width: area.width,
        height: height.min(area.height),
    };
    frame.render_widget(
        Paragraph::new(Text::from(lines)).alignment(Alignment::Center),
        centered,
    );
}

/// Codex's welcome: a `>_` banner over a left-aligned block.
fn draw_welcome_banner(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines = vec![
        Line::from(vec![
            Span::styled(">_ ", accent().bold()),
            Span::styled("Wizard", theme::style(Token::Text).bold()),
            Span::styled(", your sovereign agent", dim()),
        ]),
        Line::raw(""),
        welcome_status(app),
        Line::from(Span::styled(
            format!("cwd: {}", format_cwd(&app.project_root, 48)),
            dim(),
        )),
    ];
    lines.push(Line::raw(""));
    lines.extend(welcome_hints(app));
    draw_welcome_lines(frame, area, lines);
}

/// Grok Build's welcome: the same block, behind the accent bar the whole skin
/// is built around.
fn draw_welcome_bar(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines = vec![
        Line::from(Span::styled("Wizard", theme::style(Token::Text).bold())),
        Line::from(Span::styled("your sovereign agent", dim().italic())),
        Line::raw(""),
        welcome_status(app),
        Line::from(Span::styled(
            format!("cwd: {}", format_cwd(&app.project_root, 48)),
            dim(),
        )),
        Line::raw(""),
    ];
    lines.extend(welcome_hints(app));
    // The bar runs the height of the block, blank rows included — the same
    // rule `gutter_block` follows, and for the same reason.
    let bar = Span::styled("┃ ", accent());
    let lines = lines
        .into_iter()
        .map(|line| {
            let mut spans = vec![bar.clone()];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect();
    draw_welcome_lines(frame, area, lines);
}

/// The startup warnings and the "here is how to begin" hints, shared by the
/// three left-aligned welcome screens.
fn welcome_hints(app: &App) -> Vec<Line<'static>> {
    let mut lines = welcome_notices(app);
    if !lines.is_empty() {
        lines.push(Line::raw(""));
    }
    lines.push(Line::from(vec![
        Span::styled("type a message", muted()),
        Span::styled(" and press Enter to begin", dim()),
    ]));
    lines.push(Line::raw(""));
    // Padded into a column: left-aligned, ragged blurbs read as a list of
    // unrelated fragments, and this is the part of the screen a first-time
    // user is actually meant to act on.
    for (command, blurb) in [
        ("/", "commands — Tab completes, ↑/↓ select"),
        ("/model", "pick a model"),
        ("/ui", "switch the interface"),
        ("/help", "all commands & keys"),
    ] {
        lines.push(Line::from(vec![
            Span::styled(format!("{command:<8}"), accent()),
            Span::styled(blurb, dim()),
        ]));
    }
    lines
}
