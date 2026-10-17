// OWNER: dialogs
//! `/themes`: every built-in theme. Moving the selection previews it live, enter keeps it and
//! stores it, esc restores the one that was active.

use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use tuikit::dialog::SelectDialog;
use tuikit::select::SelectItem;
use tuikit::Theme;

use super::list::{ListDialog, ListEvent};
use super::{Ctx, Dialog, Effect, Nav, Outcome};

pub struct ThemeDialog {
    list: ListDialog<String>,
    original: String,
    previewed: String,
    /// Text colour on the selected row, taken from the theme the dialog opened under: opencode
    /// reads it once, so previewing another theme does not change it.
    selected_fg: Option<ratatui::style::Color>,
}

pub fn names() -> Vec<String> {
    let mut v: Vec<String> = Theme::names().into_iter().map(String::from).collect();
    v.sort_by_key(|n| n.to_lowercase());
    v
}

pub fn open(ctx: &Ctx) -> Box<dyn Dialog> {
    let current = ctx.theme.as_str();
    let items = names()
        .into_iter()
        .map(|n| {
            let cur = n == current;
            SelectItem::new(n.clone(), n).current(cur)
        })
        .collect();
    Box::new(ThemeDialog {
        list: ListDialog::new(SelectDialog::new("Themes", items)).centered(),
        original: current.to_string(),
        previewed: current.to_string(),
        selected_fg: None,
    })
}

impl ThemeDialog {
    /// Effects that bring the screen in line with the highlighted theme. An empty filter goes
    /// back to the original; a filter with no match keeps what is showing.
    fn preview(&mut self) -> Vec<Effect> {
        let want = if self.list.query().trim().is_empty() && self.list.selected_index().is_none() {
            Some(self.original.clone())
        } else {
            self.list
                .selected_index()
                .map(|i| self.list.items()[i].value.clone())
        };
        match want {
            Some(w) if w != self.previewed => {
                self.previewed = w.clone();
                vec![Effect::PreviewTheme(w)]
            }
            _ => Vec::new(),
        }
    }

    fn pick(&mut self, ev: ListEvent) -> Outcome {
        match ev {
            ListEvent::Cancel => {
                Outcome::close_with(vec![Effect::PreviewTheme(self.original.clone())])
            }
            ListEvent::Select(i) => {
                let name = self.list.items()[i].value.clone();
                Outcome::close_with(vec![Effect::SetTheme(name)])
            }
            _ => Outcome {
                nav: Nav::Stay,
                effects: self.preview(),
            },
        }
    }
}

impl Dialog for ThemeDialog {
    fn title(&self) -> &str {
        "Themes"
    }
    fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        let ev = self.list.handle_key(key);
        self.pick(ev)
    }
    fn handle_paste(&mut self, text: &str) -> Outcome {
        let ev = self.list.handle_paste(text);
        self.pick(ev)
    }
    fn handle_mouse(&mut self, ev: MouseEvent) -> Outcome {
        let ev = self.list.handle_mouse(ev);
        self.pick(ev)
    }
    fn draw(&mut self, buf: &mut Buffer, screen: Rect, theme: &Theme) -> Option<(u16, u16)> {
        let fg = *self
            .selected_fg
            .get_or_insert_with(|| theme.selected_foreground(Some(theme.primary)));
        self.list.draw(buf, screen, &theme.with_selected_text(fg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn ctx() -> Ctx {
        Ctx {
            theme: "opencode".into(),
            ..Default::default()
        }
    }

    #[test]
    fn all_33_builtin_themes_are_listed() {
        assert_eq!(names().len(), 33);
    }

    #[test]
    fn moving_previews_and_esc_restores() {
        let mut d = open(&ctx());
        let out = d.handle_key(key(KeyCode::Down));
        assert!(matches!(out.effects.as_slice(), [Effect::PreviewTheme(n)] if n != "opencode"));
        let out = d.handle_key(key(KeyCode::Esc));
        assert_eq!(out.effects, vec![Effect::PreviewTheme("opencode".into())]);
        assert!(matches!(out.nav, Nav::Close));
    }

    #[test]
    fn enter_keeps_the_highlighted_theme() {
        let mut d = open(&ctx());
        let out = d.handle_key(key(KeyCode::Down));
        let Effect::PreviewTheme(name) = out.effects[0].clone() else {
            panic!("no preview")
        };
        let out = d.handle_key(key(KeyCode::Enter));
        assert_eq!(out.effects, vec![Effect::SetTheme(name)]);
    }

    #[test]
    fn typing_previews_the_first_match_and_clearing_restores() {
        let mut d = open(&ctx());
        let mut last = None;
        for c in "tokyo".chars() {
            let out = d.handle_key(key(KeyCode::Char(c)));
            if let Some(Effect::PreviewTheme(n)) = out.effects.first() {
                last = Some(n.clone());
            }
        }
        assert!(last.unwrap().contains("tokyo"));
        let mut restored = false;
        for _ in 0..5 {
            let out = d.handle_key(key(KeyCode::Backspace));
            restored |= out.effects == vec![Effect::PreviewTheme("opencode".into())];
        }
        assert!(restored);
    }
}
