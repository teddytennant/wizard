//! `/docs`: the How-to Guides list and a guide page (spec 6.11). The guides are the user guide
//! Grok Build installs under `~/.grok/docs/user-guide`; with that directory missing the list
//! says so instead of inventing pages.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use tuikit::width::{display_width, truncate};

use super::widgets::{self, Hint};
use super::{modal_frame, modal_rect, page, Modal};
use crate::app::App;
use crate::ui::{put, put_right, st, Layout};

/// File, title and one-line description of each guide, in Grok's order.
pub const GUIDES: &[(&str, &str, &str)] = &[
    (
        "01-getting-started.md",
        "Getting Started",
        "Installation, first launch, and basic interaction",
    ),
    (
        "02-authentication.md",
        "Authentication",
        "Browser login, API keys, OIDC, external auth providers",
    ),
    (
        "03-keyboard-shortcuts.md",
        "Keyboard Shortcuts",
        "Complete reference for all TUI key bindings",
    ),
    (
        "04-slash-commands.md",
        "Slash Commands",
        "All / commands, including goals, research, and workflow management",
    ),
    (
        "05-configuration.md",
        "Configuration",
        "config.toml, pager.toml, environment variables, file locations",
    ),
    (
        "06-theming.md",
        "Theming and Appearance",
        "Themes, color support, pager.toml customization",
    ),
    (
        "07-mcp-servers.md",
        "MCP Servers",
        "Setting up external tool integrations via MCP",
    ),
    (
        "08-skills.md",
        "Skills",
        "Creating and using reusable prompt packages",
    ),
    (
        "09-plugins.md",
        "Plugins and Marketplace",
        "Installing, managing, and creating plugin packages",
    ),
    (
        "10-hooks.md",
        "Hooks",
        "Project lifecycle scripts for pre/post tool-use events",
    ),
    (
        "11-custom-models.md",
        "Custom Models",
        "BYOK, Ollama, OpenAI-compatible endpoints",
    ),
    (
        "12-project-rules.md",
        "Project Rules (AGENTS.md)",
        "Per-directory instructions and precedence rules",
    ),
    (
        "13-memory.md",
        "Memory",
        "Cross-session knowledge persistence and search",
    ),
    (
        "14-headless-mode.md",
        "Headless Mode and Scripting",
        "Non-interactive CLI for automation and CI/CD",
    ),
    (
        "15-agent-mode.md",
        "Agent Mode and IDE Integration",
        "ACP stdio transport, WebSocket relay, SDK integration",
    ),
    (
        "16-subagents.md",
        "Subagents and Personas",
        "Spawning parallel child agents with specialized roles",
    ),
    (
        "17-sessions.md",
        "Session Management",
        "Save, load, resume, rewind, and compact sessions",
    ),
    (
        "18-sandbox.md",
        "Sandbox Mode",
        "OS-level filesystem and network isolation",
    ),
    (
        "19-plan-mode.md",
        "Plan Mode",
        "Structured planning with approval dialogs",
    ),
    (
        "20-background-tasks.md",
        "Background Tasks and Monitoring",
        "Background commands, /loop, monitor, scheduler",
    ),
    (
        "21-terminal-support.md",
        "Terminal Support and Troubleshooting",
        "tmux, Byobu, Zellij, SSH, truecolor, clipboard, and diagnostics",
    ),
    (
        "22-permissions-and-safety.md",
        "Permissions and Safety",
        "Modes, authorization order, allow/ask/deny rules, matching, and hooks",
    ),
    (
        "23-dashboard.md",
        "Agent Dashboard",
        "Live multi-session roster: peek, dispatch, pin, stop, and search",
    ),
    (
        "24-monitoring-usage.md",
        "Monitoring Usage (External OpenTelemetry)",
        "Export usage metrics to a customer OpenTelemetry collector",
    ),
    (
        "25-status-line.md",
        "Status Line",
        "A bottom row of live session context, or the output of your own script",
    ),
    (
        "26-config-reference.md",
        "Configuration Reference",
        "Field list for config.toml, managed_config.toml, and requirements.toml",
    ),
    (
        "27-grok-clone.md",
        "grok clone",
        "Depth-1 Grove clone, --full-history, and safe deepen/switch commands",
    ),
];

/// `$GROK_HOME` or `~/.grok`, then `docs/user-guide`.
pub fn guide_dir() -> PathBuf {
    let home = std::env::var_os("GROK_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".grok")))
        .unwrap_or_default();
    home.join("docs").join("user-guide")
}

fn dir_label() -> String {
    let d = guide_dir().display().to_string();
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && d.starts_with(&h) => format!("~{}", &d[h.len()..]),
        _ => d,
    }
}

#[derive(Clone, Debug)]
pub struct Docs {
    /// Guides whose file is there: index into `GUIDES`.
    pub have: Vec<usize>,
    pub sel: usize,
    pub top: usize,
    pub query: String,
    pub searching: bool,
    /// An open guide: its index and scroll.
    pub page: Option<(usize, usize)>,
    /// What the page shows, read when it opened.
    pub body: String,
}

fn have() -> Vec<usize> {
    let d = guide_dir();
    (0..GUIDES.len())
        .filter(|&i| d.join(GUIDES[i].0).is_file())
        .collect()
}

pub fn open(app: &mut App, arg: &str) {
    app.enter_session();
    let mut d = Docs {
        have: have(),
        sel: 0,
        top: 0,
        query: String::new(),
        searching: false,
        page: None,
        body: String::new(),
    };
    let want = arg.trim().to_lowercase();
    if !want.is_empty() {
        if let Some(i) = d
            .have
            .iter()
            .copied()
            .find(|&i| GUIDES[i].1.to_lowercase().contains(&want))
        {
            open_page(&mut d, i);
        }
    }
    app.modal = Some(Modal::Docs(d));
    app.dirty = true;
}

fn open_page(d: &mut Docs, i: usize) {
    d.body = std::fs::read_to_string(guide_dir().join(GUIDES[i].0)).unwrap_or_default();
    d.page = Some((i, 0));
}

fn rows(d: &Docs) -> Vec<usize> {
    let q = d.query.to_lowercase();
    d.have
        .iter()
        .copied()
        .filter(|&i| {
            q.is_empty()
                || GUIDES[i].1.to_lowercase().contains(&q)
                || GUIDES[i].2.to_lowercase().contains(&q)
        })
        .collect()
}

fn list_rect(lay: &Layout) -> Rect {
    modal_rect(lay.w, lay.h, 0.7, 120, 44, 4)
}

fn page_rect(lay: &Layout) -> Rect {
    modal_rect(lay.w, lay.h, 0.8, 120, 44, 4)
}

fn list_hints() -> Vec<Hint<'static>> {
    vec![("↑/↓", "nav"), ("Enter", "select"), ("Esc", "close")]
}

fn page_hints() -> Vec<Hint<'static>> {
    vec![("↑/↓", "scroll"), ("Esc", "back")]
}

pub fn draw(buf: &mut Buffer, app: &App, lay: &Layout, d: &Docs) {
    let th = &app.theme;
    if let Some((i, scroll)) = d.page {
        let r = page_rect(lay);
        let _ = modal_frame(buf, app, r, GUIDES[i].1);
        page::draw(buf, app, r, &page_hints(), &d.body, scroll);
        return;
    }
    let r = list_rect(lay);
    let inner = modal_frame(buf, app, r, "How-to Guides");
    if inner.height < 8 {
        return;
    }
    let hints = list_hints();
    let b = widgets::body(r, widgets::foot_rows(r, &hints), true);
    widgets::draw_hints(buf, app, r, &hints);
    widgets::draw_search(buf, app, r, b.search_y, &d.query, d.searching);
    widgets::draw_divider(buf, app, r, b.div_y, 0);
    if let Some(t) = b.tip_y {
        let long = format!(
            "Tip · Ask Wizard about the docs ({}), e.g. \"how do I set up MCP?\"",
            dir_label()
        );
        let short = format!("Tip · Ask Wizard about the docs · {}", dir_label());
        let text = if display_width(&long) + 6 <= r.width as usize {
            long
        } else {
            short
        };
        let x = r.x + (r.width.saturating_sub(display_width(&text) as u16)) / 2;
        put(
            buf,
            x,
            t,
            &text,
            st(th.gray_dim).add_modifier(Modifier::ITALIC),
        );
    }
    let rs = rows(d);
    let h = b.list_h as usize;
    if rs.is_empty() {
        let msg = if d.have.is_empty() {
            format!("  No guides found in {}", dir_label())
        } else {
            "  No matches".to_string()
        };
        put(buf, r.x + 3, b.list_y, &msg, st(th.gray_dim));
        return;
    }
    let bar = rs.len() > h;
    let x0 = r.x + 3;
    let x1 = r.right() - 3 - bar as u16;
    for (k, &gi) in rs.iter().skip(d.top).take(h).enumerate() {
        let y = b.list_y + k as u16;
        let selected = d.top + k == d.sel;
        let bg = if selected { th.bg_visual } else { th.bg_base };
        if selected {
            widgets::band(buf, app, x0, x1, y);
        }
        put(buf, x0, y, "◆ ", Style::new().fg(th.gray_dim).bg(bg));
        let mut ls = Style::new().fg(th.text_primary).bg(bg);
        if selected {
            ls = ls.add_modifier(Modifier::BOLD);
        }
        let desc = truncate(GUIDES[gi].2, 36);
        let dw = display_width(&desc);
        let room = ((x1 - 1) as usize).saturating_sub(x0 as usize + 2 + dw + 2);
        put(buf, x0 + 2, y, &truncate(GUIDES[gi].1, room), ls);
        put_right(buf, x1 - 1, y, &desc, Style::new().fg(th.gray).bg(bg));
    }
    if bar {
        widgets::draw_scrollbar(buf, app, r, b.list_y, b.list_h, rs.len(), d.top);
    }
}

pub fn key(app: &mut App, key: KeyEvent) {
    let lay = Layout::compute(app);
    let Some(Modal::Docs(d)) = app.modal.clone() else {
        return;
    };
    let mut d = d;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if let Some((i, mut scroll)) = d.page {
        if key.code == KeyCode::Esc {
            d.page = None;
        } else {
            page::scroll_key(
                app,
                page_rect(&lay),
                &page_hints(),
                &d.body,
                &mut scroll,
                key,
            );
            d.page = Some((i, scroll));
        }
        app.modal = Some(Modal::Docs(d));
        return;
    }
    let r = list_rect(&lay);
    let b = widgets::body(r, widgets::foot_rows(r, &list_hints()), true);
    let h = b.list_h as usize;
    let n = rows(&d).len();
    if d.searching {
        match key.code {
            KeyCode::Esc => {
                d.searching = false;
                d.query.clear();
            }
            KeyCode::Enter => d.searching = false,
            KeyCode::Backspace => {
                d.query.pop();
            }
            KeyCode::Char(c) if !ctrl => d.query.push(c),
            _ => {}
        }
        d.sel = 0;
        d.top = 0;
    } else {
        match key.code {
            KeyCode::Esc => {
                app.modal = None;
                return;
            }
            KeyCode::Down | KeyCode::Char('j') => d.sel = (d.sel + 1).min(n.saturating_sub(1)),
            KeyCode::Up | KeyCode::Char('k') => d.sel = d.sel.saturating_sub(1),
            KeyCode::PageDown => d.sel = (d.sel + h).min(n.saturating_sub(1)),
            KeyCode::PageUp => d.sel = d.sel.saturating_sub(h),
            KeyCode::Home => d.sel = 0,
            KeyCode::End => d.sel = n.saturating_sub(1),
            KeyCode::Char('/') => d.searching = true,
            KeyCode::Enter => {
                if let Some(&i) = rows(&d).get(d.sel) {
                    open_page(&mut d, i);
                }
            }
            _ => {}
        }
    }
    if d.sel < d.top {
        d.top = d.sel;
    } else if d.sel >= d.top + h.max(1) {
        d.top = d.sel + 1 - h.max(1);
    }
    app.modal = Some(Modal::Docs(d));
}
