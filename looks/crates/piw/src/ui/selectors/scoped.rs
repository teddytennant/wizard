//! `/scoped-models` (spec 11.5), after `scoped-models-selector.js`: which models `ctrl+p` cycles,
//! and in which order. Changes apply to the session at once; `ctrl+s` writes `enabledModels`.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use tuikit::width::{wrap_spans, WrapMode};

use agent_core::ModelOption;

use super::{fuzzy_filter, is_cancel, is_ctrl, Input, SelOut};
use crate::theme::Tok;
use crate::ui::{span, Cx, Lines};

const VISIBLE: usize = 8;

pub struct ScopedSelector {
    models: Vec<ModelOption>,
    /// `None` is "all enabled".
    enabled: Option<Vec<String>>,
    input: Input,
    sel: usize,
    dirty: bool,
}

#[derive(Clone)]
struct Item {
    id: String,
    model: Option<ModelOption>,
    enabled: bool,
}

fn normalize(result: Vec<String>, all: &[String]) -> Option<Vec<String>> {
    if result.len() == all.len() && result.iter().all(|i| all.contains(i)) {
        None
    } else {
        Some(result)
    }
}

impl ScopedSelector {
    pub fn new(models: &[ModelOption], enabled: &[String]) -> Self {
        ScopedSelector {
            models: models.to_vec(),
            enabled: if enabled.is_empty() {
                None
            } else {
                Some(enabled.to_vec())
            },
            input: Input::new(),
            sel: 0,
            dirty: false,
        }
    }

    fn all_ids(&self) -> Vec<String> {
        self.models.iter().map(|m| m.id.clone()).collect()
    }

    fn items(&self) -> Vec<Item> {
        let all = self.all_ids();
        let ids: Vec<String> = match &self.enabled {
            None => all,
            Some(en) => {
                let mut v = en.clone();
                v.extend(all.into_iter().filter(|i| !en.contains(i)));
                v
            }
        };
        ids.into_iter()
            .map(|id| Item {
                model: self.models.iter().find(|m| m.id == id).cloned(),
                enabled: self.enabled.as_ref().is_none_or(|e| e.contains(&id)),
                id,
            })
            .collect()
    }

    fn shown(&self) -> Vec<Item> {
        let items = self.items();
        let q = self.input.text();
        if q.is_empty() {
            return items;
        }
        fuzzy_filter(&items, q, |i| match &i.model {
            Some(m) => format!("{} {} {}", m.provider, m.id, m.name),
            None => i.id.clone(),
        })
    }

    fn toggle(&mut self, id: &str) {
        let all = self.all_ids();
        self.enabled = match self.enabled.take() {
            None => Some(all.iter().filter(|i| *i != id).cloned().collect()),
            Some(mut e) => {
                if let Some(p) = e.iter().position(|i| i == id) {
                    e.remove(p);
                    Some(e)
                } else {
                    e.push(id.to_string());
                    normalize(e, &all)
                }
            }
        };
    }

    fn enable_all(&mut self, targets: Option<Vec<String>>) {
        let all = self.all_ids();
        let Some(mut e) = self.enabled.take() else {
            return;
        };
        for id in targets.unwrap_or_else(|| all.clone()) {
            if !e.contains(&id) {
                e.push(id);
            }
        }
        self.enabled = normalize(e, &all);
    }

    fn clear_all(&mut self, targets: Option<Vec<String>>) {
        let all = self.all_ids();
        self.enabled = Some(match (self.enabled.take(), targets) {
            (None, Some(t)) => all.into_iter().filter(|i| !t.contains(i)).collect(),
            (None, None) => Vec::new(),
            (Some(e), t) => {
                let t = t.unwrap_or_else(|| e.clone());
                e.into_iter().filter(|i| !t.contains(i)).collect()
            }
        });
    }

    fn out(&self) -> Vec<String> {
        self.enabled.clone().unwrap_or_default()
    }

    pub fn paste(&mut self, text: &str) {
        self.input.paste(&text.replace(['\n', '\r'], " "));
        self.sel = 0;
    }

    pub fn on_key(&mut self, key: KeyEvent) -> SelOut {
        let shown = self.shown();
        let n = shown.len();
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Up if !alt => {
                if n > 0 {
                    self.sel = if self.sel == 0 { n - 1 } else { self.sel - 1 };
                }
                return SelOut::None;
            }
            KeyCode::Down if !alt => {
                if n > 0 {
                    self.sel = if self.sel + 1 == n { 0 } else { self.sel + 1 };
                }
                return SelOut::None;
            }
            KeyCode::Up | KeyCode::Down => {
                let delta: isize = if key.code == KeyCode::Up { -1 } else { 1 };
                if let (Some(en), Some(item)) = (self.enabled.as_mut(), shown.get(self.sel)) {
                    if let Some(i) = en.iter().position(|x| *x == item.id) {
                        let j = i as isize + delta;
                        if j >= 0 && (j as usize) < en.len() {
                            en.swap(i, j as usize);
                            self.dirty = true;
                            self.sel = (self.sel as isize + delta) as usize;
                            return SelOut::Scoped(self.out());
                        }
                    }
                }
                return SelOut::None;
            }
            KeyCode::Enter => {
                if let Some(item) = shown.get(self.sel) {
                    self.toggle(&item.id);
                    self.dirty = true;
                    return SelOut::Scoped(self.out());
                }
                return SelOut::None;
            }
            _ => {}
        }
        let targets = || {
            (!self.input.is_empty()).then(|| shown.iter().map(|i| i.id.clone()).collect::<Vec<_>>())
        };
        if is_ctrl(&key, 'a') {
            let t = targets();
            self.enable_all(t);
            self.dirty = true;
            return SelOut::Scoped(self.out());
        }
        if is_ctrl(&key, 'x') {
            let t = targets();
            self.clear_all(t);
            self.dirty = true;
            return SelOut::Scoped(self.out());
        }
        if is_ctrl(&key, 'p') {
            if let Some(m) = shown.get(self.sel).and_then(|i| i.model.clone()) {
                let ids: Vec<String> = self
                    .models
                    .iter()
                    .filter(|x| x.provider == m.provider)
                    .map(|x| x.id.clone())
                    .collect();
                let all_on = ids
                    .iter()
                    .all(|i| self.enabled.as_ref().is_none_or(|e| e.contains(i)));
                if all_on {
                    self.clear_all(Some(ids));
                } else {
                    self.enable_all(Some(ids));
                }
                self.dirty = true;
                return SelOut::Scoped(self.out());
            }
            return SelOut::None;
        }
        if is_ctrl(&key, 's') {
            self.dirty = false;
            return SelOut::ScopedSave(self.out());
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if self.input.is_empty() {
                return SelOut::Close;
            }
            self.input.clear();
            return SelOut::None;
        }
        if is_cancel(&key) {
            return SelOut::Close;
        }
        if self.input.key(key) {
            self.sel = self.sel.min(self.shown().len().saturating_sub(1));
        }
        SelOut::None
    }

    fn footer(&self, cx: &Cx) -> Lines {
        let th = cx.th();
        let all = self.models.len();
        let count = match &self.enabled {
            None => "all enabled".to_string(),
            Some(e) => format!(
                "{}/{all} enabled",
                e.iter()
                    .filter(|i| self.models.iter().any(|m| &m.id == *i))
                    .count()
            ),
        };
        let parts = [
            "Enter toggle".to_string(),
            "Ctrl+A all".into(),
            "Ctrl+X clear".into(),
            "Ctrl+P provider".into(),
            "Alt+Up/Alt+Down reorder".into(),
            "Ctrl+S save".into(),
            count,
        ];
        let mut spans: Vec<Span<'static>> =
            vec![span(format!("  {}", parts.join(" · ")), th.fg(Tok::Dim))];
        if self.dirty {
            spans[0] = span(format!("  {} ", parts.join(" · ")), th.fg(Tok::Dim));
            spans.push(span("(unsaved)", th.fg(Tok::Warning)));
        }
        wrap_spans(&spans, cx.width as usize, WrapMode::Word)
            .into_iter()
            .map(Line::from)
            .collect()
    }

    pub fn render(&self, cx: &Cx) -> Lines {
        let th = cx.th();
        let muted = th.fg(Tok::Muted);
        let accent = th.fg(Tok::Accent);
        let mut body: Lines = vec![
            super::blank_line(),
            Line::from(span("Model Configuration", th.bold(Tok::Accent))),
            Line::from(span("Session-only. Ctrl+S to save to settings.", muted)),
            super::blank_line(),
            self.input.render(cx.width),
            super::blank_line(),
        ];
        let shown = self.shown();
        let n = shown.len();
        if n == 0 {
            body.push(Line::from(span("  No matching models", muted)));
        } else {
            let sel = self.sel.min(n - 1);
            let start = sel
                .saturating_sub(VISIBLE / 2)
                .min(n.saturating_sub(VISIBLE));
            let end = (start + VISIBLE).min(n);
            for (i, it) in shown.iter().enumerate().take(end).skip(start) {
                let selected = i == sel;
                let name = it.model.as_ref().map_or(it.id.clone(), |m| {
                    if m.name.is_empty() {
                        m.id.clone()
                    } else {
                        m.name.clone()
                    }
                });
                let mut text_style = if selected { accent } else { Style::default() };
                if it.model.is_none() {
                    text_style = text_style.add_modifier(Modifier::CROSSED_OUT);
                }
                let badge = it.model.as_ref().map_or(" [unavailable]".to_string(), |m| {
                    format!(" [{}]", m.provider)
                });
                body.push(Line::from(vec![
                    if selected {
                        span("→ ", accent)
                    } else {
                        Span::raw("  ")
                    },
                    if it.model.is_some() && it.enabled {
                        span("✓ ", accent)
                    } else {
                        Span::raw("  ")
                    },
                    span(name, text_style),
                    span(badge, muted),
                ]));
            }
            if start > 0 || end < n {
                body.push(Line::from(span(format!("  ({}/{})", sel + 1, n), muted)));
            }
            body.push(super::blank_line());
            let s = &shown[sel];
            body.push(Line::from(span(
                match &s.model {
                    Some(m) => format!(
                        "  Model Name: {}",
                        if m.name.is_empty() { &m.id } else { &m.name }
                    ),
                    None => "  Model unavailable".to_string(),
                },
                muted,
            )));
        }
        body.push(super::blank_line());
        body.extend(self.footer(cx));
        super::frame(cx, Tok::Border, body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::PiTheme;
    use std::time::Duration;

    fn cx() -> Cx {
        Cx {
            theme: PiTheme::dark(),
            width: 120,
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

    fn k(c: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(c, m)
    }

    #[test]
    fn toggling_then_enabling_all_goes_back_to_all() {
        let ms = vec![
            mo("fake-model", "fake"),
            mo("small", "other"),
            mo("big", "other"),
        ];
        let mut s = ScopedSelector::new(&ms, &[]);
        let rows: Vec<String> = s.render(&cx()).iter().map(t).collect();
        assert!(
            rows.contains(&"→ ✓ fake-model [fake]".to_string()),
            "{rows:#?}"
        );
        assert!(
            rows.last().unwrap().len() > 100 || rows.iter().any(|r| r.ends_with("all enabled"))
        );
        s.on_key(k(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(
            s.on_key(k(KeyCode::Enter, KeyModifiers::NONE)),
            SelOut::Scoped(vec!["fake/fake-model".into(), "other/big".into()])
        );
        let rows: Vec<String> = s.render(&cx()).iter().map(t).collect();
        assert!(rows.iter().any(|r| r == "(unsaved)"), "{rows:#?}");
        assert_eq!(
            s.on_key(k(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            SelOut::Scoped(vec![])
        );
    }
}
