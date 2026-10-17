// OWNER: dialogs
//! Stashed prompts, newest first. Enter takes one back into the prompt; `ctrl+d` twice drops it.

use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use tuikit::dialog::SelectDialog;
use tuikit::select::SelectItem;
use tuikit::width::truncate;
use tuikit::Theme;

use super::clock;
use super::list::{Act, ListDialog, ListEvent};
use super::{Ctx, Dialog, Effect, Outcome, StashItem};

pub struct StashDialog {
    list: ListDialog<usize>,
    entries: Vec<StashItem>,
    to_delete: Option<usize>,
    now: i64,
    off: i64,
}

fn build(
    entries: &[StashItem],
    to_delete: Option<usize>,
    now: i64,
    off: i64,
) -> Vec<SelectItem<usize>> {
    entries
        .iter()
        .enumerate()
        .rev()
        .map(|(i, e)| {
            let deleting = to_delete == Some(i);
            let first = e.text.split('\n').next().unwrap_or("").trim();
            let title = if deleting {
                "Press ctrl+d again to confirm".to_string()
            } else {
                truncate(first, 50)
            };
            let mut it = SelectItem::new(i, title)
                .danger(deleting)
                .description(clock::relative(now, e.at, off));
            let lines = e.text.matches('\n').count() + 1;
            if lines > 1 {
                it = it.hint(format!("~{lines} lines"));
            }
            it
        })
        .collect()
}

pub fn open(ctx: &Ctx) -> Box<dyn Dialog> {
    let mut d = StashDialog {
        list: ListDialog::new(SelectDialog::new("Stash", Vec::new())),
        entries: ctx.stash.clone(),
        to_delete: None,
        now: ctx.now,
        off: ctx.utc_offset,
    };
    d.rebuild();
    Box::new(d)
}

impl StashDialog {
    fn rebuild(&mut self) {
        let keep = self
            .list
            .selected_index()
            .map(|i| self.list.items()[i].value);
        self.list
            .dlg
            .state
            .set_items(build(&self.entries, self.to_delete, self.now, self.off));
        if let Some(k) = keep {
            self.list.dlg.state.select_where(|v| *v == k);
        }
        self.list
            .set_actions(vec![Act::new("delete", "delete", "ctrl+d")]);
    }

    fn pick(&mut self, ev: ListEvent) -> Outcome {
        match ev {
            ListEvent::Cancel => Outcome::close(),
            ListEvent::Moved | ListEvent::Filtered => {
                if self.to_delete.take().is_some() {
                    self.rebuild();
                }
                Outcome::stay()
            }
            ListEvent::Select(i) => {
                let idx = self.list.items()[i].value;
                Outcome::close_with(vec![Effect::StashPop(idx)])
            }
            ListEvent::Action("delete", i) => {
                let idx = self.list.items()[i].value;
                if self.to_delete == Some(idx) {
                    self.to_delete = None;
                    self.entries.remove(idx);
                    self.rebuild();
                    Outcome::stay().with(Effect::StashRemove(idx))
                } else {
                    self.to_delete = Some(idx);
                    self.rebuild();
                    Outcome::stay()
                }
            }
            _ => Outcome::stay(),
        }
    }
}

impl Dialog for StashDialog {
    fn title(&self) -> &str {
        "Stash"
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
        self.list.draw(buf, screen, theme)
    }
}
