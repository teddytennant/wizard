// OWNER: dialogs
//! `/skills`: `~/.wizard/skills/*/SKILL.md`, one row per skill. Picking one types `/<skill> `
//! into the prompt.

use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use tuikit::dialog::{SelectDialog, LARGE};
use tuikit::paint::put_str;
use tuikit::select::SelectItem;
use tuikit::width::{display_width, pad_right};
use tuikit::Theme;

use super::list::{ListDialog, ListEvent};
use super::wizard_dir;
use super::{Ctx, Dialog, Effect, Outcome};

pub struct SkillDialog {
    list: ListDialog<String>,
    /// A failed load replaces the list with an error and takes the search away.
    error: Option<String>,
}

pub fn open(ctx: &Ctx) -> Box<dyn Dialog> {
    let (skills, error) = match ctx.wizard_dir.as_deref().map(wizard_dir::load_skills) {
        Some(Ok(v)) => (v, None),
        Some(Err(e)) => (Vec::new(), Some(e)),
        None => (Vec::new(), None),
    };
    let width = skills
        .iter()
        .map(|s| display_width(&s.name))
        .max()
        .unwrap_or(0);
    let items = skills
        .iter()
        .map(|s| {
            let mut it = SelectItem::new(s.name.clone(), pad_right(&s.name, width)).group("Skills");
            if !s.description.is_empty() {
                it = it.description(s.description.clone());
            }
            it
        })
        .collect();
    let mut dlg = SelectDialog::new("Skills", items).width(LARGE);
    dlg.placeholder = "Search skills…".into();
    dlg.show_filter = error.is_none();
    let mut list = ListDialog::new(dlg);
    list.locked = error.is_some();
    Box::new(SkillDialog { list, error })
}

impl SkillDialog {
    fn pick(&mut self, ev: ListEvent) -> Outcome {
        match ev {
            ListEvent::Cancel => Outcome::close(),
            ListEvent::Select(i) => {
                let name = self.list.items()[i].value.clone();
                Outcome::close_with(vec![Effect::InsertPrompt(format!("/{name} "))])
            }
            _ => Outcome::stay(),
        }
    }
}

impl Dialog for SkillDialog {
    fn title(&self) -> &str {
        "Skills"
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
        let c = self.list.draw(buf, screen, theme);
        if let Some(e) = &self.error {
            // `renderFilter` is off, so the list's own "No results found" row is replaced.
            let l = tuikit::dialog::layout(screen, LARGE, 5);
            let a = Rect::new(
                l.inner.x + 4,
                l.inner.y + 2,
                l.inner.width.saturating_sub(8),
                2,
            );
            tuikit::paint::fill(buf, a, Style::new().bg(theme.background_panel));
            put_str(
                buf,
                a.x,
                a.y,
                "Could not load skills",
                Style::new()
                    .fg(theme.error)
                    .bg(theme.background_panel)
                    .add_modifier(Modifier::BOLD),
                a,
            );
            put_str(
                buf,
                a.x,
                a.y + 1,
                e,
                Style::new().fg(theme.text_muted).bg(theme.background_panel),
                a,
            );
        }
        c
    }
}
