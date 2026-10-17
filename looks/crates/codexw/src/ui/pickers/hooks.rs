//! The `/hooks` events screen (spec C.7.12): one row per lifecycle event with how many hooks
//! are installed and active, from wizard's `hooks.toml` files. Enter on an event that has hooks
//! prints them; wizard has no switch to turn one off, so there is no toggle page.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::outputs::{HOOK_EVENTS, Table, hook_counts, hook_detail};
use crate::style::palette;
use crate::ui::{AppAction, BottomView, ViewResult};

#[derive(Debug)]
pub struct HooksView {
    counts: Vec<(&'static str, usize, usize)>,
    global: Vec<Table>,
    project: Vec<Table>,
    selected: usize,
}

impl HooksView {
    pub fn new(global: Vec<Table>, project: Vec<Table>) -> Self {
        Self {
            counts: hook_counts(&global, &project),
            global,
            project,
            selected: 0,
        }
    }

    /// The hooks of the selected event, as the lines Enter prints.
    fn detail(&self) -> Option<String> {
        let (_, id, _) = HOOK_EVENTS.get(self.selected)?;
        let mut blocks: Vec<String> = Vec::new();
        for (tables, source) in [
            (&self.global, "~/.wizard/hooks.toml"),
            (&self.project, ".wizard/hooks.toml"),
        ] {
            for t in tables.iter().filter(|t| t.get("event") == Some(id)) {
                blocks.push(hook_detail(t, source).join("\n"));
            }
        }
        (!blocks.is_empty()).then(|| {
            format!(
                "{} hooks\n{}",
                HOOK_EVENTS[self.selected].0,
                blocks.join("\n\n")
            )
        })
    }
}

const BODY_ROWS: u16 = 4; // title, subtitle, blank, column header

impl BottomView for HooksView {
    fn desired_height(&self, _width: u16) -> u16 {
        // pad, header block, events, pad, hint
        1 + BODY_ROWS + self.counts.len() as u16 + 1 + 1
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.height < 3 {
            return;
        }
        let band_h = area.height - 1;
        let band = Rect::new(area.x, area.y, area.width, band_h);
        tuikit::paint::set_style(buf, band, palette().user_message_style());
        let mut y = area.y + 1;
        let put = |buf: &mut Buffer, y: u16, line: Line<'static>| {
            if y < band.bottom().saturating_sub(1) {
                let r = Rect::new(band.x + 2, y, band.width.saturating_sub(2), 1);
                tuikit::paint::put_line(buf, r.x, y, &line, r);
            }
        };
        put(
            buf,
            y,
            Line::from(Span::styled("Hooks", Style::default().bold())),
        );
        y += 1;
        put(
            buf,
            y,
            Line::from(Span::styled(
                "Lifecycle hooks from config and enabled plugins.",
                Style::default().dim(),
            )),
        );
        y += 2;
        put(
            buf,
            y,
            Line::from(format!(
                "{:<22}{:<12}{:<12}{}",
                "Event", "Installed", "Active", "Description"
            )),
        );
        y += 1;
        for (i, ((name, installed, active), (_, _, desc))) in
            self.counts.iter().zip(HOOK_EVENTS.iter()).enumerate()
        {
            let rest = format!("{installed:<12}{active:<12}{desc}");
            let line = if i == self.selected {
                Line::from(Span::styled(
                    format!("{name:<22}{rest}"),
                    palette().accent(),
                ))
            } else {
                Line::from(vec![
                    Span::raw(format!("{name:<22}")),
                    Span::styled(rest, Style::default().dim()),
                ])
            };
            put(buf, y, line);
            y += 1;
        }
        let hint_y = area.bottom() - 1;
        let hint = "Press enter to view hooks; esc to close";
        let r = Rect::new(area.x + 2, hint_y, area.width.saturating_sub(2), 1);
        tuikit::paint::put_line(
            buf,
            r.x,
            hint_y,
            &Line::from(Span::styled(hint, Style::default().dim())),
            r,
        );
    }

    fn handle_key(&mut self, key: KeyEvent) -> ViewResult {
        let n = self.counts.len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.selected = (self.selected + n - 1) % n,
            KeyCode::Down | KeyCode::Char('j') => self.selected = (self.selected + 1) % n,
            KeyCode::Esc => return ViewResult::Close,
            KeyCode::Enter => {
                return match self.detail() {
                    Some(text) => ViewResult::CloseWith(AppAction::Info(text)),
                    None => ViewResult::Pending,
                };
            }
            _ => {}
        }
        ViewResult::Pending
    }
}
