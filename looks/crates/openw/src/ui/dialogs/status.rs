// OWNER: dialogs
//! `/status`: what openw can say about the backend it is talking to. Sections follow
//! opencode's (`N MCP Servers`, `No Formatters`, `No Plugins`) with wizard's own facts first.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use tuikit::dialog::MEDIUM;
use tuikit::paint::put_str;
use tuikit::width::{truncate, wrap};
use tuikit::Theme;

use super::panel::frame;
use super::wizard_dir;
use super::{Ctx, Dialog, Outcome};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dot {
    Success,
    Muted,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub dot: Dot,
    pub name: String,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Section {
    /// `3 MCP Servers`, or the fallback line when there are no rows.
    pub title: String,
    pub rows: Vec<Row>,
}

pub struct StatusDialog {
    sections: Vec<Section>,
}

pub fn sections(ctx: &Ctx) -> Vec<Section> {
    let c = &ctx.config;
    let mut v = Vec::new();
    let mut rows = Vec::new();
    let mut add = |name: &str, detail: &str| {
        if !detail.is_empty() {
            rows.push(Row {
                dot: Dot::Success,
                name: name.into(),
                detail: detail.into(),
            });
        }
    };
    add("Model", &c.model);
    add("Directory", &ctx.cwd);
    add("Mode", &c.mode);
    add("Effort", &c.effort);
    v.push(Section {
        title: if c.backend.is_empty() {
            "wizard".into()
        } else {
            c.backend.clone()
        },
        rows,
    });
    let mcp = super::mcp::servers(ctx);
    v.push(Section {
        title: match mcp.len() {
            0 => "No MCP Servers".into(),
            // `~/.wizard/mcp.toml` says what is configured; nothing here knows what connected
            n => format!("{n} MCP Servers configured"),
        },
        rows: mcp
            .iter()
            .map(|s| Row {
                dot: Dot::Muted,
                name: s.name.clone(),
                detail: if s.enabled {
                    format!("{} {}", s.transport, s.target).trim().to_string()
                } else {
                    "Disabled in configuration".into()
                },
            })
            .collect(),
    });
    // opencode lists enabled formatters between MCP and plugins; wizard has none to report
    v.push(Section {
        title: "No Formatters".into(),
        rows: Vec::new(),
    });
    let plugins = ctx
        .wizard_dir
        .as_deref()
        .map(wizard_dir::load_plugins)
        .unwrap_or_default();
    v.push(Section {
        title: match plugins.len() {
            0 => "No Plugins".into(),
            n => format!("{n} Plugins"),
        },
        rows: plugins
            .into_iter()
            .map(|n| Row {
                dot: Dot::Success,
                name: n,
                detail: String::new(),
            })
            .collect(),
    });
    v
}

pub fn open(ctx: &Ctx) -> Box<dyn Dialog> {
    Box::new(StatusDialog {
        sections: sections(ctx),
    })
}

impl Dialog for StatusDialog {
    fn title(&self) -> &str {
        "Status"
    }

    fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Esc => Outcome::close(),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Outcome::close(),
            _ => Outcome::stay(),
        }
    }

    fn draw(&mut self, buf: &mut Buffer, screen: Rect, theme: &Theme) -> Option<(u16, u16)> {
        let w = MEDIUM.min(screen.width.saturating_sub(2));
        let text_w = w.saturating_sub(4) as usize;
        // Each row is `• name detail`, wrapped by word under the bullet.
        struct Laid {
            title: String,
            rows: Vec<(Row, Vec<String>)>,
        }
        let laid: Vec<Laid> = self
            .sections
            .iter()
            .map(|s| Laid {
                title: truncate(&s.title, text_w),
                rows: s
                    .rows
                    .iter()
                    .map(|r| {
                        let full = if r.detail.is_empty() {
                            r.name.clone()
                        } else {
                            format!("{} {}", r.name, r.detail)
                        };
                        (r.clone(), wrap(&full, text_w.saturating_sub(2).max(1)))
                    })
                    .collect(),
            })
            .collect();
        let body_rows: u16 = laid
            .iter()
            .map(|s| 1 + s.rows.iter().map(|(_, l)| l.len() as u16).sum::<u16>())
            .sum::<u16>()
            + laid.len().saturating_sub(1) as u16;
        // header, gap, sections with a gap between them, bottom padding
        let content_h = 1 + 1 + body_rows + 1;
        let (_, body) = frame(buf, screen, theme, MEDIUM, content_h, "Status", "esc");
        let panel = theme.background_panel;
        let mut y = body.y;
        for (i, s) in laid.iter().enumerate() {
            if i > 0 {
                y += 1;
            }
            put_str(
                buf,
                body.x,
                y,
                &s.title,
                Style::new().fg(theme.text).bg(panel),
                body,
            );
            y += 1;
            for (r, lines) in &s.rows {
                let dot = match r.dot {
                    Dot::Success => theme.success,
                    Dot::Muted => theme.text_muted,
                };
                for (li, l) in lines.iter().enumerate() {
                    if li == 0 {
                        put_str(buf, body.x, y, "•", Style::new().fg(dot).bg(panel), body);
                        // the name is bold, the rest muted
                        let name_len = r.name.chars().count().min(l.chars().count());
                        let (a, b): (String, String) = (
                            l.chars().take(name_len).collect(),
                            l.chars().skip(name_len).collect(),
                        );
                        let x = put_str(
                            buf,
                            body.x + 2,
                            y,
                            &a,
                            Style::new()
                                .fg(theme.text)
                                .bg(panel)
                                .add_modifier(Modifier::BOLD),
                            body,
                        );
                        put_str(
                            buf,
                            x,
                            y,
                            &b,
                            Style::new().fg(theme.text_muted).bg(panel),
                            body,
                        );
                    } else {
                        put_str(
                            buf,
                            body.x + 2,
                            y,
                            l,
                            Style::new().fg(theme.text_muted).bg(panel),
                            body,
                        );
                    }
                    y += 1;
                }
            }
        }
        None
    }
}
