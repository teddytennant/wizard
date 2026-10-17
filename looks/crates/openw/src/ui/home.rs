// OWNER: home (logo, centered prompt, hints, tip, footer)
//! The home screen: wordmark, centered prompt, hint row, one tip, directory footer. Vertical
//! placement follows opencode's flex column (spec 2.1): spacers above and below split the free
//! height, rounding the top one up.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use tuikit::paint::put_str;
use tuikit::width::{display_width, wrap};

use super::{footer, logo, prompt};
use crate::app::App;
use crate::keys::Action;

/// Wrap a fragment in the markup the tips use for the highlighted spans.
fn hl(s: &str) -> String {
    format!("{{highlight}}{s}{{/highlight}}")
}

/// `Press <keys> <text>`, or nothing when the action has no key.
fn press(keys: &str, text: &str) -> Option<String> {
    (!keys.is_empty()).then(|| format!("Press {} {text}", hl(keys)))
}

/// `/command or key`, as opencode's `commandText` writes it.
fn command_text(cmd: &str, keys: &str) -> String {
    if keys.is_empty() {
        hl(cmd)
    } else {
        format!("{} or {}", hl(cmd), hl(keys))
    }
}

/// The tips from opencode's `tips-view.tsx` that describe something openw does, in source order
/// with the same wording; shortcut text comes from the live keymap. Spans to show in `text`
/// are wrapped in `{highlight}`.
fn tips(app: &App) -> Vec<String> {
    let all = |a: Action| app.keymap.display(&a);
    let one = |a: Action| app.keymap.first(&a);
    let h = hl;
    let mut v: Vec<Option<String>> = vec![
        Some(format!(
            "Type {} followed by a filename to fuzzy search and attach files",
            h("@")
        )),
        Some(format!(
            "Start a message with {} to run shell commands (e.g., {})",
            h("!"),
            h("!ls -la")
        )),
        press(
            &one(Action::AgentCycle),
            "to cycle between Build and Plan agents",
        ),
        Some(format!(
            "Use {} to revert the last message and file changes",
            h("/undo")
        )),
        Some("Drag and drop images or PDFs into the terminal as context".into()),
        press(
            "ctrl+v",
            "to paste images from your clipboard into the prompt",
        ),
        Some(format!(
            "Use {} to compose messages in your external editor",
            command_text("/editor", &one(Action::Editor))
        )),
        Some(format!(
            "Use {} to switch between available AI models",
            command_text("/models", &one(Action::Models))
        )),
        Some(format!(
            "Use {} to switch between {} built-in themes",
            command_text("/themes", &one(Action::Themes)),
            tuikit::Theme::names().len()
        )),
        Some(format!(
            "Use {} to start a fresh conversation session",
            command_text("/new", &one(Action::NewSession))
        )),
        Some(format!(
            "Use {} to list and continue sessions",
            command_text("/sessions", &one(Action::Sessions))
        )),
        Some(format!(
            "Run {} to summarize long sessions near context limits",
            h("/compact")
        )),
        Some(format!(
            "Use {} to save the conversation as Markdown",
            command_text("/export", &one(Action::Export))
        )),
        press(
            &one(Action::CopyLast),
            "to copy the assistant's last message to clipboard",
        ),
        press(
            &one(Action::Palette),
            "to see all available actions and commands",
        ),
        Some(format!(
            "The leader key is {}; combine with other keys for quick actions",
            h(&app.keymap.leader.display())
        )),
        press(
            &one(Action::ModelCycle),
            "to quickly switch between recently used models",
        ),
        press(
            &one(Action::SidebarToggle),
            "in a session to show or hide the sidebar panel",
        ),
        Some(format!(
            "Use {}/{} to navigate through conversation history",
            h(&all(Action::PageUp)),
            h(&all(Action::PageDown))
        )),
        press(
            &all(Action::First),
            "to jump to the beginning of the conversation",
        ),
        press(&all(Action::Last), "to jump to the most recent message"),
        press("shift+return", "to add newlines in your prompt"),
        press("ctrl+c", "when typing to clear the input field"),
        press("escape", "to stop the AI mid-response"),
        Some(format!(
            "Switch to {} agent for suggestions without making changes",
            h("Plan")
        )),
        Some(format!(
            "Use {} to jump to specific messages",
            command_text("/timeline", &one(Action::Timeline))
        )),
        press(
            &one(Action::ToggleConceal),
            "to toggle code block visibility in messages",
        ),
        Some(format!(
            "Use {} to see system status info",
            command_text("/status", &one(Action::Status))
        )),
        Some(format!(
            "Use {} to show the help dialog",
            command_text("/help", "")
        )),
        Some(format!(
            "Use {} to rename the current session",
            h("/rename")
        )),
        press(
            &one(Action::Suspend),
            "to suspend the terminal and return to your shell",
        ),
    ];
    v.retain(Option::is_some);
    v.into_iter().flatten().collect()
}

/// The tip to show, if any.
pub fn tip_text(app: &App) -> Option<String> {
    if app.flags.tips_hidden {
        return None;
    }
    let connected = !app.config.models.is_empty();
    if !connected {
        return Some(format!(
            "Run {} to add an AI provider and start coding",
            hl("/connect")
        ));
    }
    if app.sessions.is_empty() {
        return None;
    }
    let all = tips(app);
    all.get(app.tip % all.len().max(1)).cloned()
}

/// Split `{highlight}..{/highlight}` markup into `(text, highlighted)` parts.
fn parts(tip: &str) -> Vec<(String, bool)> {
    const OPEN: &str = "{highlight}";
    const CLOSE: &str = "{/highlight}";
    let mut out = Vec::new();
    let mut rest = tip;
    while let Some(i) = rest.find(OPEN) {
        if i > 0 {
            out.push((rest[..i].to_string(), false));
        }
        let after = &rest[i + OPEN.len()..];
        let Some(j) = after.find(CLOSE) else {
            out.push((rest[i..].to_string(), false));
            return out;
        };
        out.push((after[..j].to_string(), true));
        rest = &after[j + CLOSE.len()..];
    }
    if !rest.is_empty() {
        out.push((rest.to_string(), false));
    }
    out
}

/// The tip without its markup.
fn plain(tip: &str) -> String {
    parts(tip).into_iter().map(|(s, _)| s).collect()
}

pub struct Layout {
    pub logo: (u16, u16),
    pub prompt: Rect,
    pub hint_y: u16,
    pub tip_y: u16,
    pub tip_lines: Vec<String>,
}

pub fn layout(app: &App) -> Layout {
    let (w, h) = app.size;
    let footer_h = footer::home_footer_height(app, w);
    let pw = 75u16.min(w.saturating_sub(4)).max(1);
    let ph = prompt::box_height(app, pw);
    let e = 1 + ph + 1;
    let tip = tip_text(app);
    let tip_lines: Vec<String> = match &tip {
        Some(t) => {
            // the text node shrinks to what is left of the row after "● Tip ", so the
            // continuation lines hang under the text
            wrap(&plain(t), (pw as usize).saturating_sub(TIP_PREFIX))
        }
        None => Vec::new(),
    };
    let f = 3 + tip_lines.len() as u16;
    let avail = h.saturating_sub(footer_h) as i32;
    let free = avail - (4 + 4 + 1 + e as i32 + f as i32);
    let a = if free > 0 { (free + 1) / 2 } else { 0 } as u16;
    let (b, d) = match free {
        f if f >= 0 => (4u16, 1u16),
        f if f >= -4 => ((4 + f) as u16, 1),
        f => (0, (1 + (f + 4)).max(0) as u16),
    };
    let logo_y = a + b;
    let prompt_y = logo_y + 4 + d + 1;
    let px = 2 + w.saturating_sub(4).saturating_sub(pw).div_ceil(2);
    let lx = 2
        + w.saturating_sub(4)
            .saturating_sub(logo::SLOT_WIDTH)
            .div_ceil(2)
        + (logo::SLOT_WIDTH - logo::WIDTH) / 2;
    Layout {
        logo: (lx, logo_y),
        prompt: Rect::new(px, prompt_y, pw, ph),
        hint_y: prompt_y + ph,
        tip_y: prompt_y + ph + 1 + 3,
        tip_lines,
    }
}

pub fn draw(buf: &mut Buffer, app: &mut App) -> Option<(u16, u16)> {
    let l = layout(app);
    let screen = Rect::new(0, 0, app.size.0, app.size.1);
    logo::draw(buf, l.logo.0, l.logo.1, &app.theme);
    let cursor = prompt::draw_box(buf, l.prompt, app);
    prompt::draw_popup(buf, l.prompt, app);
    footer::home_hints(buf, l.prompt, l.hint_y, app);
    draw_tip(buf, &l, app);
    footer::home_footer(buf, screen, app);
    cursor
}

/// Width of `● Tip `.
const TIP_PREFIX: usize = 6;

fn draw_tip(buf: &mut Buffer, l: &Layout, app: &App) {
    let Some(tip) = tip_text(app) else { return };
    let t = &app.theme;
    let box_x = l.prompt.x;
    let box_w = l.prompt.width as usize;
    // Highlight flag per char of the tip text, then cut along the wrapped rows.
    let mut flat: Vec<bool> = Vec::new();
    for (s, hl) in parts(&tip) {
        flat.extend(s.chars().map(|_| hl));
    }
    let widest = l
        .tip_lines
        .iter()
        .map(|s| display_width(s))
        .max()
        .unwrap_or(0);
    // The whole row (prefix + text) is one box centered in the 75 columns, half cells up.
    let block = TIP_PREFIX + widest;
    let x0 = box_x + box_w.saturating_sub(block).div_ceil(2) as u16;
    put_str(
        buf,
        x0,
        l.tip_y,
        "● Tip",
        Style::new().fg(t.warning).bg(t.background),
        buf.area,
    );
    let mut idx = 0usize;
    for (row, line) in l.tip_lines.iter().enumerate() {
        let n = line.chars().count();
        let y = l.tip_y + row as u16;
        let mut x = x0 + TIP_PREFIX as u16;
        for (i, ch) in line.chars().enumerate() {
            let hl = flat.get(idx + i).copied().unwrap_or(false);
            let fg = if hl { t.text } else { t.text_muted };
            x = put_str(
                buf,
                x,
                y,
                &ch.to_string(),
                Style::new().fg(fg).bg(t.background),
                buf.area,
            );
        }
        idx += n;
        // wrap() drops the space it broke on
        while flat.get(idx).is_some() && tip_char_at(&tip, idx) == Some(' ') {
            idx += 1;
        }
    }
}

/// The `idx`-th char of the tip text without its `[..]` markup.
fn tip_char_at(tip: &str, idx: usize) -> Option<char> {
    plain(tip).chars().nth(idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tip_markup_splits_highlights() {
        assert_eq!(
            parts("Run {highlight}/connect{/highlight} now"),
            vec![
                ("Run ".into(), false),
                ("/connect".into(), true),
                (" now".into(), false)
            ]
        );
    }
}
