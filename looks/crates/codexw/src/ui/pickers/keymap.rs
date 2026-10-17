//! `/keymap` (spec C.9.3): the tabbed list of every shortcut with its bindings, and `/keymap
//! debug`, the keypress inspector. Codex lets the list edit a binding; codexw's keys are fixed,
//! so Enter on a row says so instead of opening an editor.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};

use crate::keymap::{BINDINGS, Binding, Context, action_label, inspect};
use crate::style::palette;
use crate::ui::slash_popup::{MAX_POPUP_ROWS, ScrollState};
use crate::ui::{AppAction, BottomView, ViewResult};
use crate::wrap::width_of;

const TABS: [(&str, &str); 11] = [
    ("All", "All configurable shortcuts."),
    ("Common", "Frequently customized shortcuts."),
    ("Customized", "Root-level shortcut overrides."),
    ("Unbound", "Actions without an active shortcut."),
    ("App", "Global and chat-level shortcuts."),
    ("Composer", "Composer submission and queue shortcuts."),
    ("Editor", "Inline editor movement and editing shortcuts."),
    ("Vim", "Vim normal-mode and operator shortcuts."),
    (
        "Navigation",
        "Pager and selection-list navigation shortcuts.",
    ),
    ("Approval", "Approval prompt shortcuts."),
    ("Debug", "Shortcuts for diagnosing key input."),
];

/// The Common tab's order (spec C.9.3).
const COMMON: [(Context, &str); 20] = [
    (Context::Composer, "submit"),
    (Context::Chat, "interrupt_turn"),
    (Context::Editor, "insert_newline"),
    (Context::Composer, "queue"),
    (Context::Global, "open_external_editor"),
    (Context::Global, "copy"),
    (Context::Global, "toggle_vim_mode"),
    (Context::Editor, "delete_backward_word"),
    (Context::Editor, "delete_forward_word"),
    (Context::Editor, "move_word_left"),
    (Context::Editor, "move_word_right"),
    (Context::Global, "open_transcript"),
    (Context::Pager, "close"),
    (Context::Pager, "page_up"),
    (Context::Pager, "page_down"),
    (Context::Approval, "open_fullscreen"),
    (Context::Approval, "approve"),
    (Context::Approval, "approve_for_session"),
    (Context::Approval, "decline"),
    (Context::Approval, "cancel"),
];

fn in_tab(tab: usize, b: &Binding) -> bool {
    use Context::*;
    match tab {
        0 => true,
        1 => COMMON
            .iter()
            .any(|(c, a)| *c == b.context && *a == b.action),
        2 => false,
        3 => b.keys.is_empty(),
        4 => matches!(b.context, Global | Chat),
        5 => b.context == Composer,
        6 => b.context == Editor,
        7 => matches!(b.context, VimNormal | VimOperator | VimTextObject),
        8 => matches!(b.context, Pager | List),
        9 => b.context == Approval,
        _ => false,
    }
}

/// The bindings of a tab in its display order.
fn rows_of(tab: usize) -> Vec<&'static Binding> {
    if tab == 1 {
        return COMMON
            .iter()
            .filter_map(|(c, a)| BINDINGS.iter().find(|b| b.context == *c && b.action == *a))
            .collect();
    }
    BINDINGS.iter().filter(|b| in_tab(tab, b)).collect()
}

#[derive(Debug)]
pub struct KeymapView {
    tab: usize,
    query: String,
    state: ScrollState,
}

impl KeymapView {
    pub fn new() -> Self {
        let mut v = Self {
            tab: 0,
            query: String::new(),
            state: ScrollState::default(),
        };
        v.refresh();
        v
    }

    fn rows(&self) -> Vec<&'static Binding> {
        let q = self.query.to_lowercase();
        rows_of(self.tab)
            .into_iter()
            .filter(|b| {
                q.is_empty()
                    || action_label(b.action).to_lowercase().contains(&q)
                    || b.context.label().to_lowercase().contains(&q)
                    || b.summary().to_lowercase().contains(&q)
            })
            .collect()
    }

    fn refresh(&mut self) {
        let n = self.rows().len();
        self.state.reset();
        self.state.clamp_selection(n);
        self.state.ensure_visible(n, MAX_POPUP_ROWS.min(n.max(1)));
    }

    fn unbound(&self) -> usize {
        BINDINGS.iter().filter(|b| b.keys.is_empty()).count()
    }

    fn tab_line(&self) -> Line<'static> {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (i, (name, _)) in TABS.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw("  "));
            }
            let label = match *name {
                "Customized" => "Customized (0)".to_string(),
                "Unbound" => format!("Unbound ({})", self.unbound()),
                n => n.to_string(),
            };
            if i == self.tab {
                spans.push(Span::styled(format!("[{label}]"), palette().accent()));
            } else {
                spans.push(Span::styled(label, Style::default().dim()));
            }
        }
        Line::from(spans)
    }

    fn row_count_rows(&self) -> u16 {
        self.rows().len().clamp(1, MAX_POPUP_ROWS) as u16
    }
}

impl Default for KeymapView {
    fn default() -> Self {
        Self::new()
    }
}

impl BottomView for KeymapView {
    fn desired_height(&self, width: u16) -> u16 {
        let tabs_h = width_of(
            &self
                .tab_line()
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>(),
        )
        .div_ceil((width.saturating_sub(4)).max(1) as usize)
        .max(1) as u16;
        // pad, title, subtitle, summary, blank, tabs, blank, search, rows, pad, hint
        1 + 3 + 1 + tabs_h + 1 + 1 + self.row_count_rows() + 1 + 1
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.height < 6 {
            return;
        }
        let band_h = area.height - 1;
        let band = Rect::new(area.x, area.y, area.width, band_h);
        tuikit::paint::set_style(buf, band, palette().user_message_style());
        let inner_w = band.width.saturating_sub(4);
        let mut y = band.y + 1;
        let put = |buf: &mut Buffer, y: u16, line: Line<'static>| {
            if y < band.bottom().saturating_sub(1) {
                let r = Rect::new(band.x + 2, y, inner_w, 1);
                tuikit::paint::put_line(buf, r.x, y, &line, r);
            }
        };
        put(buf, y, Line::from("Keymap".bold()));
        y += 1;
        put(buf, y, Line::from(TABS[self.tab].1.dim()));
        y += 1;
        let total = BINDINGS.len();
        let count = if self.tab == 0 {
            format!("{total} actions, 0 customized, {} unbound.", self.unbound())
        } else {
            format!("{} actions.", self.rows().len())
        };
        put(buf, y, Line::from(count.dim()));
        y += 2;
        // the tab bar wraps when it does not fit
        let tabs = self.tab_line();
        let tabs_rows = crate::wrap::word_wrap_line(
            &tabs,
            &crate::wrap::WrapOpts::new(inner_w.max(1) as usize),
        );
        for l in tabs_rows {
            put(buf, y, l);
            y += 1;
        }
        y += 1;
        let mut search = vec![Span::from("Type to search shortcuts").dim()];
        if !self.query.is_empty() {
            search = vec![Span::raw(self.query.clone())];
        }
        put(buf, y, Line::from(search));
        y += 1;
        let rows = self.rows();
        if rows.is_empty() {
            let (a, b) = if self.tab == 2 {
                (
                    "No customized shortcuts",
                    "No root-level keymap overrides have been configured.",
                )
            } else {
                ("No shortcuts", "Nothing matches.")
            };
            let l = Line::from(vec![
                Span::styled(format!("› {a}  "), palette().accent()),
                Span::styled(b.to_string(), palette().accent()),
            ]);
            let r = Rect::new(band.x, y, band.width.saturating_sub(2), 1);
            tuikit::paint::put_line(buf, r.x, y, &l, r);
        }
        let ctx_w = 12;
        let name_w = rows
            .iter()
            .map(|b| width_of(&action_label(b.action)))
            .max()
            .unwrap_or(0)
            .max(24);
        let visible = MAX_POPUP_ROWS.min(rows.len());
        let mut start = self.state.scroll_top.min(rows.len().saturating_sub(1));
        if let Some(sel) = self.state.selected_idx {
            if sel < start {
                start = sel;
            } else if visible > 0 && sel > start + visible - 1 {
                start = sel + 1 - visible;
            }
        }
        for (i, b) in rows.iter().enumerate().skip(start).take(visible) {
            let selected = Some(i) == self.state.selected_idx;
            let marker = if b.keys.is_empty() { '-' } else { ' ' };
            let label = action_label(b.action);
            let text = format!(
                "{} {:<ctx_w$} {marker} {label:<name_w$}  {}",
                if selected { '›' } else { ' ' },
                b.context.label(),
                b.summary()
            );
            let line = if selected {
                Line::from(Span::styled(text, palette().accent()))
            } else {
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        format!("{:<ctx_w$} {marker} ", b.context.label()),
                        Style::default().dim(),
                    ),
                    Span::raw(format!("{label:<name_w$}  ")),
                    Span::styled(b.summary(), Style::default().dim()),
                ])
            };
            let ry = y + (i - start) as u16;
            let r = Rect::new(band.x, ry, band.width, 1);
            if ry < band.bottom().saturating_sub(1) {
                tuikit::paint::put_line(buf, r.x, ry, &line, r);
            }
        }
        let hint_y = area.bottom() - 1;
        let key = Style::default()
            .add_modifier(Modifier::BOLD | Modifier::DIM)
            .fg(ratatui::style::Color::Cyan);
        let d = Style::default().dim();
        let hint = Line::from(vec![
            Span::styled("left/right", key),
            Span::styled(" group · ", d),
            Span::styled("enter", key),
            Span::styled(" edit shortcut · ", d),
            Span::styled("*", key),
            Span::styled(" custom · ", d),
            Span::styled("-", key),
            Span::styled(" unbound · ", d),
            Span::styled("esc", key),
            Span::styled(" close", d),
        ]);
        let r = Rect::new(area.x + 2, hint_y, area.width.saturating_sub(2), 1);
        tuikit::paint::put_line(buf, r.x, hint_y, &hint, r);
    }

    fn handle_key(&mut self, key: KeyEvent) -> ViewResult {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let n = self.rows().len();
        match key.code {
            KeyCode::Esc => return ViewResult::Close,
            KeyCode::Char('c') if ctrl => return ViewResult::Close,
            KeyCode::Left => {
                self.tab = (self.tab + TABS.len() - 1) % TABS.len();
                self.refresh();
            }
            KeyCode::Right => {
                self.tab = (self.tab + 1) % TABS.len();
                self.refresh();
            }
            KeyCode::Up => self.state.move_up_wrap(n),
            KeyCode::Down => self.state.move_down_wrap(n),
            KeyCode::Char('p') if ctrl => self.state.move_up_wrap(n),
            KeyCode::Char('n') if ctrl => self.state.move_down_wrap(n),
            KeyCode::Backspace => {
                self.query.pop();
                self.refresh();
            }
            KeyCode::Enter => {
                return ViewResult::CloseWith(AppAction::Error(
                    "Shortcuts cannot be remapped in codexw: they are fixed to Wizard's defaults, which this list shows."
                        .into(),
                ));
            }
            KeyCode::Char(c) if !ctrl => {
                self.query.push(c);
                self.refresh();
            }
            _ => {}
        }
        self.state.ensure_visible(
            self.rows().len(),
            MAX_POPUP_ROWS.min(self.rows().len().max(1)),
        );
        ViewResult::Pending
    }
}

/// `/keymap debug`: names the next key pressed and the actions bound to it.
#[derive(Debug, Default)]
pub struct InspectorView {
    last: Option<Vec<String>>,
}

impl InspectorView {
    /// The view's rows at `width`.
    fn lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = vec![
            Line::from("Keypress Inspector".bold()),
            Line::from(
                "Press any key to see what Wizard receives. Esc is inspected; Ctrl+C closes.".dim(),
            ),
        ];
        match &self.last {
            None => {
                lines.push(Line::from(
                    "Still waiting? If nothing changes when you press a key, your terminal is not sending that key to Wizard. Only received keys can be assigned as shortcuts."
                        .dim(),
                ));
                lines.push(Line::default());
                lines.push(Line::from("Waiting for a keypress...".dim()));
            }
            Some(rows) => {
                lines.push(Line::from(
                    "Tip: Wizard can only inspect keys your terminal sends.".dim(),
                ));
                lines.push(Line::default());
                for r in rows {
                    let l = match r.split_once(": ") {
                        Some((k, v)) if matches!(k, "Detected" | "Config key") => Line::from(vec![
                            Span::styled(format!("{k}: "), Style::default().dim()),
                            Span::styled(
                                v.to_string(),
                                Style::default().fg(ratatui::style::Color::Cyan),
                            ),
                        ]),
                        _ => Line::from(Span::styled(r.clone(), Style::default().dim())),
                    };
                    lines.push(l);
                }
            }
        }
        let opts = crate::wrap::WrapOpts::new(width.max(1) as usize);
        lines
            .iter()
            .flat_map(|l| crate::wrap::word_wrap_line(l, &opts))
            .collect()
    }
}

impl BottomView for InspectorView {
    fn desired_height(&self, width: u16) -> u16 {
        self.lines(width).len() as u16
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        for (i, l) in self.lines(area.width).iter().enumerate() {
            let y = area.y + i as u16;
            if y >= area.bottom() {
                break;
            }
            tuikit::paint::put_line(buf, area.x, y, l, area);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> ViewResult {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return ViewResult::Close;
        }
        self.last = Some(inspect(&key));
        ViewResult::Pending
    }
}
