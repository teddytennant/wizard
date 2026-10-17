//! `/model` and `ctrl+l` (spec 11.3), after `model-selector.js`. Fed from `Config.models`; the
//! catalogue refresh status line has no counterpart over ACP and is left out, the blank rows
//! around it stay.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use agent_core::ModelOption;

use super::{fuzzy_filter, is_cancel, is_ctrl, Input, SelOut};
use crate::theme::Tok;
use crate::ui::{span, Cx, Lines};

const VISIBLE: usize = 10;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    All,
    Scoped,
}

pub struct ModelSelector {
    all: Vec<ModelOption>,
    scoped: Vec<ModelOption>,
    current: String,
    /// The model wizard starts with, which Pi would call the saved default.
    default: Option<String>,
    scope: Scope,
    input: Input,
    shown: Vec<ModelOption>,
    sel: usize,
}

impl ModelSelector {
    pub fn new(mut models: Vec<ModelOption>, current: &str, scoped: &[String]) -> Self {
        // the current model first, then by provider (stable)
        models.sort_by_key(|m| (m.id != current, m.provider.clone()));
        let scoped_models: Vec<ModelOption> = scoped
            .iter()
            .filter_map(|id| models.iter().find(|m| &m.id == id).cloned())
            .collect();
        let scope = if scoped_models.is_empty() {
            Scope::All
        } else {
            Scope::Scoped
        };
        let mut s = ModelSelector {
            all: models,
            scoped: scoped_models,
            current: current.to_string(),
            default: None,
            scope,
            input: Input::new(),
            shown: Vec::new(),
            sel: 0,
        };
        s.refilter(false);
        s.sel = s.shown.iter().position(|m| m.id == s.current).unwrap_or(0);
        s
    }

    /// Mark the model the backend starts with, as Pi marks its saved default.
    pub fn set_default(&mut self, id: Option<&str>) {
        self.default = id.map(String::from);
    }

    fn active(&self) -> &[ModelOption] {
        match self.scope {
            Scope::All => &self.all,
            Scope::Scoped => &self.scoped,
        }
    }

    fn refilter(&mut self, reset: bool) {
        let q = self.input.text().to_string();
        self.shown = if q.is_empty() {
            self.active().to_vec()
        } else {
            fuzzy_filter(self.active(), &q, |m| {
                format!("{} {} {}", m.provider, m.id, m.name)
            })
        };
        self.sel = if reset || !q.is_empty() {
            0
        } else {
            self.sel.min(self.shown.len().saturating_sub(1))
        };
    }

    pub fn paste(&mut self, text: &str) {
        self.input.paste(&text.replace(['\n', '\r'], " "));
        self.refilter(false);
    }

    pub fn on_key(&mut self, key: KeyEvent) -> SelOut {
        let n = self.shown.len();
        if key.code == KeyCode::Tab {
            if !self.scoped.is_empty() {
                self.scope = if self.scope == Scope::All {
                    Scope::Scoped
                } else {
                    Scope::All
                };
                self.refilter(false);
                self.sel = self
                    .shown
                    .iter()
                    .position(|m| m.id == self.current)
                    .unwrap_or(0);
            }
            return SelOut::None;
        }
        if is_cancel(&key) {
            return SelOut::Close;
        }
        match key.code {
            KeyCode::Up => {
                if n > 0 {
                    self.sel = if self.sel == 0 { n - 1 } else { self.sel - 1 };
                }
            }
            KeyCode::Down => {
                if n > 0 {
                    self.sel = if self.sel + 1 == n { 0 } else { self.sel + 1 };
                }
            }
            KeyCode::Enter => {
                if let Some(m) = self.shown.get(self.sel) {
                    return SelOut::SetModel(m.id.clone());
                }
            }
            _ if is_ctrl(&key, 's') => {
                return SelOut::Warning(
                    "wizard keeps its default model in ~/.wizard/config.toml; edit it there."
                        .into(),
                )
            }
            _ => {
                if self.input.key(key) {
                    self.refilter(false);
                }
            }
        }
        SelOut::None
    }

    pub fn render(&self, cx: &Cx, _screen_h: u16) -> Lines {
        let th = cx.th();
        let muted = th.fg(Tok::Muted);
        let accent = th.fg(Tok::Accent);
        let mut body: Lines = vec![super::blank_line()];
        if self.scoped.is_empty() {
            body.push(Line::from(span(
                "Only showing models from configured providers. Use /login to add providers.",
                th.fg(Tok::Warning),
            )));
        } else {
            let on = |s: Scope| if self.scope == s { accent } else { muted };
            body.push(Line::from(vec![
                span("Scope: ", muted),
                span("all", on(Scope::All)),
                span(" | ", muted),
                span("scoped", on(Scope::Scoped)),
            ]));
            body.push(Line::from(vec![
                span("tab", th.fg(Tok::Dim)),
                span(" scope", muted),
                span(" (all/scoped)", muted),
            ]));
        }
        body.push(super::blank_line());
        body.push(self.input.render(cx.width));
        body.push(super::blank_line());
        let n = self.shown.len();
        let start = self
            .sel
            .saturating_sub(VISIBLE / 2)
            .min(n.saturating_sub(VISIBLE));
        let end = (start + VISIBLE).min(n);
        for (i, m) in self.shown.iter().enumerate().take(end).skip(start) {
            let selected = i == self.sel;
            let cur = m.id == self.current;
            let mut spans: Vec<Span<'static>> = vec![
                if selected {
                    span("→ ", accent)
                } else {
                    Span::raw("  ")
                },
                if cur {
                    span("✓ ", accent)
                } else {
                    Span::raw("  ")
                },
            ];
            let name = if m.name.is_empty() { &m.id } else { &m.name };
            spans.push(span(
                name.clone(),
                if selected { accent } else { Style::default() },
            ));
            let is_default = self.default.as_deref() == Some(m.id.as_str());
            if !m.provider.is_empty() || is_default {
                let mut badge = if m.provider.is_empty() {
                    String::new()
                } else {
                    format!("[{}]", m.provider)
                };
                if is_default {
                    badge.push_str(" · default");
                }
                spans.push(Span::raw(" "));
                spans.push(span(badge, muted));
            }
            body.push(Line::from(spans));
        }
        if start > 0 || end < n {
            body.push(Line::from(span(
                format!("  ({}/{})", self.sel + 1, n),
                muted,
            )));
        }
        match self.shown.get(self.sel) {
            None => body.push(Line::from(span("  No matching models", muted))),
            Some(m) => {
                body.push(super::blank_line());
                let name = if m.name.is_empty() { &m.id } else { &m.name };
                body.push(Line::from(span(format!("  Model Name: {name}"), muted)));
            }
        }
        body.push(super::blank_line());
        body.push(super::list::hint(
            cx,
            "Enter to select · Ctrl+S to set as default · Escape/Ctrl+C to cancel",
        ));
        super::frame(cx, Tok::Border, body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::PiTheme;
    use crossterm::event::KeyModifiers;
    use std::time::Duration;

    fn cx() -> Cx {
        Cx {
            theme: PiTheme::dark(),
            width: 100,
            expanded: false,
            hide_thinking: false,
            out_pad: 1,
            cwd: String::new(),
            home: String::new(),
            clock: Duration::ZERO,
            version: "1.0.3".into(),
        }
    }

    fn mo(id: &str, p: &str) -> ModelOption {
        ModelOption {
            id: format!("{p}/{id}"),
            name: id.into(),
            provider: p.into(),
        }
    }

    fn t(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn k(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[test]
    fn rows_mark_current_and_filter_and_pick() {
        let models = vec![
            mo("small", "other"),
            mo("fake-model", "fake"),
            mo("big", "other"),
        ];
        let mut s = ModelSelector::new(models, "fake/fake-model", &[]);
        let rows: Vec<String> = s.render(&cx(), 36).iter().map(t).collect();
        assert!(
            rows.contains(&"→ ✓ fake-model [fake]".to_string()),
            "{rows:#?}"
        );
        assert!(rows.contains(&"    small [other]".to_string()));
        assert!(rows.contains(&"  Model Name: fake-model".to_string()));
        s.on_key(k(KeyCode::Char('b')));
        s.on_key(k(KeyCode::Char('i')));
        assert_eq!(
            s.on_key(k(KeyCode::Enter)),
            SelOut::SetModel("other/big".into())
        );
    }

    #[test]
    fn scope_shows_when_models_are_scoped_and_tab_toggles() {
        let models = vec![mo("a", "p"), mo("b", "p")];
        let mut s = ModelSelector::new(models, "p/a", &["p/b".into()]);
        let rows: Vec<String> = s.render(&cx(), 36).iter().map(t).collect();
        assert!(rows.contains(&"Scope: all | scoped".to_string()));
        assert!(rows.contains(&"tab scope (all/scoped)".to_string()));
        s.on_key(k(KeyCode::Tab));
        assert_eq!(s.shown.len(), 2);
    }
}
