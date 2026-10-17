//! `bash`: wizard's `execute`, `git_status` and `git_diff`.

use agent_core::{ToolCall, ToolStatus};
use ratatui::style::Style;
use ratatui::text::Span;
use tuikit::spinner::braille_frame;

use super::rows::{block, block_width, click_hint, inline_block, text_rows, BlockSpec, Rows};
use super::text::{clean_output, collapse, fmt_path, str_in, wrap_text};
use super::{denied, inline};
use crate::ui::session::{Block, RenderCx};

/// The command line as the model asked for it.
fn command(c: &ToolCall) -> String {
    if let Some(s) = str_in(&c.input, &["command", "cmd"]) {
        return s.to_string();
    }
    match c.name.as_str() {
        "git_status" => "git status".into(),
        "git_diff" => {
            let mut s = String::from("git diff");
            if c.input.get("staged").and_then(|v| v.as_bool()) == Some(true) {
                s.push_str(" --staged");
            }
            if let Some(p) = str_in(&c.input, &["path"]) {
                s.push(' ');
                s.push_str(p);
            }
            s
        }
        _ => c.title.clone(),
    }
}

/// Wizard ends a failed command's output with `exit code: N`, which is a finished command
/// whose output deserves the normal block, not an error line.
fn ran_to_exit(out: &str) -> bool {
    out.lines()
        .last()
        .is_some_and(|l| l.starts_with("exit code: "))
}

pub fn bash(c: &ToolCall, cx: &RenderCx) -> Vec<Block> {
    let t = cx.theme;
    let cmd = command(c);
    let running = c.status == ToolStatus::Running;
    let failed = c.status == ToolStatus::Failed;
    // Not started, or refused: the one-line form. A shell that is running or has run always
    // has its block (opencode streams `metadata.output`, empty at first).
    if c.status == ToolStatus::Pending || denied(c) {
        let mut i = inline(c, cx, "$", cmd.clone(), "Writing command…", !cmd.is_empty());
        i.separate = false;
        return vec![inline_block(i, cx)];
    }
    let w = block_width(cx);
    let out = clean_output(c.output.as_deref().unwrap_or(""));
    let message = failed && !ran_to_exit(&out) && !out.is_empty();
    let max_chars = 10 * (cx.width as usize).saturating_sub(6).max(20);
    let expanded = cx.expanded.contains(&c.id);
    let (preview, overflow) = if message {
        (String::new(), false)
    } else {
        collapse(&out, 10, max_chars)
    };
    let shown = if expanded { out.clone() } else { preview };
    let text = Style::new().fg(t.text);

    let mut inner: Vec<Rows> = Vec::new();
    if running {
        let first = braille_frame(cx.anim).to_string();
        let rows: Rows = wrap_text(&cmd, w.saturating_sub(2))
            .into_iter()
            .enumerate()
            .map(|(n, l)| {
                if n == 0 {
                    vec![
                        Span::styled(first.clone(), text),
                        Span::raw(" "),
                        Span::styled(l, text),
                    ]
                } else {
                    vec![Span::raw("  "), Span::styled(l, text)]
                }
            })
            .collect();
        inner.push(rows);
    } else {
        inner.push(text_rows(&format!("$ {cmd}"), w, text));
    }
    if !shown.is_empty() && !message {
        inner.push(text_rows(&shown, w, text));
    }
    if overflow {
        inner.push(click_hint(t, expanded));
    }
    let mut rows: Rows = Vec::new();
    for (n, part) in inner.into_iter().enumerate() {
        if n > 0 {
            rows.push(Vec::new());
        }
        rows.extend(part);
    }
    let wd = str_in(&c.input, &["workdir", "cwd"])
        .map(|p| fmt_path(p, cx.cwd))
        .filter(|p| p != ".");
    let spec = BlockSpec {
        title: wd.map(|w| format!("# Running in {w}")),
        spinner_title: false,
        children: vec![rows],
        error: message.then(|| out.clone()),
        click: overflow.then(|| c.id.clone()),
    };
    vec![block(spec, cx)]
}
