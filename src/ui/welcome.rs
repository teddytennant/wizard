//! The home screens: what fills the transcript before the first message.
//!
//! One per [`WelcomeStyle`], because a home screen is the loudest thing a skin
//! says and they say it in genuinely different shapes — three plain lines, a
//! `>_` banner, a block behind an accent bar. They agree on content: what you
//! are talking to, where, how to begin, and anything that went wrong at
//! startup. A screen that dropped the last of those to look tidier would be
//! hiding the one thing on it that needs acting on.

use ratatui::Frame;
use ratatui::layout::Rect;
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

/// Welcome screen shown before the first message. Which one depends on the
/// active skin; they all say the same four things (who you are talking to,
/// which model and mode, how to start, and anything that went wrong at
/// startup), because a home screen that omitted a startup warning to look
/// tidier would hide the one thing on it that needs acting on.
pub(crate) fn draw_welcome(frame: &mut Frame, app: &App, area: Rect) {
    match skin::chrome().welcome {
        WelcomeStyle::Mark => draw_empty_state(frame, app, area),
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
    if let Some(line) = app.provider_health_line() {
        lines.push(Line::from(Span::styled(
            truncate_width(&format!("⚠ {line}"), WELCOME_NOTICE_WIDTH),
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
        // The glyph is for something broken. A quiet notice (a server skipped
        // because its command is not installed) is a dim line, as the
        // transcript draws it.
        let (glyph, style) = if line.starts_with("error") {
            ("⚠ ", warning())
        } else {
            ("", dim())
        };
        lines.push(Line::from(Span::styled(
            format!("{glyph}{}", truncate_width(line, WELCOME_NOTICE_WIDTH)),
            style,
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

/// The house empty state: three lines, left-aligned, at the top.
///
/// The name and version, where you are, and how to start. The model is not
/// repeated here because the status line below already says it, and the
/// mode only appears when it is `sovereign`. Startup problems go between,
/// where they cannot be missed. Suggested prompts, when there are any, belong
/// under the hint as one muted `❯ …` row each; the function that produces
/// them is onboarding's, and this is the call site that draws them.
fn draw_empty_state(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line<'static>> = vec![
        Line::from(vec![
            Span::styled("wizard", Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(format!(" {}", env!("CARGO_PKG_VERSION")), dim()),
        ]),
        Line::from(Span::styled(
            format_cwd(&app.project_root, area.width.saturating_sub(4) as usize),
            dim(),
        )),
    ];
    if app.status.mode == crate::config::Mode::Sovereign {
        lines.push(Line::from(mode_span(app.status.mode)));
    }
    let notices = welcome_notices(app);
    if !notices.is_empty() {
        lines.push(Line::raw(""));
        lines.extend(notices);
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled("type a message", muted()),
        Span::styled(" · / lists commands", dim()),
    ]));
    let starters = starter_prompt_lines(app);
    if !starters.is_empty() {
        lines.push(Line::raw(""));
        lines.extend(starters);
    }
    draw_welcome_lines(frame, area, lines);
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
    lines.extend(starter_prompt_lines(app));
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

/// The empty-state hook: the first run's summary line when there is one,
/// then the starter prompts numbered 1..3, the ↓-selected one in the accent.
/// Empty when there are no prompts. One function on purpose, so a restyle
/// touches one place.
fn starter_prompt_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(summary) = &app.first_run_summary {
        lines.push(Line::from(Span::styled(summary.clone(), dim())));
        lines.push(Line::raw(""));
    }
    if app.starter_prompts.is_empty() {
        return lines;
    }
    lines.push(Line::from(Span::styled("or ↓ to pick one", dim())));
    // Padded to one width so the rows still line up when the card is
    // centered.
    let width = app
        .starter_prompts
        .iter()
        .map(|prompt| prompt.chars().count())
        .max()
        .unwrap_or(0);
    for (index, prompt) in app.starter_prompts.iter().enumerate() {
        let selected = app.starter_index == Some(index);
        let text = format!("{}  {prompt:<width$}", index + 1);
        lines.push(Line::from(Span::styled(
            text,
            if selected { accent() } else { muted() },
        )));
    }
    lines.push(Line::raw(""));
    lines
}
