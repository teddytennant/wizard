//! `/settings` (spec 11.2), after `settings-selector.js` and pi-tui's `SettingsList`. Only the
//! settings piw owns are listed (spec 14.3); the rest of Pi's 32 rows have nothing to configure
//! on the wizard backend. Every change is emitted as the full new [`Settings`].

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::text::{Line, Span};
use tuikit::width::{display_width, wrap_spans, WrapMode};

use super::list::{hint, Row, SelectList};
use super::{cut, fuzzy_filter, is_cancel, Input, SelOut};
use crate::settings::{Quiet, Scrollbar, Settings};
use crate::theme::Tok;
use crate::ui::{span, Cx, Lines};

const VISIBLE: usize = 10;

#[derive(Clone)]
struct Item {
    id: &'static str,
    label: &'static str,
    desc: &'static str,
    value: String,
    values: &'static [&'static str],
}

const BOOL: &[&str] = &["true", "false"];

fn b(v: bool) -> String {
    v.to_string()
}

fn items(s: &Settings, theme: &str) -> Vec<Item> {
    let it = |id, label, desc, value: String, values| Item {
        id,
        label,
        desc,
        value,
        values,
    };
    vec![
        it(
            "show-hardware-cursor",
            "Show hardware cursor",
            "Show the terminal cursor while still positioning it for IME support",
            b(s.show_hardware_cursor),
            BOOL,
        ),
        it(
            "editor-padding",
            "Editor padding",
            "Horizontal padding for input editor (0-3)",
            s.editor_padding_x.to_string(),
            &["0", "1", "2", "3"],
        ),
        it(
            "output-padding",
            "Output padding",
            "Horizontal padding for user messages, assistant messages, and thinking",
            s.output_padding.to_string(),
            &["0", "1"],
        ),
        it(
            "autocomplete-max-visible",
            "Autocomplete max items",
            "Max visible items in autocomplete dropdown (3-20)",
            s.autocomplete_max_visible.to_string(),
            &["3", "5", "7", "10", "15", "20"],
        ),
        it(
            "hide-thinking",
            "Hide thinking",
            "Hide thinking blocks in assistant responses",
            b(s.hide_thinking_block),
            BOOL,
        ),
        it(
            "collapse-changelog",
            "Collapse changelog",
            "Show condensed changelog after updates",
            b(s.collapse_changelog),
            BOOL,
        ),
        it(
            "quiet-startup",
            "Quiet startup",
            "Disable verbose printing at startup (header: keep only the startup header)",
            match s.quiet_startup {
                Quiet::Off => "false",
                Quiet::Header => "header",
                Quiet::On => "true",
            }
            .into(),
            &["true", "header", "false"],
        ),
        it(
            "fullscreen-scrollbar",
            "Fullscreen scrollbar",
            "Scrollbar behavior in fullscreen mode; has no effect in regular mode",
            match s.fullscreen_scrollbar {
                Scrollbar::Auto => "auto",
                Scrollbar::Always => "always",
                Scrollbar::Hidden => "hidden",
            }
            .into(),
            &["auto", "always", "hidden"],
        ),
        it(
            "fullscreen-copy-on-select",
            "Fullscreen copy on select",
            "Automatically copy selected text in fullscreen mode; disable to copy selections with Ctrl+X",
            b(s.fullscreen_copy_on_select),
            BOOL,
        ),
        it(
            "theme",
            "Theme",
            "Color theme for the interface",
            theme.to_string(),
            &[],
        ),
    ]
}

fn apply(s: &mut Settings, id: &str, v: &str) {
    match id {
        "show-hardware-cursor" => s.show_hardware_cursor = v == "true",
        "editor-padding" => s.editor_padding_x = v.parse().unwrap_or(0),
        "output-padding" => s.output_padding = v.parse().unwrap_or(1),
        "autocomplete-max-visible" => s.autocomplete_max_visible = v.parse().unwrap_or(5),
        "hide-thinking" => s.hide_thinking_block = v == "true",
        "collapse-changelog" => s.collapse_changelog = v == "true",
        "quiet-startup" => {
            s.quiet_startup = match v {
                "true" => Quiet::On,
                "header" => Quiet::Header,
                _ => Quiet::Off,
            }
        }
        "fullscreen-scrollbar" => {
            s.fullscreen_scrollbar = match v {
                "always" => Scrollbar::Always,
                "hidden" => Scrollbar::Hidden,
                _ => Scrollbar::Auto,
            }
        }
        "fullscreen-copy-on-select" => s.fullscreen_copy_on_select = v == "true",
        "theme" => s.theme = Some(v.to_string()),
        _ => {}
    }
}

/// The `Theme` submenu: a titled select list; moving the highlight previews the theme, `escape`
/// puts the old one back. `automatic` (light and dark themes by terminal appearance) is not
/// offered: piw does not read the terminal's colour scheme reports yet.
struct ThemeMenu {
    list: SelectList,
    original: String,
}

fn theme_rows(current: &str) -> Vec<Row> {
    ["system", "dark", "light"]
        .iter()
        .map(|n| Row {
            value: (*n).into(),
            label: format!("{}{n}", if *n == current { "✓ " } else { "  " }),
            description: (*n == "system")
                .then(|| "Theme created from your terminal's colors".into()),
        })
        .collect()
}

pub struct SettingsSelector {
    settings: Settings,
    theme: String,
    items: Vec<Item>,
    input: Input,
    shown: Vec<usize>,
    sel: usize,
    sub: Option<ThemeMenu>,
}

impl SettingsSelector {
    pub fn new(s: &Settings, theme: &str) -> Self {
        let items = items(s, theme);
        SettingsSelector {
            settings: s.clone(),
            theme: theme.into(),
            shown: (0..items.len()).collect(),
            items,
            input: Input::new(),
            sel: 0,
            sub: None,
        }
    }

    fn refilter(&mut self) {
        let q = self.input.text().to_string();
        let idx: Vec<usize> = (0..self.items.len()).collect();
        self.shown = fuzzy_filter(&idx, &q, |i| self.items[*i].label.to_string());
        self.sel = 0;
    }

    pub fn paste(&mut self, text: &str) {
        if self.sub.is_none() {
            self.input.paste(&text.replace(['\n', '\r'], " "));
            self.refilter();
        }
    }

    fn activate(&mut self) -> SelOut {
        let Some(&i) = self.shown.get(self.sel) else {
            return SelOut::None;
        };
        if self.items[i].id == "theme" {
            let mut list = SelectList::new(theme_rows(&self.theme), 3, 12, 32);
            list.select_value(&self.theme);
            self.sub = Some(ThemeMenu {
                list,
                original: self.theme.clone(),
            });
            return SelOut::None;
        }
        let vals = self.items[i].values;
        if vals.is_empty() {
            return SelOut::None;
        }
        let cur = vals
            .iter()
            .position(|v| *v == self.items[i].value)
            .unwrap_or(vals.len() - 1);
        let next = vals[(cur + 1) % vals.len()];
        self.items[i].value = next.to_string();
        apply(&mut self.settings, self.items[i].id, next);
        SelOut::Settings(Box::new(self.settings.clone()))
    }

    pub fn on_key(&mut self, key: KeyEvent) -> SelOut {
        if let Some(m) = &mut self.sub {
            match key.code {
                KeyCode::Up | KeyCode::Down => {
                    if key.code == KeyCode::Up {
                        m.list.up()
                    } else {
                        m.list.down()
                    }
                    let name = m
                        .list
                        .current()
                        .map(|r| r.value.clone())
                        .unwrap_or_default();
                    return SelOut::Theme(name);
                }
                KeyCode::Enter => {
                    let name = m
                        .list
                        .current()
                        .map(|r| r.value.clone())
                        .unwrap_or_default();
                    self.sub = None;
                    self.theme = name.clone();
                    if let Some(it) = self.items.iter_mut().find(|i| i.id == "theme") {
                        it.value = name.clone();
                    }
                    apply(&mut self.settings, "theme", &name);
                    return SelOut::Settings(Box::new(self.settings.clone()));
                }
                _ if is_cancel(&key) => {
                    let orig = m.original.clone();
                    self.sub = None;
                    return SelOut::Theme(orig);
                }
                _ => return SelOut::None,
            }
        }
        let n = self.shown.len();
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
            KeyCode::Enter => return self.activate(),
            KeyCode::Char(' ') if self.input.is_empty() => return self.activate(),
            _ if is_cancel(&key) => return SelOut::Close,
            _ => {
                if !key.modifiers.intersects(KeyModifiers::ALT) && self.input.key(key) {
                    self.refilter();
                }
            }
        }
        SelOut::None
    }

    fn render_sub(&self, cx: &Cx, m: &ThemeMenu) -> Lines {
        let th = cx.th();
        let mut body: Lines = vec![
            Line::from(span("Theme", th.bold(Tok::Accent))),
            super::blank_line(),
            Line::from(span(
                "Select a theme, or choose automatic to follow terminal appearance.",
                th.fg(Tok::Muted),
            )),
            super::blank_line(),
        ];
        body.extend(m.list.render(cx));
        body.push(super::blank_line());
        body.push(hint(cx, "Enter to select · Esc to go back"));
        super::frame(cx, Tok::Border, body)
    }

    pub fn render(&self, cx: &Cx) -> Lines {
        if let Some(m) = &self.sub {
            return self.render_sub(cx, m);
        }
        let th = cx.th();
        let w = cx.width as usize;
        let accent = th.fg(Tok::Accent);
        let mut body: Lines = vec![self.input.render(cx.width), super::blank_line()];
        let label_w = self
            .items
            .iter()
            .map(|i| display_width(i.label))
            .max()
            .unwrap_or(0)
            .min(36);
        let n = self.shown.len();
        if n == 0 {
            body.push(Line::from(span(
                cut("  No matching settings", w),
                th.fg(Tok::Dim),
            )));
        } else {
            let start = self
                .sel
                .saturating_sub(VISIBLE / 2)
                .min(n.saturating_sub(VISIBLE));
            let end = (start + VISIBLE).min(n);
            for (k, &i) in self.shown.iter().enumerate().take(end).skip(start) {
                let it = &self.items[i];
                let selected = k == self.sel;
                let pad = " ".repeat(label_w.saturating_sub(display_width(it.label)));
                let room = w.saturating_sub(2 + label_w + 2 + 2);
                let value = cut(&it.value, room);
                let row: Vec<Span<'static>> = if selected {
                    vec![
                        span(format!("→ {}{pad}", it.label), accent),
                        Span::raw("  "),
                        span(value, accent),
                    ]
                } else {
                    vec![
                        Span::raw(format!("  {}{pad}", it.label)),
                        Span::raw("  "),
                        span(value, th.fg(Tok::Muted)),
                    ]
                };
                body.push(Line::from(tuikit::width::clip_spans(row, w)));
            }
            if start > 0 || end < n {
                body.push(Line::from(span(
                    cut(&format!("  ({}/{})", self.sel + 1, n), w.saturating_sub(2)),
                    th.fg(Tok::Dim),
                )));
            }
            if let Some(&i) = self.shown.get(self.sel) {
                body.push(super::blank_line());
                let d = vec![span(self.items[i].desc, th.fg(Tok::Dim))];
                for r in wrap_spans(&d, w.saturating_sub(4).max(1), WrapMode::Word) {
                    let mut spans = vec![span("  ", th.fg(Tok::Dim))];
                    spans.extend(r);
                    body.push(Line::from(spans));
                }
            }
        }
        body.push(super::blank_line());
        body.push(Line::from(span(
            cut(
                "  Type to search · Enter/Space to change · Esc to cancel",
                w,
            ),
            th.fg(Tok::Dim),
        )));
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

    fn t(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn k(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[test]
    fn rows_align_values_after_the_widest_label() {
        let s = SettingsSelector::new(&Settings::default(), "dark");
        let rows: Vec<String> = s.render(&cx()).iter().map(t).collect();
        assert_eq!(rows[1], ">  ");
        let r = &rows[3];
        assert!(r.starts_with("→ Show hardware cursor"), "{rows:#?}");
        assert_eq!(r[..r.find("false").unwrap()].chars().count(), 2 + 25 + 2);
        assert!(rows
            .iter()
            .any(|r| r == "  Show the terminal cursor while still positioning it for IME support"));
        assert_eq!(rows.last().unwrap(), &"─".repeat(120));
    }

    #[test]
    fn space_cycles_and_emits_settings() {
        let mut s = SettingsSelector::new(&Settings::default(), "dark");
        let SelOut::Settings(n) = s.on_key(k(KeyCode::Char(' '))) else {
            panic!()
        };
        assert!(n.show_hardware_cursor);
        s.on_key(k(KeyCode::Down));
        let SelOut::Settings(n) = s.on_key(k(KeyCode::Enter)) else {
            panic!()
        };
        assert_eq!(n.editor_padding_x, 1);
    }

    #[test]
    fn theme_submenu_previews_and_restores() {
        let mut s = SettingsSelector::new(&Settings::default(), "dark");
        for _ in 0..9 {
            s.on_key(k(KeyCode::Down));
        }
        s.on_key(k(KeyCode::Enter));
        assert_eq!(s.on_key(k(KeyCode::Down)), SelOut::Theme("light".into()));
        assert_eq!(s.on_key(k(KeyCode::Esc)), SelOut::Theme("dark".into()));
    }
}
