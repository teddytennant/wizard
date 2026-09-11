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
use crate::ui::truncate_width;

/// Ctrl-C on any screen: the whole wizard is over, not just the step. Esc is
/// the step's own back button; this is the door. Raised as an error so it
/// passes through every `?` between the widget and the entry point, which
/// turns it into "nothing saved" and a clean exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Interrupted;

impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled with ctrl-c")
    }
}

impl std::error::Error for Interrupted {}

/// Below this width the option rows drop their detail column: at 40 columns
/// a label plus its detail is cut mid-word, and the label alone is the
/// answer.
const DETAIL_MIN_WIDTH: u16 = 60;

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

/// True when `key` is Esc, the step's back button. Ctrl-C is [`Interrupted`]:
/// the wizard ends, whatever screen it was on.
pub(super) fn cancelled(key: &KeyEvent) -> Result<bool> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
        return Err(Interrupted.into());
    }
    Ok(matches!(key.code, KeyCode::Esc))
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
        if cancelled(&key)? {
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
        if cancelled(&key)? {
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
        if cancelled(&key)? {
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
            && (cancelled(&key)? || matches!(key.code, KeyCode::Enter | KeyCode::Char(_)))
        {
            return Ok(());
        }
    }
}

/// A yes-or-back question: Enter is `true`, Esc is `false`.
pub(super) fn confirm(terminal: &mut Tui, message: &str) -> Result<bool> {
    loop {
        terminal.draw(|frame| draw_notice(frame, message, "enter continue · esc back"))?;
        if let Some(key) = next_key()? {
            if cancelled(&key)? {
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
/// as far as the screen allows, and the footer sits right under it, so four
/// options are not a page of border with the keys at the bottom of it. Every
/// line of chrome is cut to the width with `…`.
fn frame_body(
    frame: &mut ratatui::Frame,
    title: &str,
    subtitle: &str,
    footer: &str,
    rows: usize,
) -> Rect {
    let area = frame.area();
    let box_height = u16::try_from(rows.max(1) + 2).unwrap_or(u16::MAX);
    let [header, body, foot, _] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(box_height),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(area);
    let width = area.width.saturating_sub(2) as usize;

    let header_lines = Text::from(vec![
        Line::from(Span::styled(
            truncate_width(&format!("  {title}"), width),
            accent().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            truncate_width(&format!("  {subtitle}"), width),
            text_dim(),
        )),
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
        Paragraph::new(Span::styled(
            truncate_width(&format!("  {footer}"), width),
            dim(),
        )),
        foot,
    );
    inner
}

/// The slice of `count` rows a box `height` tall shows with `selected` in
/// view: the window slides so the selected row is never off the bottom.
fn list_window(count: usize, height: usize, selected: usize) -> std::ops::Range<usize> {
    if height == 0 || count <= height {
        return 0..count;
    }
    let start = selected.saturating_sub(height - 1).min(count - height);
    start..start + height
}

/// One option row: the marker, the label, and the detail when the screen
/// has room for it, cut to `width` with `…`.
fn option_row(option: &Opt, active: bool, prefix: &str, width: u16) -> Line<'static> {
    let marker = if active { "▸ " } else { "  " };
    let label_style = if active {
        accent().add_modifier(Modifier::BOLD)
    } else {
        text_dim()
    };
    let mut spans = vec![
        Span::styled(format!(" {marker}{prefix}"), accent()),
        Span::styled(option.label.clone(), label_style),
    ];
    if !option.detail.is_empty() && width >= DETAIL_MIN_WIDTH {
        spans.push(Span::styled(format!("   {}", option.detail), dim()));
    }
    crate::ui::truncate_line(Line::from(spans), width as usize)
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
        "↑↓ move · enter select · esc back",
        options.len(),
    );
    // A list taller than the box scrolls with the selection; the last row
    // says how many are below rather than letting them fall off unseen.
    let window = list_window(options.len(), inner.height as usize, selected);
    let below = options.len() - window.end;
    let mut lines = Vec::with_capacity(inner.height as usize);
    for index in window.clone() {
        if below > 0 && index + 1 == window.end {
            lines.push(Line::from(Span::styled(
                format!("     … {} more", below + 1),
                dim(),
            )));
            break;
        }
        lines.push(option_row(
            &options[index],
            index == selected,
            "",
            inner.width,
        ));
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
        "↑↓ move · space toggle · enter confirm · esc skip",
        options.len(),
    );
    let window = list_window(options.len(), inner.height as usize, selected);
    let mut lines = Vec::with_capacity(inner.height as usize);
    for index in window {
        let box_ = if checked.get(index).copied().unwrap_or(false) {
            "[x] "
        } else {
            "[ ] "
        };
        lines.push(option_row(
            &options[index],
            index == selected,
            box_,
            inner.width,
        ));
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
    draw_input_with_footer(
        frame,
        title,
        subtitle,
        buffer,
        default,
        secret,
        "enter accept · esc back",
    );
}

/// The input screen held up while a pasted key is checked: the same frame,
/// the key still masked, the subtitle saying which host is being asked,
/// and a spinner where the keys were (none work until the answer is in).
pub(super) fn draw_checking(
    frame: &mut ratatui::Frame,
    title: &str,
    host: &str,
    shown: &str,
    tick: u64,
) {
    draw_input_with_footer(
        frame,
        title,
        &format!("checking with {host}…"),
        shown,
        "",
        false,
        &crate::ui::spinner_frame(tick).to_string(),
    );
}

fn draw_input_with_footer(
    frame: &mut ratatui::Frame,
    title: &str,
    subtitle: &str,
    buffer: &str,
    default: &str,
    secret: bool,
    footer: &str,
) {
    let inner = frame_body(frame, title, subtitle, footer, 2);
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
pub(super) fn masked(secret: &str) -> String {
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

    /// Esc backs out of the step; Ctrl-C ends the wizard, from any step.
    #[test]
    fn esc_backs_out_and_ctrl_c_interrupts() {
        assert!(cancelled(&KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)).unwrap());
        let err = cancelled(&KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .expect_err("ctrl-c is the door");
        assert!(err.is::<Interrupted>());
        assert!(!cancelled(&KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)).unwrap());
        assert!(!cancelled(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)).unwrap());
    }

    /// A list taller than the box slides with the selection, and the row
    /// that would fall off the bottom is counted instead of lost.
    #[test]
    fn a_long_list_scrolls_with_the_selection() {
        assert_eq!(list_window(15, 14, 0), 0..14);
        assert_eq!(list_window(15, 14, 13), 0..14);
        assert_eq!(list_window(15, 14, 14), 1..15);
        assert_eq!(list_window(3, 14, 2), 0..3);
        assert_eq!(list_window(15, 0, 2), 0..15);
    }

    /// At 40 columns an option row ends in `…` rather than mid-word, and the
    /// detail column is gone rather than cut.
    #[test]
    fn option_rows_fit_a_narrow_box() {
        let _pin = theme::pin(theme::minimal());
        let option = Opt::new(
            "Paste an API key",
            "Anthropic, OpenAI, xAI, Gemini and 11 more",
        );
        let wide: String = option_row(&option, true, "", 100)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(
            wide,
            " ▸ Paste an API key   Anthropic, OpenAI, xAI, Gemini and 11 more"
        );
        let narrow: String = option_row(&option, true, "", 36)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(narrow, " ▸ Paste an API key", "no detail under 60 columns");
        let long = Opt::new(
            "Another OpenAI-compatible endpoint, with a very long label",
            "",
        );
        let cut: String = option_row(&long, false, "", 30)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(cut.ends_with('…'), "{cut:?}");
        assert!(cut.chars().count() <= 30, "{cut:?}");
    }
}
