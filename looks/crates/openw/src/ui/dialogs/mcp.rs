// OWNER: dialogs
//! `/mcps`: the servers declared in `~/.wizard/mcp.toml`. wizard reads that file when it starts
//! (or on `/reload`) and exposes no connection state over ACP, so the footer says whether a
//! server is enabled in the file, not whether it connected.

use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use tuikit::dialog::SelectDialog;
use tuikit::select::SelectItem;
use tuikit::theme::Variant;
use tuikit::Theme;

use super::list::{Act, ListDialog, ListEvent};
use super::wizard_dir::{self, McpServer};
use super::{Ctx, Dialog, Effect, Outcome};

pub struct McpDialog {
    list: ListDialog<String>,
}

pub fn servers(ctx: &Ctx) -> Vec<McpServer> {
    let mut v = ctx
        .wizard_dir
        .as_deref()
        .map(wizard_dir::load_mcp)
        .unwrap_or_default();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

pub fn open(ctx: &Ctx) -> Box<dyn Dialog> {
    let servers = servers(ctx);
    let items = servers
        .iter()
        .map(|s| {
            let it =
                SelectItem::new(s.name.clone(), s.name.clone()).description(s.transport.clone());
            if s.enabled {
                it.hint("✓ Enabled").hint_success(true)
            } else {
                it.hint("○ Disabled")
            }
        })
        .collect();
    let mut list = ListDialog::new(SelectDialog::new("MCPs", items));
    list.set_actions(vec![Act::new("toggle", "toggle", "space")]);
    Box::new(McpDialog { list })
}

impl McpDialog {
    fn pick(&mut self, ev: ListEvent) -> Outcome {
        match ev {
            ListEvent::Cancel => Outcome::close(),
            ListEvent::Action("toggle", _) => Outcome::stay().with(Effect::Toast(
                Variant::Info,
                "wizard reads ~/.wizard/mcp.toml at start; edit it and run /reload".into(),
            )),
            _ => Outcome::stay(),
        }
    }
}

impl Dialog for McpDialog {
    fn title(&self) -> &str {
        "MCPs"
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
