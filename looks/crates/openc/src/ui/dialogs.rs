// OWNER: dialogs
//! Overlays: command palette, session list, theme list, history search, the `ctrl+t` settings
//! popover and the key help. Lists are `tuikit` select dialogs themed from the palette.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use tuikit::dialog::{FooterHint, SelectDialog};
use tuikit::paint::fill;
use tuikit::paint::put_str;
use tuikit::select::{SelectEvent, SelectItem};
use tuikit::theme::Theme;
use tuikit::width::display_width;

use crate::keys::{Action, Scope, BINDINGS};
use crate::palette::{Glyphs, Palette};
use crate::ui::resume::{Resume, ResumeOut};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelKind {
    Palette,
    Theme,
    History,
}

pub enum Overlay {
    Select {
        kind: SelKind,
        dlg: Box<SelectDialog<String>>,
    },
    /// `ctrl+t`: the focused row is 0 model, 1 effort, 2 mode.
    Settings {
        row: usize,
    },
    Help {
        scroll: usize,
    },
    Resume(Box<Resume>),
}

#[derive(Debug, PartialEq, Eq)]
pub enum OvOut {
    None,
    Close,
    /// A list item was chosen.
    Pick(SelKind, String),
    /// The highlighted theme changed (live preview).
    Preview(String),
    /// Something happened in the resume dialog that the app has to act on.
    Resume(ResumeOut),
    /// Settings row changed by `delta` steps.
    Step {
        row: usize,
        delta: i32,
    },
}

/// What the settings popover shows.
#[derive(Default)]
pub struct SettingsView {
    /// `(label, current value, can change)` for model, effort and mode.
    pub rows: Vec<(String, String, bool)>,
    /// What each row can be set to, `(name, is current)`, read from the backend's `Config`.
    pub options: Vec<Vec<(String, bool)>>,
}

pub struct OvCtx<'a> {
    pub p: &'a Palette,
    pub theme: &'a Theme,
    pub g: &'a Glyphs,
    pub settings: &'a SettingsView,
}

/// The one width rule for every dialog (finding 11 found 56, 80, 100 and 112).
pub(crate) fn dlg_width(screen: Rect) -> u16 {
    screen.width.saturating_sub(8).min(80)
}

/// What every list dialog shares: a `▸` on the selected row, no `esc` in the title (the footer
/// says it), a counter in its place, a bar in front of the search field and footers that read
/// `key label` with the key in bold.
fn house_style<T>(mut dlg: SelectDialog<T>, g: &Glyphs) -> SelectDialog<T> {
    dlg.title_hint = String::new();
    dlg.key_first = true;
    dlg.filter_bar = Some(g.bar);
    dlg.style.marker = Some(g.select);
    dlg
}

pub fn palette_items(cmds: &[crate::commands::Cmd]) -> Vec<SelectItem<String>> {
    let mut items: Vec<SelectItem<String>> = Vec::new();
    // Composer actions that are worth finding without knowing the key.
    let wanted = |bd: &crate::keys::Binding| {
        bd.scope == Scope::Global
            || matches!(
                bd.action,
                Action::HistorySearch | Action::ExternalEditor | Action::PasteImage
            )
    };
    for bd in BINDINGS.iter().filter(|b| wanted(b)) {
        if matches!(
            bd.action,
            Action::CtrlC
                | Action::CtrlD
                | Action::Esc
                | Action::PageUp
                | Action::PageDown
                | Action::Palette
        ) {
            continue;
        }
        let key = bd.keys.split(' ').next().unwrap_or("");
        items.push(
            SelectItem::new(format!("act:{:?}", bd.action), capitalize(bd.help))
                .hint(key)
                .group("Actions"),
        );
    }
    for c in cmds {
        let mut it =
            SelectItem::new(format!("/{}", c.name), format!("/{}", c.name)).group("Commands");
        if !c.desc.is_empty() {
            it = it.description(c.desc.clone());
        }
        items.push(it);
    }
    items
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or(String::new(), |f| {
        f.to_uppercase().collect::<String>() + c.as_str()
    })
}

pub fn palette(cmds: &[crate::commands::Cmd]) -> Overlay {
    let mut dlg = house_style(
        SelectDialog::new("Commands", palette_items(cmds)).footer(vec![
            FooterHint::new("run", "enter"),
            FooterHint::new("close", "esc"),
        ]),
        &crate::palette::UNICODE,
    );
    dlg.placeholder = "Search commands".into();
    Overlay::Select {
        kind: SelKind::Palette,
        dlg: Box::new(dlg),
    }
}

pub fn themes(current: &str) -> Overlay {
    let items = ["auto", "hearth", "parchment"]
        .iter()
        .map(|n| {
            let mut it = SelectItem::new((*n).to_string(), *n);
            if *n == current {
                it = it.hint("current");
            }
            it
        })
        .collect();
    let mut dlg = house_style(
        SelectDialog::new("Theme", items).footer(vec![
            FooterHint::new("apply", "enter"),
            FooterHint::new("close", "esc"),
        ]),
        &crate::palette::UNICODE,
    );
    dlg.show_filter = false;
    Overlay::Select {
        kind: SelKind::Theme,
        dlg: Box::new(dlg),
    }
}

pub fn history<'a>(entries: impl Iterator<Item = &'a str>) -> Overlay {
    let mut seen = std::collections::HashSet::new();
    let items: Vec<SelectItem<String>> = entries
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .filter(|e| seen.insert(*e))
        .take(300)
        .map(|e| {
            let first = e.lines().next().unwrap_or("").to_string();
            let it = SelectItem::new(e.to_string(), first);
            match e.lines().count() {
                0 | 1 => it,
                n => it.hint(format!("+{} lines", n - 1)),
            }
        })
        .collect();
    let mut dlg = house_style(
        SelectDialog::new("History", items).footer(vec![
            FooterHint::new("paste", "enter tab"),
            FooterHint::new("close", "esc"),
        ]),
        &crate::palette::UNICODE,
    );
    dlg.placeholder = "Search history".into();
    Overlay::Select {
        kind: SelKind::History,
        dlg: Box::new(dlg),
    }
}

/// The key's code, or `Null` when ctrl or alt is held so a bare-letter arm cannot claim it.
fn plain_code(key: &KeyEvent) -> KeyCode {
    if matches!(key.code, KeyCode::Char(_))
        && key.modifiers.intersects(
            crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::ALT,
        )
    {
        KeyCode::Null
    } else {
        key.code
    }
}

impl Overlay {
    pub fn on_key(&mut self, key: KeyEvent) -> OvOut {
        match self {
            Overlay::Select { kind, dlg } => {
                // Tab puts a history entry in the composer, like enter does; both leave it
                // there to edit before it is sent.
                if *kind == SelKind::History && key.code == KeyCode::Tab {
                    return dlg
                        .state
                        .selected()
                        .map_or(OvOut::None, |it| OvOut::Pick(*kind, it.value.clone()));
                }
                let before = dlg.state.selected().map(|s| s.value.clone());
                match dlg.handle_key(key) {
                    SelectEvent::Cancel => OvOut::Close,
                    SelectEvent::Submit(i) => dlg
                        .state
                        .items()
                        .get(i)
                        .map_or(OvOut::Close, |it| OvOut::Pick(*kind, it.value.clone())),
                    SelectEvent::Changed | SelectEvent::Ignored => {
                        let now = dlg.state.selected().map(|s| s.value.clone());
                        if *kind == SelKind::Theme && now != before {
                            if let Some(v) = now {
                                return OvOut::Preview(v);
                            }
                        }
                        OvOut::None
                    }
                }
            }
            // Letters are vim keys only when typed plain: ctrl+l is a repaint and ctrl+h, ctrl+j
            // and ctrl+k are not arrows.
            Overlay::Settings { row } => match plain_code(&key) {
                KeyCode::Esc | KeyCode::Enter => OvOut::Close,
                KeyCode::Up | KeyCode::Char('k') => {
                    *row = (*row + 2) % 3;
                    OvOut::None
                }
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                    *row = (*row + 1) % 3;
                    OvOut::None
                }
                KeyCode::Left | KeyCode::Char('h') => OvOut::Step {
                    row: *row,
                    delta: -1,
                },
                KeyCode::Right | KeyCode::Char('l') => OvOut::Step {
                    row: *row,
                    delta: 1,
                },
                _ => OvOut::None,
            },
            Overlay::Resume(d) => match d.on_key(key) {
                ResumeOut::None => OvOut::None,
                ResumeOut::Close => OvOut::Close,
                o => OvOut::Resume(o),
            },
            Overlay::Help { scroll } => match plain_code(&key) {
                KeyCode::Down | KeyCode::Char('j') => {
                    *scroll += 1;
                    OvOut::None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    *scroll = scroll.saturating_sub(1);
                    OvOut::None
                }
                KeyCode::PageDown => {
                    *scroll += 10;
                    OvOut::None
                }
                KeyCode::PageUp => {
                    *scroll = scroll.saturating_sub(10);
                    OvOut::None
                }
                _ => OvOut::Close,
            },
        }
    }

    pub fn on_mouse(&mut self, ev: crossterm::event::MouseEvent) -> OvOut {
        if let Overlay::Help { scroll } = self {
            match ev.kind {
                crossterm::event::MouseEventKind::ScrollDown => *scroll += 3,
                crossterm::event::MouseEventKind::ScrollUp => *scroll = scroll.saturating_sub(3),
                _ => {}
            }
            return OvOut::None;
        }
        if let Overlay::Resume(d) = self {
            return match d.on_mouse(ev) {
                ResumeOut::None => OvOut::None,
                ResumeOut::Close => OvOut::Close,
                o => OvOut::Resume(o),
            };
        }
        if let Overlay::Select { kind, dlg } = self {
            return match dlg.handle_mouse(ev) {
                SelectEvent::Submit(i) => dlg
                    .state
                    .items()
                    .get(i)
                    .map_or(OvOut::None, |it| OvOut::Pick(*kind, it.value.clone())),
                SelectEvent::Cancel => OvOut::Close,
                _ => OvOut::None,
            };
        }
        OvOut::None
    }

    pub fn paste(&mut self, s: &str) {
        match self {
            Overlay::Select { dlg, .. } => dlg.state.paste_query(s),
            Overlay::Resume(d) => d.paste(s),
            _ => {}
        }
    }

    /// Paint over the screen; returns where the caret goes, if there is a text field.
    pub fn draw(&mut self, buf: &mut Buffer, screen: Rect, cx: &OvCtx) -> Option<(u16, u16)> {
        match self {
            Overlay::Select { dlg, .. } => {
                dlg.width = dlg_width(screen);
                dlg.title_hint = match dlg.state.len() {
                    0 => String::new(),
                    n => format!("{} of {}", dlg.state.position() + 1, n),
                };
                dlg.style.marker = Some(cx.g.select);
                dlg.filter_bar = Some(cx.g.bar);
                dlg.render(buf, screen, cx.theme).cursor
            }
            Overlay::Settings { row } => {
                crate::ui::settings::draw(buf, screen, *row, cx);
                None
            }
            Overlay::Help { scroll } => {
                crate::ui::help::draw(buf, screen, scroll, cx);
                None
            }
            Overlay::Resume(d) => d.draw(buf, screen, cx),
        }
    }
}

/// Dim the screen and paint the one dialog frame: placement, padding and background come from
/// `tuikit::dialog`, the same code the list dialogs use, so the five dialogs line up. `content_h`
/// counts the rows under the top padding row. Returns the panel; the title is on `r.y + 1`.
pub(crate) fn panel(buf: &mut Buffer, screen: Rect, content_h: u16, cx: &OvCtx) -> Rect {
    tuikit::dialog::render_frame(buf, screen, cx.theme, dlg_width(screen), content_h).outer
}

/// Like [`panel`] for a page that wants most of the screen (the key help): it is centred and
/// leaves one row above and below instead of sitting a quarter of the way down.
pub(crate) fn tall_panel(buf: &mut Buffer, screen: Rect, content_h: u16, cx: &OvCtx) -> Rect {
    tuikit::dialog::dim(buf, screen, cx.theme);
    let w = dlg_width(screen).min(screen.width.saturating_sub(2)).max(1);
    let h = (content_h + 1).min(screen.height.saturating_sub(2)).max(1);
    let x = screen.x + screen.width.saturating_sub(w).div_ceil(2);
    let y = screen.y + screen.height.saturating_sub(h) / 2;
    let r = Rect::new(x, y, w, h);
    fill(buf, r, Style::new().bg(cx.p.raised).fg(cx.p.text));
    r
}

/// Cells between the panel edge and its text.
pub(crate) const INSET: u16 = 4;

/// The bold title on the panel's first content row.
pub(crate) fn title(buf: &mut Buffer, r: Rect, text: &str, cx: &OvCtx) {
    tuikit::dialog::title_row(buf, Rect::new(r.x, r.y + 1, r.width, 1), cx.theme, text, "");
}

/// Footer hints split into rows of whole hints that fit between the insets.
pub(crate) fn footer_lines<'a>(
    width: u16,
    hints: &[(&'a str, &'a str)],
) -> Vec<Vec<(&'a str, &'a str)>> {
    let max = width.saturating_sub(2 * INSET) as usize;
    let mut lines: Vec<Vec<(&str, &str)>> = vec![Vec::new()];
    let mut used = 0usize;
    for (k, d) in hints {
        let need = display_width(k) + 1 + display_width(d);
        let gap = if used == 0 { 0 } else { 3 };
        if used > 0 && used + gap + need > max {
            lines.push(Vec::new());
            used = 0;
        }
        used += if used == 0 { 0 } else { 3 } + need;
        lines.last_mut().expect("a line").push((k, d));
    }
    lines
}

/// Footer hints as `key label`, key in bold `text`, label `dim`, three apart. The last row is
/// on `y`; when the hints need more than one row the earlier ones go above it, so a caller
/// reserves `footer_lines(..).len()` rows. Whole hints only: one is never cut.
pub(crate) fn footer(buf: &mut Buffer, r: Rect, y: u16, hints: &[(&str, &str)], cx: &OvCtx) {
    let bg = cx.p.raised;
    let lines = footer_lines(r.width, hints);
    let n = lines.len() as u16;
    for (i, line) in lines.iter().enumerate() {
        let row = (y + 1 + i as u16).saturating_sub(n);
        let mut x = r.x + INSET;
        for (k, d) in line {
            x = put_str(
                buf,
                x,
                row,
                k,
                Style::new()
                    .fg(cx.p.text)
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
                r,
            );
            x = put_str(buf, x + 1, row, d, Style::new().fg(cx.p.dim).bg(bg), r) + 3;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[test]
    fn palette_filters_and_submits_a_command() {
        let cmds = crate::commands::registry(&[]);
        let mut ov = palette(&cmds);
        for ch in "theme".chars() {
            ov.on_key(key(KeyCode::Char(ch)));
        }
        let out = ov.on_key(key(KeyCode::Enter));
        assert!(
            matches!(&out, OvOut::Pick(SelKind::Palette, v) if v == "/theme" || v.starts_with("act:")),
            "{out:?}"
        );
    }

    #[test]
    fn footer_hints_wrap_whole_and_none_is_lost() {
        let hints = [
            ("enter", "resume"),
            ("space", "preview"),
            ("ctrl+r", "rename"),
            ("ctrl+d", "delete"),
            ("ctrl+a", "all projects"),
            ("ctrl+b", "branch"),
        ];
        for w in [36u16, 60, 80, 100] {
            let lines = footer_lines(w, &hints);
            let flat: Vec<_> = lines.iter().flatten().copied().collect();
            assert_eq!(flat, hints, "{w}: every hint survives, in order");
            for l in &lines {
                let used: usize = l
                    .iter()
                    .map(|(k, d)| display_width(k) + 1 + display_width(d))
                    .sum::<usize>()
                    + 3 * l.len().saturating_sub(1);
                assert!(
                    used <= (w - 2 * INSET) as usize || l.len() == 1,
                    "{w}: {l:?}"
                );
            }
        }
        assert_eq!(footer_lines(80, &hints).len(), 2);
        assert_eq!(
            footer_lines(120, &hints).len(),
            2.min(footer_lines(120, &hints).len())
        );
    }

    #[test]
    fn settings_rows_step_and_close() {
        let mut ov = Overlay::Settings { row: 0 };
        assert_eq!(
            ov.on_key(key(KeyCode::Right)),
            OvOut::Step { row: 0, delta: 1 }
        );
        ov.on_key(key(KeyCode::Down));
        assert_eq!(
            ov.on_key(key(KeyCode::Left)),
            OvOut::Step { row: 1, delta: -1 }
        );
        assert_eq!(ov.on_key(key(KeyCode::Enter)), OvOut::Close);
    }
}
