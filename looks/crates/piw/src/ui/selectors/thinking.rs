//! `/thinking` (spec 11.4), after `thinking-selector.js`. The levels are the backend's own
//! (`Config.efforts`); Pi's descriptions are used where the names match.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::text::Line;

use super::list::{hint, Row, SelectList};
use super::{fuzzy_filter, is_cancel, is_ctrl, Input, SelOut};
use crate::theme::Tok;
use crate::ui::{span, Cx, Lines};

pub fn describe(level: &str) -> Option<&'static str> {
    Some(match level {
        "off" => "No reasoning",
        "minimal" => "Very brief reasoning (~1k tokens)",
        "low" => "Light reasoning (~2k tokens)",
        "medium" => "Moderate reasoning (~8k tokens)",
        "high" => "Deep reasoning (~16k tokens)",
        "xhigh" => "Extra-high reasoning (~32k tokens)",
        "max" => "Maximum reasoning",
        _ => return None,
    })
}

pub struct ThinkingSelector {
    all: Vec<Row>,
    list: SelectList,
    input: Input,
}

impl ThinkingSelector {
    pub fn new(levels: &[String], current: &str) -> Self {
        Self::with_default(levels, current, None)
    }

    /// `default` is the level the backend starts with, marked as Pi marks its saved default.
    pub fn with_default(levels: &[String], current: &str, default: Option<&str>) -> Self {
        let all: Vec<Row> = levels
            .iter()
            .map(|l| Row {
                value: l.clone(),
                label: format!("{}{l}", if l == current { "✓ " } else { "  " }),
                description: match (describe(l), default == Some(l.as_str())) {
                    (Some(d), true) => Some(format!("{d} · default")),
                    (None, true) => Some("default".to_string()),
                    (d, false) => d.map(String::from),
                },
            })
            .collect();
        let mut list = SelectList::new(all.clone(), all.len(), 12, 32);
        list.select_value(current);
        ThinkingSelector {
            all,
            list,
            input: Input::new(),
        }
    }

    fn refilter(&mut self) {
        let keep = self.list.current().map(|r| r.value.clone());
        let q = self.input.text().to_string();
        let items = if q.is_empty() {
            self.all.clone()
        } else {
            fuzzy_filter(&self.all, &q, |r| {
                format!("{} {}", r.value, r.description.clone().unwrap_or_default())
            })
        };
        let n = items.len();
        self.list = SelectList::new(items, n.max(1), 12, 32);
        if let Some(k) = keep {
            self.list.select_value(&k);
        }
    }

    pub fn paste(&mut self, text: &str) {
        self.input.paste(&text.replace(['\n', '\r'], " "));
        self.refilter();
    }

    pub fn on_key(&mut self, key: KeyEvent) -> SelOut {
        if is_ctrl(&key, 's') {
            return SelOut::Warning(
                "wizard keeps its default thinking level in ~/.wizard/config.toml; edit it there."
                    .into(),
            );
        }
        if is_cancel(&key) {
            return SelOut::Close;
        }
        match key.code {
            KeyCode::Up => self.list.up(),
            KeyCode::Down => self.list.down(),
            KeyCode::Enter => {
                if let Some(r) = self.list.current() {
                    return SelOut::SetThinking(r.value.clone());
                }
            }
            _ => {
                if self.input.key(key) {
                    self.refilter();
                }
            }
        }
        SelOut::None
    }

    pub fn render(&self, cx: &Cx) -> Lines {
        let mut body: Lines = vec![
            super::blank_line(),
            Line::from("Thinking Level"),
            super::blank_line(),
            Line::from("Shift+Tab cycles thinking levels in-session"),
            super::blank_line(),
            self.input.render(cx.width),
            super::blank_line(),
        ];
        if self.list.items.is_empty() {
            body.push(Line::from(span(
                "  No matching commands",
                cx.th().fg(Tok::Muted),
            )));
        } else {
            body.extend(self.list.render(cx));
        }
        body.push(super::blank_line());
        body.push(hint(
            cx,
            "Enter to select · Ctrl+S to set as default · Escape/Ctrl+C to cancel",
        ));
        super::frame(cx, Tok::Border, body)
    }
}
