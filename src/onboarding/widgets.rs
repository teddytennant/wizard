//! The setup screens' widgets: the select list, the checklist, the text
//! input, the transient notice, and the terminal they draw on.

use std::io::Stdout;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::theme::{self, Token};

// The wizard paints with the same semantic tokens as the main TUI, so
// `NO_COLOR`, `WIZARD_COLOR` and `WIZARD_THEME` mean here what they mean
// everywhere else. It used to carry its own white/gray/darkgray constants,
// which made first-run setup the one screen that ignored all three: a machine
// with no config is exactly where a user who sets `NO_COLOR=1` meets Wizard
// first, and the wizard painted colors at them anyway. Under the default
// theme the three tokens below are the same white/gray/darkgray, so nothing
// looks different unless the user asked for it.

/// The one accent: titles, the selection marker, the input caret.
pub(super) fn accent() -> Style {
    theme::style(Token::Accent)
}

/// Dim chrome: borders, hints, footers.
pub(super) fn dim() -> Style {
    theme::style(Token::Faint)
}

/// Secondary text: subtitles and option details.
pub(super) fn text_dim() -> Style {
    theme::style(Token::Muted)
}

pub(super) type Tui = Terminal<CrosstermBackend<Stdout>>;

pub(super) fn setup_terminal() -> Result<Tui> {
    crossterm::terminal::enable_raw_mode().context("enabling raw mode")?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)
        .context("entering alternate screen")?;
    Terminal::new(CrosstermBackend::new(stdout)).context("creating terminal")
}

/// Restore the terminal if (and only if) raw mode is active. Safe on any exit
/// path; idempotent.
pub(super) fn restore_terminal_best_effort() {
    if crossterm::terminal::is_raw_mode_enabled().unwrap_or(false) {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen);
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// One selectable row.
pub(super) struct Opt {
    label: String,
    detail: String,
}

impl Opt {
    pub(super) fn new(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            detail: detail.into(),
        }
    }
}

/// True when `key` is Esc or Ctrl-C — the universal cancel chord.
pub(super) fn is_cancel(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Esc)
        || (key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')))
}

/// Render a vertical list of options; navigate with ↑/↓, confirm with Enter.
/// Returns the selected index, or `None` on Esc/Ctrl-C.
pub(super) fn select(
    terminal: &mut Tui,
    title: &str,
    subtitle: &str,
    options: &[Opt],
    default: usize,
) -> Result<Option<usize>> {
    let mut selected = default.min(options.len().saturating_sub(1));
    loop {
        terminal.draw(|frame| draw_select(frame, title, subtitle, options, selected))?;
        let Some(key) = next_key()? else { continue };
        if is_cancel(&key) {
            return Ok(None);
        }
        match key.code {
            KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') => {
                selected = if selected == 0 {
                    options.len().saturating_sub(1)
                } else {
                    selected - 1
                };
            }
            KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => {
                selected = if selected + 1 >= options.len() {
                    0
                } else {
                    selected + 1
                };
            }
            KeyCode::Enter => return Ok(Some(selected)),
            _ => {}
        }
    }
}

/// Render a checklist of options; ↑/↓ move, Space toggles the current row,
/// Enter confirms. Returns the per-row checked state, or `None` on Esc/Ctrl-C.
/// All rows start unchecked.
pub(super) fn multi_select(
    terminal: &mut Tui,
    title: &str,
    subtitle: &str,
    options: &[Opt],
) -> Result<Option<Vec<bool>>> {
    let mut checked = vec![false; options.len()];
    let mut selected = 0usize;
    loop {
        terminal
            .draw(|frame| draw_multi_select(frame, title, subtitle, options, &checked, selected))?;
        let Some(key) = next_key()? else { continue };
        if is_cancel(&key) {
            return Ok(None);
        }
        match key.code {
            KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') => {
                selected = if selected == 0 {
                    options.len().saturating_sub(1)
                } else {
                    selected - 1
                };
            }
            KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => {
                selected = if selected + 1 >= options.len() {
                    0
                } else {
                    selected + 1
                };
            }
            KeyCode::Char(' ') => {
                if let Some(slot) = checked.get_mut(selected) {
                    *slot = !*slot;
                }
            }
            KeyCode::Enter => return Ok(Some(checked)),
            _ => {}
        }
    }
}

/// Free-text input step. Enter accepts (empty submits the default); Esc/Ctrl-C
/// cancels. Returns the entered (or default) value.
pub(super) fn text_input(
    terminal: &mut Tui,
    title: &str,
    subtitle: &str,
    default: &str,
) -> Result<Option<String>> {
    input(terminal, title, subtitle, default, false)
}

/// [`text_input`] for a secret: the screen shows the first characters and
/// dots for the rest. No default.
pub(super) fn secret_input(
    terminal: &mut Tui,
    title: &str,
    subtitle: &str,
) -> Result<Option<String>> {
    input(terminal, title, subtitle, "", true)
}

fn input(
    terminal: &mut Tui,
    title: &str,
    subtitle: &str,
    default: &str,
    secret: bool,
) -> Result<Option<String>> {
    let mut buffer = String::new();
    loop {
        terminal.draw(|frame| draw_input(frame, title, subtitle, &buffer, default, secret))?;
        let Some(key) = next_key()? else { continue };
        if is_cancel(&key) {
            return Ok(None);
        }
        match key.code {
            KeyCode::Enter => {
                let value = if buffer.trim().is_empty() {
                    default.to_string()
                } else {
                    buffer.trim().to_string()
                };
                return Ok(Some(value));
            }
            KeyCode::Backspace => {
                buffer.pop();
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                buffer.push(c);
            }
            _ => {}
        }
    }
}

/// Show a transient message until the user presses a key (used for validation
/// errors). Always returns once a key is read.
pub(super) fn notice(terminal: &mut Tui, message: &str) -> Result<()> {
    loop {
        terminal.draw(|frame| draw_notice(frame, message, "press any key to continue"))?;
        if let Some(key) = next_key()?
            && (is_cancel(&key) || matches!(key.code, KeyCode::Enter | KeyCode::Char(_)))
        {
            return Ok(());
        }
    }
}

/// A yes-or-back question: Enter is `true`, Esc / Ctrl-C is `false`.
pub(super) fn confirm(terminal: &mut Tui, message: &str) -> Result<bool> {
    loop {
        terminal.draw(|frame| draw_notice(frame, message, "enter continue · esc back"))?;
        if let Some(key) = next_key()? {
            if is_cancel(&key) {
                return Ok(false);
            }
            if key.code == KeyCode::Enter {
                return Ok(true);
            }
        }
    }
}

/// Block until the next key *press* (ignoring releases), polling so the draw
/// loop stays responsive. `None` means "nothing yet, redraw".
fn next_key() -> Result<Option<KeyEvent>> {
    if event::poll(Duration::from_millis(150)).context("polling terminal events")?
        && let Event::Key(key) = event::read().context("reading terminal event")?
        && key.kind != KeyEventKind::Release
    {
        return Ok(Some(key));
    }
    Ok(None)
}

/// Compose the outer frame (header + bordered body + footer) and return the
/// inner content area for the step to fill. The box is `rows` tall inside,
/// as far as the screen allows, so four options are not a page of border.
fn frame_body(
    frame: &mut ratatui::Frame,
    title: &str,
    subtitle: &str,
    footer: &str,
    rows: usize,
) -> Rect {
    let area = frame.area();
    let box_height = u16::try_from(rows.max(1) + 2).unwrap_or(u16::MAX);
    let [header, body, _, foot] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(box_height),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);

    let header_lines = Text::from(vec![
        Line::from(Span::styled(
            format!("  {title}"),
            accent().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(format!("  {subtitle}"), text_dim())),
    ]);
    frame.render_widget(Paragraph::new(header_lines), header);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(theme::border_type())
        .border_style(dim())
        .title(Span::styled(" wizard setup ", dim()));
    let inner = block.inner(body);
    frame.render_widget(block, body);

    frame.render_widget(
        Paragraph::new(Span::styled(format!("  {footer}"), dim())),
        foot,
    );
    inner
}

fn draw_select(
    frame: &mut ratatui::Frame,
    title: &str,
    subtitle: &str,
    options: &[Opt],
    selected: usize,
) {
    let inner = frame_body(
        frame,
        title,
        subtitle,
        "↑/↓ move · enter select · esc cancel",
        options.len(),
    );
    let mut lines = Vec::with_capacity(options.len());
    for (index, option) in options.iter().enumerate() {
        let active = index == selected;
        let marker = if active { "▸ " } else { "  " };
        let label_style = if active {
            accent().add_modifier(Modifier::BOLD)
        } else {
            text_dim()
        };
        let mut spans = vec![
            Span::styled(format!(" {marker}"), accent()),
            Span::styled(option.label.clone(), label_style),
        ];
        if !option.detail.is_empty() {
            spans.push(Span::styled(format!("   {}", option.detail), dim()));
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn draw_multi_select(
    frame: &mut ratatui::Frame,
    title: &str,
    subtitle: &str,
    options: &[Opt],
    checked: &[bool],
    selected: usize,
) {
    let inner = frame_body(
        frame,
        title,
        subtitle,
        "↑/↓ move · space toggle · enter confirm · esc skip",
        options.len(),
    );
    let mut lines = Vec::with_capacity(options.len());
    for (index, option) in options.iter().enumerate() {
        let active = index == selected;
        let marker = if active { "▸ " } else { "  " };
        let box_ = if checked.get(index).copied().unwrap_or(false) {
            "[x]"
        } else {
            "[ ]"
        };
        let label_style = if active {
            accent().add_modifier(Modifier::BOLD)
        } else {
            text_dim()
        };
        let mut spans = vec![
            Span::styled(format!(" {marker}"), accent()),
            Span::styled(format!("{box_} "), accent()),
            Span::styled(option.label.clone(), label_style),
        ];
        if !option.detail.is_empty() {
            spans.push(Span::styled(format!("   {}", option.detail), dim()));
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn draw_input(
    frame: &mut ratatui::Frame,
    title: &str,
    subtitle: &str,
    buffer: &str,
    default: &str,
    secret: bool,
) {
    let inner = frame_body(frame, title, subtitle, "enter accept · esc cancel", 2);
    let shown = if buffer.is_empty() {
        Span::styled(
            if default.is_empty() {
                "  (type a value)".to_string()
            } else {
                format!("  {default}")
            },
            dim(),
        )
    } else if secret {
        Span::styled(format!("  {}", masked(buffer)), accent())
    } else {
        Span::styled(format!("  {buffer}"), accent())
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(" ▸ ", accent()),
        shown,
        Span::styled("▏", accent()),
    ])];
    if !default.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("   default: {default}"),
            dim(),
        )));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// The first four characters, then a dot per character typed.
fn masked(secret: &str) -> String {
    let mut out = String::new();
    for (index, c) in secret.chars().enumerate() {
        out.push(if index < 4 { c } else { '•' });
    }
    out
}

fn draw_notice(frame: &mut ratatui::Frame, message: &str, footer: &str) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(theme::border_type())
        .border_style(theme::style(Token::Warning))
        .title(Span::styled(
            " notice ",
            theme::style(Token::Warning).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(format!("  {message}"), text_dim())),
            Line::from(""),
            Line::from(Span::styled(format!("  {footer}"), dim())),
        ])
        .alignment(Alignment::Left)
        .wrap(Wrap { trim: false }),
        inner,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Adversarial: first-run setup is where a user who exports `NO_COLOR=1`
    /// meets Wizard, and it is the one screen that used to ignore it. The
    /// wizard carried its own white/gray/darkgray constants, so neither
    /// `NO_COLOR`, `WIZARD_COLOR` nor `WIZARD_THEME` reached it while the TUI
    /// it hands off to honoured all three.
    #[test]
    fn the_wizard_paints_with_the_active_theme_not_a_palette_of_its_own() {
        use ratatui::style::Color;
        use std::sync::Arc;

        use crate::theme::ColorDepth;

        // A terminal that wants no color at all: every one of the wizard's
        // three styles has to come back uncolored.
        {
            let _pin = theme::pin(Arc::new(theme::minimal().with_depth(ColorDepth::Mono)));
            for (name, style) in [
                ("accent", accent()),
                ("dim", dim()),
                ("text_dim", text_dim()),
            ] {
                assert_eq!(style.fg, Some(Color::Reset), "{name} kept a color");
            }
            assert_eq!(theme::style(Token::Warning).fg, Some(Color::Reset));
        }

        // And under the default theme they are the palette the wizard used to
        // hard-code, so honouring the theme changed nothing for the user who
        // set none of those variables.
        let _pin = theme::pin(theme::minimal());
        assert_eq!(accent().fg, Some(Color::White));
        assert_eq!(dim().fg, Some(Color::DarkGray));
        assert_eq!(text_dim().fg, Some(Color::Gray));
    }

    #[test]
    fn is_cancel_matches_esc_and_ctrl_c_only() {
        assert!(is_cancel(&KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(is_cancel(&KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        )));
        assert!(!is_cancel(&KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::NONE
        )));
        assert!(!is_cancel(&KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL
        )));
    }
}
