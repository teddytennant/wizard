// OWNER: dialogs
//! `Plugins` and `Install plugin` from the palette. wizard loads every directory under
//! `~/.wizard/plugins` when it starts, so the list shows what is there; installing goes
//! through `wizard plugins install`, which this UI does not run on your behalf.

use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use tuikit::dialog::{SelectDialog, LARGE};
use tuikit::select::SelectItem;
use tuikit::theme::Variant;
use tuikit::Theme;

use super::list::{Act, ListDialog, ListEvent};
use super::panel::Alert;
use super::wizard_dir;
use super::{Ctx, Dialog, Effect, Nav, Outcome};

pub struct PluginsDialog {
    list: ListDialog<String>,
}

pub fn open(ctx: &Ctx) -> Box<dyn Dialog> {
    let names = ctx
        .wizard_dir
        .as_deref()
        .map(wizard_dir::load_plugins)
        .unwrap_or_default();
    let items = names
        .into_iter()
        .map(|n| {
            SelectItem::new(n.clone(), n)
                .group("Wizard")
                .description("Plugin in ~/.wizard/plugins")
                .hint("active")
        })
        .collect();
    let mut list = ListDialog::new(SelectDialog::new("Plugins", items).width(LARGE));
    list.set_actions(vec![
        Act::new("toggle", "toggle", "space"),
        Act::new("install", "install", "shift+i"),
    ]);
    Box::new(PluginsDialog { list })
}

/// The explanation behind `Install plugin`.
pub fn install() -> Box<dyn Dialog> {
    Box::new(Alert::new(
        "Install plugin",
        "Run this in a terminal: wizard plugins install <package>\nwizard picks plugins up the next time it starts, or on /reload.",
    ))
}

impl PluginsDialog {
    fn pick(&mut self, ev: ListEvent) -> Outcome {
        match ev {
            ListEvent::Cancel => Outcome::close(),
            ListEvent::Action("toggle", _) => Outcome::stay().with(Effect::Toast(
                Variant::Info,
                "wizard loads every directory under ~/.wizard/plugins".into(),
            )),
            ListEvent::Action("install", _) => Outcome {
                nav: Nav::Replace(install()),
                effects: Vec::new(),
            },
            _ => Outcome::stay(),
        }
    }
}

impl Dialog for PluginsDialog {
    fn title(&self) -> &str {
        "Plugins"
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
